use super::{
    adapter::{
        Accumulator, AgentAdapter, AgentOutcome, ConfiguredAdapter, DispatchRequest, ExitInfo,
    },
    breaker::{self, Breaker, Trip},
    config::{self, AgentSpec, Registry},
    decision::{self, Decision, NextAction},
    git::{self, Preflight},
    planner::{self, PlanRequest, Planner},
    runner::{self, Cancel, RunSpec},
    store,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tauri::Emitter;
use tokio::sync::mpsc;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum TaskStatus {
    Running,
    Waiting,
    Paused,
    Completed,
    Error,
    Aborted,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum NodeStatus {
    Running,
    Succeeded,
    Failed,
    Killed,
    TimedOut,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct TaskRecord {
    pub id: String,
    pub project: String,
    pub goal: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub status: TaskStatus,
    pub reason: Option<String>,
    pub turn: u32,
    pub max_turns: u32,
    pub planner_session: String,
    pub planner_started: bool,
    pub steps: Vec<Step>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Step {
    pub id: String,
    pub turn: u32,
    pub plan_node_id: String,
    pub decision: Option<Decision>,
    pub error: Option<String>,
    pub run: Option<RunRecord>,
    pub reply: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RunRecord {
    pub agent_id: String,
    pub node_id: String,
    pub status: NodeStatus,
    pub started_at: u64,
    pub ended_at: Option<u64>,
    pub exit_code: Option<i32>,
    pub summary: String,
    pub diff_stat: String,
    pub untracked: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum LogStream {
    Stdout,
    Stderr,
    System,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct LogLine {
    pub seq: u64,
    pub stream: LogStream,
    pub text: String,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct LogBatch {
    pub task_id: String,
    pub node_id: String,
    pub lines: Vec<LogLine>,
}

#[derive(Serialize, Clone, Debug)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum OrchestratorEvent {
    Task {
        task: TaskRecord,
    },
    Planning {
        task_id: String,
        step_id: String,
        node_id: String,
        turn: u32,
    },
    Decision {
        task_id: String,
        step: Step,
    },
    NodeStarted {
        task_id: String,
        step_id: String,
        node_id: String,
        agent_id: String,
    },
    NodeFinished {
        task_id: String,
        step_id: String,
        node_id: String,
        run: RunRecord,
    },
    State {
        task_id: String,
        status: TaskStatus,
        turn: u32,
        max_turns: u32,
        reason: Option<String>,
    },
}

pub trait Sink: Send + Sync {
    fn event(&self, event: &OrchestratorEvent);
    fn logs(&self, batch: &LogBatch);
}

pub struct TauriSink(pub tauri::AppHandle);
impl Sink for TauriSink {
    fn event(&self, event: &OrchestratorEvent) {
        let _ = self.0.emit("orchestrator://event", event);
    }
    fn logs(&self, batch: &LogBatch) {
        let _ = self.0.emit("agent://log", batch);
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeResult {
    pub id: String,
    pub resolved: Option<String>,
    pub version: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone)]
pub struct Orchestra {
    inner: Arc<Inner>,
}
struct Inner {
    config_path: PathBuf,
    tasks_dir: PathBuf,
    sink: Arc<dyn Sink>,
    planner: Arc<dyn Planner>,
    core: Mutex<Core>,
}
struct Core {
    registry: Registry,
    tasks: HashMap<String, TaskRecord>,
    live: Option<Live>,
    logs: HashMap<String, VecDeque<LogLine>>,
}
struct Live {
    id: String,
    cancel: Cancel,
    replies: mpsc::UnboundedSender<String>,
    reply_pending: bool,
}

fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value.lock().unwrap_or_else(|e| e.into_inner())
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn active(status: TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Running | TaskStatus::Waiting | TaskStatus::Paused
    )
}
fn checked_text(text: String, field: &str) -> Result<String, String> {
    let text = text.trim();
    if text.is_empty() || text.chars().count() > 20000 {
        return Err(format!("{field}は 1～20000 文字で入力してください"));
    }
    Ok(text.to_owned())
}

impl Orchestra {
    pub fn new(
        config_path: PathBuf,
        tasks_dir: PathBuf,
        sink: Arc<dyn Sink>,
        planner: Arc<dyn Planner>,
    ) -> Self {
        let mut tasks = HashMap::new();
        for mut task in store::load(&tasks_dir) {
            if active(task.status) {
                task.status = TaskStatus::Aborted;
                task.reason = Some("アプリが終了していました".into());
                task.updated_at = now();
                if let Err(error) = store::save(&tasks_dir, &task) {
                    task.status = TaskStatus::Error;
                    task.reason = Some(error);
                }
            }
            tasks.insert(task.id.clone(), task);
        }
        let registry = config::load(&config_path);
        Self {
            inner: Arc::new(Inner {
                config_path,
                tasks_dir,
                sink,
                planner,
                core: Mutex::new(Core {
                    registry,
                    tasks,
                    live: None,
                    logs: HashMap::new(),
                }),
            }),
        }
    }

    pub fn registry(&self) -> Registry {
        lock(&self.inner.core).registry.clone()
    }
    pub fn save_registry(&self, registry: Registry) -> Result<Registry, String> {
        let mut core = lock(&self.inner.core);
        config::save(&self.inner.config_path, &registry)?;
        core.registry = registry.clone();
        Ok(registry)
    }
    pub fn reset_registry(&self) -> Result<Registry, String> {
        self.save_registry(config::defaults())
    }

    pub async fn probe(&self, id: &str) -> ProbeResult {
        let mut result = ProbeResult {
            id: id.into(),
            resolved: None,
            version: None,
            error: None,
        };
        let Some(spec) = self.registry().agents.into_iter().find(|s| s.id == id) else {
            result.error = Some("エージェントが見つかりません".into());
            return result;
        };
        let program = match runner::resolve_program(&spec.program) {
            Ok(path) => path,
            Err(error) => {
                result.error = Some(error);
                return result;
            }
        };
        result.resolved = Some(program.to_string_lossy().into_owned());
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
        let exit = runner::run(
            RunSpec {
                program,
                args: vec!["--version".into()],
                cwd,
                stdin: None,
                prompt_file: None,
                timeout: Duration::from_secs(10),
            },
            Cancel::new(),
            |stream, text| {
                if matches!(stream, LogStream::Stdout | LogStream::Stderr)
                    && result.version.is_none()
                    && !text.trim().is_empty()
                {
                    result.version = Some(text);
                }
            },
        )
        .await;
        match exit {
            Err(error) => result.error = Some(error),
            Ok(exit) if exit.timed_out => {
                result.error = Some("バージョンの確認が時間切れになりました".into())
            }
            Ok(exit) if exit.code != Some(0) => {
                result.error = Some("バージョンを確認できませんでした".into())
            }
            _ => {}
        }
        result
    }

    pub async fn preflight(&self, project: &Path) -> Preflight {
        git::preflight(project).await
    }

    pub async fn start(&self, project: PathBuf, goal: String) -> Result<String, String> {
        let goal = checked_text(goal, "目的")?;
        if !project.is_absolute() || !project.is_dir() {
            return Err("存在するフォルダを絶対パスで指定してください".into());
        }
        self.check_idle()?;
        let preflight = git::preflight(&project).await;
        if !preflight.is_git || !preflight.has_commit {
            return Err(
                "git のリポジトリで、1 つ以上のコミットがあるフォルダを選んでください".into(),
            );
        }
        let registry = self.registry();
        config::validate(&registry)?;
        if !registry.agents.iter().any(|agent| agent.enabled) {
            return Err("有効なエージェントを 1 つ以上登録してください".into());
        }
        runner::resolve_program(&registry.orchestrator.planner_program)?;
        let created_at = now();
        let id = format!("t{created_at}-{}", &uuid::Uuid::new_v4().to_string()[..8]);
        let task = TaskRecord {
            id: id.clone(),
            project: project.to_string_lossy().into_owned(),
            goal,
            created_at,
            updated_at: created_at,
            status: TaskStatus::Running,
            reason: None,
            turn: 0,
            max_turns: registry.orchestrator.max_turns,
            planner_session: uuid::Uuid::new_v4().to_string(),
            planner_started: false,
            steps: Vec::new(),
        };
        let cancel = Cancel::new();
        let (tx, rx) = mpsc::unbounded_channel();
        {
            let mut core = lock(&self.inner.core);
            if core.live.is_some() || core.tasks.values().any(|task| active(task.status)) {
                return Err("別のタスクが動いています。止めてから始めてください".into());
            }
            store::save(&self.inner.tasks_dir, &task)?;
            core.tasks.insert(id.clone(), task.clone());
            core.live = Some(Live {
                id: id.clone(),
                cancel: cancel.clone(),
                replies: tx,
                reply_pending: false,
            });
        }
        self.inner
            .sink
            .event(&OrchestratorEvent::Task { task: task.clone() });
        self.emit_state(&task);
        let this = self.clone();
        let task_id = id.clone();
        tokio::spawn(async move {
            if let Err(error) = this.run_loop(&task_id, registry, cancel, rx).await {
                this.fail(&task_id, error);
            }
        });
        Ok(id)
    }

    fn check_idle(&self) -> Result<(), String> {
        let core = lock(&self.inner.core);
        if core.live.is_some() || core.tasks.values().any(|task| active(task.status)) {
            Err("別のタスクが動いています。止めてから始めてください".into())
        } else {
            Ok(())
        }
    }

    pub fn answer(&self, task_id: &str, text: String) -> Result<(), String> {
        let text = checked_text(text, "返事")?;
        let mut core = lock(&self.inner.core);
        let waiting = core
            .tasks
            .get(task_id)
            .is_some_and(|task| matches!(task.status, TaskStatus::Waiting | TaskStatus::Paused));
        let live = core.live.as_ref().filter(|live| {
            live.id == task_id && !live.reply_pending && !live.cancel.is_cancelled()
        });
        if !waiting || live.is_none() {
            return Err("このタスクは返事を待っていません".into());
        }
        let sender = live.unwrap().replies.clone();
        let task = core.tasks.get_mut(task_id).unwrap();
        // An answered question can be followed at once by a turn-limit pause on the same step.
        if let Some(step) = task.steps.last_mut() {
            step.reply = Some(match step.reply.take() {
                Some(previous) => format!("{previous}\n\n{text}"),
                None => text.clone(),
            });
        }
        task.updated_at = now();
        if let Err(error) = store::save(&self.inner.tasks_dir, task) {
            drop(core);
            self.fail(task_id, error.clone());
            return Err(error);
        }
        let task = task.clone();
        core.live.as_mut().unwrap().reply_pending = true;
        drop(core);
        self.inner.sink.event(&OrchestratorEvent::Task { task });
        sender
            .send(text)
            .map_err(|_| "このタスクは返事を待っていません".to_owned())
    }

    pub fn abort(&self, task_id: &str) -> Result<(), String> {
        let core = lock(&self.inner.core);
        if !core
            .tasks
            .get(task_id)
            .is_some_and(|task| active(task.status))
        {
            return Err("このタスクは動いていません".into());
        }
        let live = core
            .live
            .as_ref()
            .filter(|live| live.id == task_id)
            .ok_or("このタスクは動いていません")?;
        live.cancel.cancel();
        Ok(())
    }

    pub fn tasks(&self, project: &Path) -> Vec<TaskRecord> {
        let mut tasks: Vec<_> = lock(&self.inner.core)
            .tasks
            .values()
            .filter(|task| store::same_project(&task.project, project))
            .cloned()
            .collect();
        tasks.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        tasks.truncate(100);
        tasks
    }
    pub fn task(&self, task_id: &str) -> Option<TaskRecord> {
        lock(&self.inner.core).tasks.get(task_id).cloned()
    }
    pub fn delete(&self, task_id: &str) -> Result<(), String> {
        let mut core = lock(&self.inner.core);
        let task = core.tasks.get(task_id).ok_or("タスクが見つかりません")?;
        if active(task.status) || core.live.as_ref().is_some_and(|live| live.id == task_id) {
            return Err("動いているタスクは削除できません".into());
        }
        store::delete(&self.inner.tasks_dir, task_id)?;
        let task = core.tasks.remove(task_id).unwrap();
        for step in task.steps {
            core.logs.remove(&step.plan_node_id);
            if let Some(run) = step.run {
                core.logs.remove(&run.node_id);
            }
        }
        Ok(())
    }
    pub fn logs(&self, node_id: &str) -> Vec<LogLine> {
        lock(&self.inner.core)
            .logs
            .get(node_id)
            .map(|lines| lines.iter().cloned().collect())
            .unwrap_or_default()
    }
    pub fn snapshot(&self) -> serde_json::Value {
        let core = lock(&self.inner.core);
        serde_json::json!({"live": core.live.as_ref().and_then(|live| core.tasks.get(&live.id))})
    }
    pub fn shutdown(&self) {
        let mut core = lock(&self.inner.core);
        let Some(live) = &core.live else {
            return;
        };
        live.cancel.cancel();
        let id = live.id.clone();
        if let Some(task) = core.tasks.get_mut(&id) {
            task.status = TaskStatus::Aborted;
            task.reason = Some("アプリが終了しました".into());
            task.updated_at = now();
            if let Err(error) = store::save(&self.inner.tasks_dir, task) {
                task.status = TaskStatus::Error;
                task.reason = Some(error);
            }
            self.emit_state(task);
        }
    }

    fn update(&self, id: &str, edit: impl FnOnce(&mut TaskRecord)) -> Result<TaskRecord, String> {
        let mut core = lock(&self.inner.core);
        let task = core.tasks.get_mut(id).ok_or("タスクが見つかりません")?;
        edit(task);
        task.updated_at = now();
        store::save(&self.inner.tasks_dir, task)?;
        Ok(task.clone())
    }
    fn emit_state(&self, task: &TaskRecord) {
        self.inner.sink.event(&OrchestratorEvent::State {
            task_id: task.id.clone(),
            status: task.status,
            turn: task.turn,
            max_turns: task.max_turns,
            reason: task.reason.clone(),
        });
    }
    fn set_status(
        &self,
        id: &str,
        status: TaskStatus,
        reason: Option<String>,
    ) -> Result<(), String> {
        let mut core = lock(&self.inner.core);
        let cancelled = core
            .live
            .as_ref()
            .is_some_and(|live| live.id == id && live.cancel.is_cancelled());
        let task = core.tasks.get_mut(id).ok_or("タスクが見つかりません")?;
        // A cancellation caused by a storage failure must not erase its Error state.
        if task.status == TaskStatus::Error {
            return Ok(());
        }
        task.status = if cancelled {
            TaskStatus::Aborted
        } else {
            status
        };
        task.reason = if cancelled {
            Some("停止しました".into())
        } else {
            reason
        };
        task.updated_at = now();
        store::save(&self.inner.tasks_dir, task)?;
        let task = task.clone();
        if !active(task.status) && core.live.as_ref().is_some_and(|live| live.id == id) {
            core.live = None;
        }
        drop(core);
        self.emit_state(&task);
        Ok(())
    }
    fn fail(&self, id: &str, error: String) {
        let mut core = lock(&self.inner.core);
        if let Some(live) = core.live.as_ref().filter(|live| live.id == id) {
            live.cancel.cancel();
        }
        if let Some(task) = core.tasks.get_mut(id) {
            task.status = TaskStatus::Error;
            task.reason = Some(error);
            task.updated_at = now();
            let _ = store::save(&self.inner.tasks_dir, task);
            self.emit_state(task);
        }
        if core.live.as_ref().is_some_and(|live| live.id == id) {
            core.live = None;
        }
    }

    async fn wait_reply(
        &self,
        id: &str,
        cancel: &Cancel,
        replies: &mut mpsc::UnboundedReceiver<String>,
        breaker: &mut Breaker,
        cause: Option<Trip>,
        pending: Option<&str>,
        settings: &config::OrchestratorSettings,
    ) -> Result<Option<String>, String> {
        let text = tokio::select! {
            biased;
            _ = cancel.cancelled() => { self.set_status(id, TaskStatus::Aborted, Some("停止しました".into()))?; return Ok(None); }
            text = replies.recv() => text.ok_or("返事の受信が終了しました")?,
        };
        match cause {
            Some(Trip::MaxTurns { .. }) => breaker.extend(settings.max_turns),
            Some(Trip::SameError { .. }) => breaker.clear_errors(),
            None => {}
        }
        self.update(id, |task| task.max_turns = breaker.max_turns())?;
        self.set_status(id, TaskStatus::Running, None)?;
        {
            let mut core = lock(&self.inner.core);
            if let Some(live) = core.live.as_mut().filter(|live| live.id == id) {
                live.reply_pending = false;
            }
        }
        if cancel.is_cancelled() {
            return Ok(None);
        }
        Ok(Some(planner::reply_prompt(pending, &text)))
    }

    async fn run_loop(
        &self,
        id: &str,
        registry: Registry,
        cancel: Cancel,
        mut replies: mpsc::UnboundedReceiver<String>,
    ) -> Result<(), String> {
        let settings = &registry.orchestrator;
        let agents: Vec<&AgentSpec> = registry
            .agents
            .iter()
            .filter(|agent| agent.enabled)
            .collect();
        let enabled: Vec<&str> = agents.iter().map(|agent| agent.id.as_str()).collect();
        let initial = self.task(id).ok_or("タスクが見つかりません")?;
        let project = PathBuf::from(&initial.project);
        let mut breaker = Breaker::new(settings.max_turns, settings.same_error_limit);
        let mut pending =
            planner::first_prompt(&initial.goal, &project, &agents, settings.max_turns);
        loop {
            if cancel.is_cancelled() {
                return self.set_status(id, TaskStatus::Aborted, Some("停止しました".into()));
            }
            let turn = match breaker.begin_turn() {
                Ok(turn) => turn,
                Err(trip) => {
                    self.set_status(id, TaskStatus::Paused, Some(breaker::trip_reason(&trip)))?;
                    let Some(next) = self
                        .wait_reply(
                            id,
                            &cancel,
                            &mut replies,
                            &mut breaker,
                            Some(trip),
                            Some(&pending),
                            settings,
                        )
                        .await?
                    else {
                        return Ok(());
                    };
                    pending = next;
                    continue;
                }
            };
            let task = self.update(id, |task| {
                let step_id = format!("{id}-s{}", task.steps.len() + 1);
                task.turn = breaker.turns();
                task.steps.push(Step {
                    plan_node_id: format!("{step_id}:plan"),
                    id: step_id,
                    turn,
                    decision: None,
                    error: None,
                    run: None,
                    reply: None,
                });
            })?;
            let step = task.steps.last().unwrap().clone();
            self.inner.sink.event(&OrchestratorEvent::Planning {
                task_id: id.into(),
                step_id: step.id.clone(),
                node_id: step.plan_node_id.clone(),
                turn,
            });
            let logger = NodeLog::new(self.inner.clone(), id, &step.plan_node_id);
            let log = |stream, line| logger.line(stream, line);
            let output = self
                .inner
                .planner
                .decide(
                    PlanRequest {
                        task_id: id.into(),
                        node_id: step.plan_node_id.clone(),
                        project: project.clone(),
                        session: task.planner_session,
                        resume: task.planner_started,
                        prompt: pending.clone(),
                        settings: settings.clone(),
                        cancel: cancel.clone(),
                    },
                    &log,
                )
                .await;
            drop(logger);
            if cancel.is_cancelled() {
                return self.set_status(id, TaskStatus::Aborted, Some("停止しました".into()));
            }
            let raw = match output {
                Ok(output) => {
                    // The session exists once the planner ran, even if its reply is unusable.
                    if !task.planner_started {
                        self.update(id, |task| task.planner_started = true)?;
                    }
                    decision::parse_planner_output(&output)
                        .and_then(|raw| decision::validate(raw, &enabled))
                }
                Err(error) => {
                    // A failed first call may still have created the session, and -s cannot reuse an ID.
                    if !task.planner_started {
                        self.update(id, |task| {
                            task.planner_session = uuid::Uuid::new_v4().to_string()
                        })?;
                    }
                    self.plan_error(id, &error)?;
                    self.set_status(id, TaskStatus::Paused, Some(error))?;
                    let Some(next) = self
                        .wait_reply(
                            id,
                            &cancel,
                            &mut replies,
                            &mut breaker,
                            None,
                            Some(&pending),
                            settings,
                        )
                        .await?
                    else {
                        return Ok(());
                    };
                    pending = next;
                    continue;
                }
            };
            let decision = match raw {
                Ok(decision) => decision,
                Err(error) => {
                    self.plan_error(id, &error)?;
                    let trip =
                        breaker.record_failure(&breaker::error_signature("planner", None, &error));
                    pending = planner::repair_prompt(&error);
                    if let Some(trip) = trip {
                        self.set_status(id, TaskStatus::Paused, Some(breaker::trip_reason(&trip)))?;
                        let Some(next) = self
                            .wait_reply(
                                id,
                                &cancel,
                                &mut replies,
                                &mut breaker,
                                Some(trip),
                                Some(&pending),
                                settings,
                            )
                            .await?
                        else {
                            return Ok(());
                        };
                        pending = next;
                    }
                    continue;
                }
            };
            let task = self.update(id, |task| {
                task.steps.last_mut().unwrap().decision = Some(decision.clone())
            })?;
            self.inner.sink.event(&OrchestratorEvent::Decision {
                task_id: id.into(),
                step: task.steps.last().unwrap().clone(),
            });
            match decision.next_action {
                NextAction::Complete => return self.set_status(id, TaskStatus::Completed, None),
                NextAction::AskHuman => {
                    self.set_status(id, TaskStatus::Waiting, None)?;
                    let Some(next) = self
                        .wait_reply(
                            id,
                            &cancel,
                            &mut replies,
                            &mut breaker,
                            None,
                            None,
                            settings,
                        )
                        .await?
                    else {
                        return Ok(());
                    };
                    pending = next;
                }
                NextAction::Dispatch => {
                    let agent = agents
                        .iter()
                        .find(|agent| Some(&agent.id) == decision.target_agent.as_ref())
                        .ok_or("エージェントが見つかりません")?;
                    let (outcome, exit_code, diff) = self
                        .dispatch(
                            id,
                            &step,
                            &project,
                            agent,
                            decision.converted_prompt.as_deref().unwrap(),
                            settings,
                            cancel.clone(),
                        )
                        .await?;
                    if cancel.is_cancelled() || outcome.status == NodeStatus::Killed {
                        return self.set_status(
                            id,
                            TaskStatus::Aborted,
                            Some("停止しました".into()),
                        );
                    }
                    pending = planner::result_prompt(
                        task.steps.len(),
                        &agent.id,
                        &outcome,
                        exit_code,
                        &diff,
                        turn,
                        breaker.max_turns(),
                    );
                    let trip = if outcome.status == NodeStatus::Succeeded {
                        breaker.record_success();
                        None
                    } else {
                        breaker.record_failure(
                            outcome
                                .error_signature
                                .as_deref()
                                .unwrap_or("実行に失敗しました"),
                        )
                    };
                    if let Some(trip) = trip {
                        self.set_status(id, TaskStatus::Paused, Some(breaker::trip_reason(&trip)))?;
                        let Some(next) = self
                            .wait_reply(
                                id,
                                &cancel,
                                &mut replies,
                                &mut breaker,
                                Some(trip),
                                Some(&pending),
                                settings,
                            )
                            .await?
                        else {
                            return Ok(());
                        };
                        pending = next;
                    }
                }
            }
        }
    }

    fn plan_error(&self, id: &str, error: &str) -> Result<(), String> {
        let task = self.update(id, |task| {
            task.steps.last_mut().unwrap().error = Some(error.into())
        })?;
        self.inner.sink.event(&OrchestratorEvent::Task { task });
        Ok(())
    }

    async fn dispatch(
        &self,
        id: &str,
        step: &Step,
        project: &Path,
        spec: &AgentSpec,
        prompt: &str,
        settings: &config::OrchestratorSettings,
        cancel: Cancel,
    ) -> Result<(AgentOutcome, Option<i32>, git::StepDiff), String> {
        let adapter = ConfiguredAdapter::new(spec.clone(), settings.log_tail_bytes);
        let node_id = format!("{}:run", step.id);
        let prompt_file = std::env::temp_dir()
            .join("grok-build-desktop")
            .join(id)
            .join(format!("{}.txt", node_id.replace(':', "_")));
        let launch = adapter.launch(&DispatchRequest {
            cwd: project,
            prompt,
            prompt_file: &prompt_file,
        });
        let snapshot = git::snapshot(project).await;
        let mut run = RunRecord {
            agent_id: adapter.spec().id.clone(),
            node_id: node_id.clone(),
            status: NodeStatus::Running,
            started_at: now(),
            ended_at: None,
            exit_code: None,
            summary: String::new(),
            diff_stat: String::new(),
            untracked: Vec::new(),
        };
        self.update(id, |task| {
            task.steps.last_mut().unwrap().run = Some(run.clone())
        })?;
        self.inner.sink.event(&OrchestratorEvent::NodeStarted {
            task_id: id.into(),
            step_id: step.id.clone(),
            node_id: node_id.clone(),
            agent_id: spec.id.clone(),
        });
        let logger = NodeLog::new(self.inner.clone(), id, &node_id);
        // Expand only the log copy with a marker so prompts never enter command logs.
        let safe_args = config::expand(&spec.args, Some("<prompt>"), Some(&prompt_file), project);
        logger.line(
            LogStream::System,
            format!("{} {}", spec.program, safe_args.join(" ")),
        );
        let mut acc = Accumulator::default();
        let execution = match launch {
            Ok(plan) => {
                runner::run(
                    RunSpec {
                        program: plan.program,
                        args: plan.args,
                        cwd: plan.cwd,
                        stdin: plan.stdin,
                        prompt_file: plan.prompt_file,
                        timeout: Duration::from_secs(settings.dispatch_timeout_secs),
                    },
                    cancel,
                    |stream, line| {
                        adapter.observe(stream, &line, &mut acc);
                        logger.line(stream, line);
                    },
                )
                .await
            }
            Err(error) => Err(error),
        };
        let (outcome, exit) = match execution {
            Ok(exit) => (adapter.outcome(acc, &exit), exit),
            Err(error) => {
                logger.line(LogStream::System, error.clone());
                (
                    AgentOutcome {
                        status: NodeStatus::Failed,
                        summary: error.clone(),
                        stderr_tail: String::new(),
                        error_signature: Some(breaker::error_signature(&spec.id, None, &error)),
                    },
                    ExitInfo {
                        code: None,
                        killed: false,
                        timed_out: false,
                    },
                )
            }
        };
        logger.line(
            LogStream::System,
            if exit.timed_out {
                "時間切れで止めました".into()
            } else if exit.killed {
                "強制終了しました".into()
            } else {
                format!(
                    "終了コード {}",
                    exit.code
                        .map(|code| code.to_string())
                        .unwrap_or_else(|| "不明".into())
                )
            },
        );
        drop(logger);
        let diff = git::diff_since(project, &snapshot, settings.diff_limit_bytes).await;
        run.status = outcome.status;
        run.ended_at = Some(now());
        run.exit_code = exit.code;
        run.summary = outcome.summary.clone();
        run.diff_stat = diff.stat.clone();
        run.untracked = diff.untracked.clone();
        self.update(id, |task| {
            task.steps.last_mut().unwrap().run = Some(run.clone())
        })?;
        self.inner.sink.event(&OrchestratorEvent::NodeFinished {
            task_id: id.into(),
            step_id: step.id.clone(),
            node_id,
            run,
        });
        Ok((outcome, exit.code, diff))
    }
}

struct LogBuffer {
    inner: Arc<Inner>,
    task_id: String,
    node_id: String,
    pending: Mutex<(u64, Vec<LogLine>)>,
}
impl LogBuffer {
    fn flush(&self) {
        let mut pending = lock(&self.pending);
        if !pending.1.is_empty() {
            self.inner.sink.logs(&LogBatch {
                task_id: self.task_id.clone(),
                node_id: self.node_id.clone(),
                lines: std::mem::take(&mut pending.1),
            });
        }
    }
}
struct NodeLog {
    buffer: Arc<LogBuffer>,
    stop: Cancel,
}
impl NodeLog {
    fn new(inner: Arc<Inner>, task_id: &str, node_id: &str) -> Self {
        let buffer = Arc::new(LogBuffer {
            inner,
            task_id: task_id.into(),
            node_id: node_id.into(),
            pending: Mutex::new((0, Vec::new())),
        });
        let weak = Arc::downgrade(&buffer);
        let stop = Cancel::new();
        let stopped = stop.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(50));
            interval.tick().await;
            loop {
                tokio::select! {
                    biased;
                    _ = stopped.cancelled() => break,
                    _ = interval.tick() => {
                        let Some(buffer) = weak.upgrade() else { break; };
                        buffer.flush();
                    }
                }
            }
        });
        Self { buffer, stop }
    }
    fn line(&self, stream: LogStream, text: String) {
        let mut pending = lock(&self.buffer.pending);
        pending.0 += 1;
        let line = LogLine {
            seq: pending.0,
            stream,
            text,
        };
        {
            let mut core = lock(&self.buffer.inner.core);
            let lines = core.logs.entry(self.buffer.node_id.clone()).or_default();
            lines.push_back(line.clone());
            while lines.len() > 2000 {
                lines.pop_front();
            }
        }
        pending.1.push(line);
        if pending.1.len() >= 64 {
            self.buffer.inner.sink.logs(&LogBatch {
                task_id: self.buffer.task_id.clone(),
                node_id: self.buffer.node_id.clone(),
                lines: std::mem::take(&mut pending.1),
            });
        }
    }
}
impl Drop for NodeLog {
    fn drop(&mut self) {
        self.stop.cancel();
        self.buffer.flush();
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::orchestra::{
        config::{AgentKind, OutputFormat, PromptVia},
        planner::PlanFuture,
        runner::no_window,
    };
    use serde_json::{Value, json};
    use std::{process::Stdio, time::Instant};

    const SUCCESS: &str = "$null = [Console]::In.ReadToEnd(); Add-Content -Path tracked.txt -Value 'changed by agent'; Set-Content -Path created.txt -Value 'new'; Write-Output 'agent done'";
    const FAIL: &str = "$null = [Console]::In.ReadToEnd(); [Console]::Error.WriteLine('boom at C:\\work\\x.rs line 12'); exit 3";
    const SLEEP: &str = "Start-Sleep 60";

    struct Dir(PathBuf);
    impl Dir {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("gbd-engine-{name}-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn git(dir: &Path, args: &[&str]) {
        let mut command = std::process::Command::new("git");
        command
            .arg("-C")
            .arg(dir)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        no_window(&mut command);
        assert!(command.status().unwrap().success(), "git {args:?}");
    }

    struct Call {
        prompt: String,
        resume: bool,
        session: String,
    }

    /// Replies in order; with none left it behaves like a planner that runs until stopped.
    struct MockPlanner {
        replies: Mutex<VecDeque<Result<String, String>>>,
        calls: Mutex<Vec<Call>>,
    }
    impl MockPlanner {
        fn new(replies: Vec<Result<String, String>>) -> Arc<Self> {
            Arc::new(Self {
                replies: Mutex::new(replies.into()),
                calls: Mutex::default(),
            })
        }
        fn prompts(&self) -> Vec<String> {
            lock(&self.calls)
                .iter()
                .map(|call| call.prompt.clone())
                .collect()
        }
    }
    impl Planner for MockPlanner {
        fn decide<'a>(
            &'a self,
            req: PlanRequest,
            log: &'a (dyn Fn(LogStream, String) + Send + Sync),
        ) -> PlanFuture<'a> {
            Box::pin(async move {
                lock(&self.calls).push(Call {
                    prompt: req.prompt.clone(),
                    resume: req.resume,
                    session: req.session.clone(),
                });
                log(LogStream::Stdout, "mock planner".into());
                let reply = lock(&self.replies).pop_front();
                match reply {
                    Some(reply) => reply,
                    None => {
                        req.cancel.cancelled().await;
                        Err("司令塔を強制終了しました".into())
                    }
                }
            })
        }
    }

    #[derive(Default)]
    struct RecordingSink {
        events: Mutex<Vec<Value>>,
    }
    impl Sink for RecordingSink {
        fn event(&self, event: &OrchestratorEvent) {
            lock(&self.events).push(serde_json::to_value(event).unwrap());
        }
        fn logs(&self, _batch: &LogBatch) {}
    }
    impl RecordingSink {
        fn types(&self) -> Vec<String> {
            lock(&self.events)
                .iter()
                .map(|event| event["type"].as_str().unwrap().to_owned())
                .collect()
        }
        fn states(&self) -> Vec<String> {
            lock(&self.events)
                .iter()
                .filter(|event| event["type"] == "state")
                .map(|event| event["status"].as_str().unwrap().to_owned())
                .collect()
        }
    }

    fn decision(action: &str) -> Result<String, String> {
        let mut value = json!({
            "next_action": action, "reasoning": "理由", "target_agent": null,
            "converted_prompt": null, "step_title": null, "question": null, "summary": null,
        });
        match action {
            "dispatch" => {
                value["target_agent"] = json!("fake");
                value["converted_prompt"] = json!("tracked.txt を変更して");
                value["step_title"] = json!("実装");
            }
            "ask_human" => value["question"] = json!("どちらにしますか"),
            _ => value["summary"] = json!("終わりました"),
        }
        Ok(value.to_string())
    }

    struct Fixture {
        orch: Orchestra,
        sink: Arc<RecordingSink>,
        planner: Arc<MockPlanner>,
        project: Dir,
        _data: Dir,
    }

    fn fixture(
        script: &str,
        replies: Vec<Result<String, String>>,
        edit: impl FnOnce(&mut Registry),
    ) -> Fixture {
        let project = Dir::new("project");
        git(&project.0, &["init", "--quiet"]);
        std::fs::write(project.0.join("tracked.txt"), "original\n").unwrap();
        git(&project.0, &["add", "."]);
        git(
            &project.0,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "initial",
            ],
        );
        let data = Dir::new("data");
        let script_path = data.0.join("agent.ps1");
        std::fs::write(&script_path, script).unwrap();
        let mut registry = config::defaults();
        registry.orchestrator.planner_program = "powershell".into();
        registry.agents = vec![AgentSpec {
            id: "fake".into(),
            name: "Fake".into(),
            kind: AgentKind::Custom,
            enabled: true,
            program: "powershell".into(),
            args: [
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ]
            .into_iter()
            .map(String::from)
            .chain([script_path.to_string_lossy().into_owned()])
            .collect(),
            prompt_via: PromptVia::Stdin,
            output: OutputFormat::Text,
            role: "テスト用".into(),
            allow_dangerous: false,
        }];
        edit(&mut registry);
        let sink = Arc::new(RecordingSink::default());
        let planner = MockPlanner::new(replies);
        let orch = Orchestra::new(
            data.0.join("agents.json"),
            data.0.join("tasks"),
            sink.clone(),
            planner.clone(),
        );
        orch.save_registry(registry).unwrap();
        Fixture {
            orch,
            sink,
            planner,
            project,
            _data: data,
        }
    }

    async fn until(what: &str, mut check: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !check() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    impl Fixture {
        async fn start(&self, goal: &str) -> String {
            self.orch
                .start(self.project.0.clone(), goal.into())
                .await
                .unwrap()
        }
        /// Waits for the nth state event with this status, which is sent after the record is saved.
        async fn reach(&self, id: &str, status: &str, nth: usize) -> TaskRecord {
            until(status, || {
                self.sink
                    .states()
                    .iter()
                    .filter(|state| *state == status)
                    .count()
                    >= nth
            })
            .await;
            self.orch.task(id).unwrap()
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_then_complete_reports_the_step_back() {
        let f = fixture(
            SUCCESS,
            vec![decision("dispatch"), decision("complete")],
            |_| {},
        );
        let id = f.start("tracked.txt を変える").await;
        let task = f.reach(&id, "completed", 1).await;
        assert_eq!(
            f.sink.types(),
            [
                "task",
                "state",
                "planning",
                "decision",
                "node-started",
                "node-finished",
                "planning",
                "decision",
                "state",
            ]
        );
        let run = task.steps[0].run.as_ref().unwrap();
        assert_eq!(run.status, NodeStatus::Succeeded);
        assert_eq!(run.exit_code, Some(0));
        assert!(run.diff_stat.contains("tracked.txt"), "{}", run.diff_stat);
        assert_eq!(run.untracked, ["created.txt"]);
        assert_eq!(task.turn, 2);
        assert_eq!(
            task.steps[1].decision.as_ref().unwrap().summary.as_deref(),
            Some("終わりました")
        );
        {
            let calls = lock(&f.planner.calls);
            assert_eq!(calls.len(), 2);
            assert!(!calls[0].resume && calls[1].resume);
            assert_eq!(calls[0].session, task.planner_session);
            assert_eq!(calls[1].session, task.planner_session);
            assert!(calls[0].prompt.contains("tracked.txt を変える"));
            assert!(calls[0].prompt.contains("- fake (Fake): テスト用"));
            for expected in [
                "status: succeeded, exit code: 0",
                "agent done",
                "tracked.txt",
                "+changed by agent",
                "created.txt",
            ] {
                assert!(calls[1].prompt.contains(expected), "{expected}");
            }
        }
        assert!(
            f.orch
                .logs(&run.node_id)
                .iter()
                .any(|line| line.text == "agent done")
        );
        assert!(!f.orch.logs(&task.steps[0].plan_node_id).is_empty());
        assert!(f.orch.check_idle().is_ok());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ask_human_waits_for_the_answer() {
        let f = fixture(
            SUCCESS,
            vec![decision("ask_human"), decision("complete")],
            |_| {},
        );
        let id = f.start("質問して").await;
        let task = f.reach(&id, "waiting", 1).await;
        assert_eq!(task.status, TaskStatus::Waiting);
        assert!(f.orch.answer(&id, "   ".into()).is_err());
        f.orch.answer(&id, "  A にします  ".into()).unwrap();
        assert!(f.orch.answer(&id, "二重".into()).is_err());
        let task = f.reach(&id, "completed", 1).await;
        assert_eq!(task.steps[0].reply.as_deref(), Some("A にします"));
        assert_eq!(
            f.planner.prompts()[1],
            "# Message from the user\nA にします\n\nDecide the next step."
        );
        assert_eq!(
            f.sink.states(),
            ["running", "waiting", "running", "completed"]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn same_failure_twice_pauses_until_stopped() {
        let f = fixture(
            FAIL,
            vec![decision("dispatch"), decision("dispatch")],
            |_| {},
        );
        let id = f.start("失敗させる").await;
        let task = f.reach(&id, "paused", 1).await;
        assert_eq!(task.reason.as_deref(), Some("同じ失敗が 2 回続きました"));
        assert_eq!(task.steps.len(), 2);
        for step in &task.steps {
            let run = step.run.as_ref().unwrap();
            assert_eq!(run.status, NodeStatus::Failed);
            assert_eq!(run.exit_code, Some(3));
        }
        let prompts = f.planner.prompts();
        assert!(prompts[1].contains("status: failed, exit code: 3"));
        assert!(prompts[1].contains("boom at"));
        f.orch.abort(&id).unwrap();
        let task = f.reach(&id, "aborted", 1).await;
        assert_eq!(task.reason.as_deref(), Some("停止しました"));
        assert!(f.orch.abort(&id).is_err());
        assert_eq!(f.planner.prompts().len(), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn turn_limit_pauses_and_an_answer_extends_it() {
        let replies = vec![
            decision("dispatch"),
            decision("dispatch"),
            decision("complete"),
        ];
        let f = fixture(SUCCESS, replies, |registry| {
            registry.orchestrator.max_turns = 2
        });
        let id = f.start("2 回で止まる").await;
        let task = f.reach(&id, "paused", 1).await;
        assert_eq!(
            task.reason.as_deref(),
            Some("ターンの上限（2 回）に達しました")
        );
        assert_eq!((task.turn, task.max_turns, task.steps.len()), (2, 2, 2));
        assert_eq!(f.planner.prompts().len(), 2);
        f.orch.answer(&id, "続けて".into()).unwrap();
        let task = f.reach(&id, "completed", 1).await;
        assert_eq!((task.turn, task.max_turns), (3, 4));
        assert_eq!(task.steps[1].reply.as_deref(), Some("続けて"));
        let prompt = &f.planner.prompts()[2];
        assert!(prompt.starts_with("# Result of step 2 (fake)"), "{prompt}");
        assert!(prompt.ends_with("# Message from the user\n続けて\n\nDecide the next step."));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stopping_kills_a_running_agent() {
        let f = fixture(SLEEP, vec![decision("dispatch")], |_| {});
        let id = f.start("止める").await;
        until("node-started", || {
            f.sink.types().iter().any(|kind| kind == "node-started")
        })
        .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let stopped = Instant::now();
        f.orch.abort(&id).unwrap();
        let task = f.reach(&id, "aborted", 1).await;
        assert!(stopped.elapsed() < Duration::from_secs(10));
        assert_eq!(
            task.steps[0].run.as_ref().unwrap().status,
            NodeStatus::Killed
        );
        assert_eq!(task.reason.as_deref(), Some("停止しました"));
        assert!(f.orch.check_idle().is_ok());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unreadable_reply_twice_pauses() {
        let replies = vec![Ok("not json".into()), Ok("still not json".into())];
        let f = fixture(SUCCESS, replies, |_| {});
        let id = f.start("壊れた返事").await;
        let task = f.reach(&id, "paused", 1).await;
        assert_eq!(task.reason.as_deref(), Some("同じ失敗が 2 回続きました"));
        assert!(
            task.steps
                .iter()
                .all(|step| step.decision.is_none() && step.error.is_some())
        );
        assert!(task.planner_started);
        {
            let calls = lock(&f.planner.calls);
            assert!(calls[1].resume);
            assert!(
                calls[1]
                    .prompt
                    .starts_with("Your previous reply could not be used")
            );
        }
        f.orch.abort(&id).unwrap();
        f.reach(&id, "aborted", 1).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn one_unreadable_reply_is_repaired() {
        let f = fixture(
            SUCCESS,
            vec![Ok("not json".into()), decision("complete")],
            |_| {},
        );
        let id = f.start("一度だけ壊れる").await;
        let task = f.reach(&id, "completed", 1).await;
        assert_eq!(task.steps.len(), 2);
        assert!(task.steps[0].error.is_some() && task.steps[0].decision.is_none());
        assert!(task.steps[1].decision.is_some());
        assert!(f.planner.prompts()[1].starts_with("Your previous reply could not be used"));
        assert_eq!(f.sink.states(), ["running", "completed"]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn planner_failure_pauses_and_retries_in_a_new_session() {
        let replies = vec![
            Err("Grok にログインしていません".into()),
            decision("complete"),
        ];
        let f = fixture(SUCCESS, replies, |_| {});
        let id = f.start("ログイン前").await;
        let task = f.reach(&id, "paused", 1).await;
        assert_eq!(task.reason.as_deref(), Some("Grok にログインしていません"));
        assert!(task.steps[0].error.is_some());
        assert!(!task.planner_started);
        let first = lock(&f.planner.calls)[0].session.clone();
        assert_ne!(task.planner_session, first);
        f.orch.answer(&id, "ログインしました".into()).unwrap();
        let task = f.reach(&id, "completed", 1).await;
        let calls = lock(&f.planner.calls);
        assert!(!calls[1].resume);
        assert_eq!(calls[1].session, task.planner_session);
        assert!(calls[1].prompt.starts_with("# Goal\nログイン前"));
        assert!(
            calls[1]
                .prompt
                .ends_with("ログインしました\n\nDecide the next step.")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn answer_then_turn_limit_keeps_both_replies() {
        let replies = vec![decision("ask_human"), decision("complete")];
        let f = fixture(SUCCESS, replies, |registry| {
            registry.orchestrator.max_turns = 1
        });
        let id = f.start("一度だけ聞く").await;
        f.reach(&id, "waiting", 1).await;
        f.orch.answer(&id, "A".into()).unwrap();
        let task = f.reach(&id, "paused", 1).await;
        assert_eq!(
            task.reason.as_deref(),
            Some("ターンの上限（1 回）に達しました")
        );
        f.orch.answer(&id, "B".into()).unwrap();
        let task = f.reach(&id, "completed", 1).await;
        assert_eq!(task.steps[0].reply.as_deref(), Some("A\n\nB"));
        assert_eq!(task.max_turns, 2);
        assert_eq!(
            f.planner.prompts()[1],
            planner::reply_prompt(Some(&planner::reply_prompt(None, "A")), "B")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_refuses_bad_input_and_a_second_task() {
        let f = fixture(SUCCESS, vec![], |_| {});
        let plain = Dir::new("plain");
        let refused = f.orch.start(plain.0.clone(), "目的".into()).await;
        assert!(refused.unwrap_err().contains("git"));
        assert!(
            f.orch
                .start(f.project.0.clone(), "  ".into())
                .await
                .is_err()
        );
        assert!(
            f.orch
                .start("relative".into(), "目的".into())
                .await
                .is_err()
        );
        let id = f.start("動き続ける").await;
        let second = f.orch.start(f.project.0.clone(), "二つ目".into()).await;
        assert!(second.unwrap_err().contains("別のタスク"));
        assert!(f.orch.delete(&id).is_err());
        assert_eq!(f.orch.snapshot()["live"]["id"], id.as_str());
        until("planning", || {
            f.sink.types().iter().any(|kind| kind == "planning")
        })
        .await;
        f.orch.abort(&id).unwrap();
        f.reach(&id, "aborted", 1).await;
        assert_eq!(f.orch.snapshot()["live"], Value::Null);
        assert_eq!(f.orch.tasks(&f.project.0).len(), 1);
        f.orch.delete(&id).unwrap();
        assert!(f.orch.task(&id).is_none());
        assert!(f.orch.tasks(&f.project.0).is_empty());
    }

    #[test]
    fn restart_marks_unfinished_tasks_aborted() {
        let data = Dir::new("restart");
        let tasks_dir = data.0.join("tasks");
        let mut record = TaskRecord {
            id: "t1-running".into(),
            project: "C:/project".into(),
            goal: "途中".into(),
            created_at: 1,
            updated_at: 1,
            status: TaskStatus::Running,
            reason: None,
            turn: 1,
            max_turns: 10,
            planner_session: "session".into(),
            planner_started: true,
            steps: Vec::new(),
        };
        store::save(&tasks_dir, &record).unwrap();
        record.id = "t2-paused".into();
        record.status = TaskStatus::Paused;
        store::save(&tasks_dir, &record).unwrap();
        record.id = "t3-done".into();
        record.status = TaskStatus::Completed;
        store::save(&tasks_dir, &record).unwrap();
        let orch = Orchestra::new(
            data.0.join("agents.json"),
            tasks_dir.clone(),
            Arc::new(RecordingSink::default()),
            MockPlanner::new(vec![]),
        );
        for id in ["t1-running", "t2-paused"] {
            let task = orch.task(id).unwrap();
            assert_eq!(task.status, TaskStatus::Aborted);
            assert_eq!(task.reason.as_deref(), Some("アプリが終了していました"));
        }
        assert_eq!(orch.task("t3-done").unwrap().status, TaskStatus::Completed);
        let saved = store::load(&tasks_dir);
        assert_eq!(
            saved
                .iter()
                .filter(|task| task.status == TaskStatus::Aborted)
                .count(),
            2
        );
        assert!(orch.check_idle().is_ok());
        assert_eq!(orch.registry(), config::defaults());
    }
}
