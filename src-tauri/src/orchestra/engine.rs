use super::{
    adapter::{Accumulator, AgentAdapter, ConfiguredAdapter, DispatchRequest},
    breaker,
    config::{self, AgentKind, AgentSpec, Registry},
    git::{self, ChangedFile, FileChange, Preflight},
    runner::{self, Cancel, RunSpec},
    store,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tauri::Emitter;
use tokio::sync::{Notify, watch};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum TaskStatus {
    Open,
    Paused,
    Closed,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RunStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Killed,
    TimedOut,
    Cancelled,
}
impl RunStatus {
    pub fn finished(self) -> bool {
        !matches!(self, Self::Queued | Self::Running)
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Source {
    Bot,
    Manual,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ErrorKind {
    Exit,
    Timeout,
    Killed,
    Launch,
    Denied,
    Interrupted,
    Cancelled,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RunError {
    pub kind: ErrorKind,
    pub message: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct TaskRecord {
    pub id: String,
    pub project: String,
    pub title: String,
    pub source: Source,
    pub created_at: u64,
    pub updated_at: u64,
    pub status: TaskStatus,
    pub pause_reason: Option<String>,
    pub dispatches: u32,
    pub dispatch_limit: u32,
    pub runs: Vec<RunRecord>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RunRecord {
    pub id: String,
    pub task_id: String,
    pub agent_id: String,
    pub source: Source,
    pub title: String,
    pub prompt: String,
    pub status: RunStatus,
    pub queued_at: u64,
    pub started_at: Option<u64>,
    pub ended_at: Option<u64>,
    pub timeout_secs: u64,
    pub exit_code: Option<i32>,
    pub summary: String,
    pub error: Option<RunError>,
    pub denied: Vec<String>,
    pub diff_stat: String,
    pub changed_files: Vec<ChangedFile>,
    pub truncated: bool,
    pub recovery_point: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct JobSpec {
    pub project: String,
    pub agent: String,
    pub prompt: String,
    pub title: Option<String>,
    pub task_id: Option<String>,
    pub timeout_secs: Option<u64>,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Submitted {
    pub job_id: String,
    pub task_id: String,
    pub position: usize,
}

#[derive(Serialize, Clone, Debug)]
pub struct HubError {
    pub code: String,
    pub message: String,
}
impl HubError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}
impl From<String> for HubError {
    fn from(message: String) -> Self {
        Self::new("internal", message)
    }
}

#[derive(Clone, Debug)]
pub struct JobView {
    pub task: TaskRecord,
    pub run: RunRecord,
    pub position: Option<usize>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct GatewaySettings {
    pub version: u32,
    pub accepting: bool,
    pub allowed_projects: Vec<String>,
}
impl Default for GatewaySettings {
    fn default() -> Self {
        Self {
            version: 1,
            accepting: false,
            allowed_projects: Vec::new(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum GatewayStatus {
    Starting,
    Listening { pipe: String },
    Failed { error: String },
}

#[derive(Serialize, Clone, Debug)]
pub struct Rejection {
    pub project: String,
    pub agent: String,
    pub code: String,
    pub at: u64,
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
    Run {
        task_id: String,
        run: RunRecord,
    },
    Gateway {
        status: GatewayStatus,
        accepting: bool,
        rejected: Option<Rejection>,
    },
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

pub struct LogTail {
    pub lines: Vec<LogLine>,
    pub truncated: bool,
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

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ProbeResult {
    pub id: String,
    pub resolved: Option<String>,
    pub version: Option<String>,
    pub error: Option<String>,
}

#[derive(Serialize, Clone, Debug)]
pub struct AgentView {
    pub id: String,
    pub name: String,
    pub kind: AgentKind,
    pub enabled: bool,
    pub role: String,
    pub available: Option<bool>,
    pub version: Option<String>,
}

#[derive(Clone)]
pub struct Orchestra {
    inner: Arc<Inner>,
}
struct Inner {
    config_path: PathBuf,
    gateway_path: PathBuf,
    tasks_dir: PathBuf,
    sink: Arc<dyn Sink>,
    core: Mutex<Core>,
    submissions: tokio::sync::Mutex<()>,
    wake: Arc<Notify>,
    changed: watch::Sender<u64>,
    shutdown: Cancel,
}
struct Core {
    registry: Registry,
    gateway: GatewaySettings,
    gateway_status: GatewayStatus,
    tasks: HashMap<String, TaskRecord>,
    queue: VecDeque<(String, String)>,
    live: Option<Live>,
    logs: HashMap<String, VecDeque<LogLine>>,
    log_nodes: VecDeque<String>,
    streaks: HashMap<String, (String, u32)>,
    project_counts: HashMap<String, u32>,
    probes: HashMap<String, (Instant, ProbeResult)>,
    worker_started: bool,
}
struct Live {
    task_id: String,
    job_id: String,
    cancel: Cancel,
    stop_message: Option<String>,
}

const LOG_LINES: usize = 2000;
const LOG_NODES: usize = 50;

fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value.lock().unwrap_or_else(|e| e.into_inner())
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn head(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        text.into()
    } else {
        let mut text: String = text.chars().take(limit.saturating_sub(1)).collect();
        text.push('…');
        text
    }
}
fn tail(text: &str, limit: usize) -> String {
    let count = text.chars().count();
    text.chars().skip(count.saturating_sub(limit)).collect()
}
fn project_key(project: &str) -> String {
    if cfg!(windows) {
        project.to_lowercase()
    } else {
        project.to_owned()
    }
}
pub fn normalize_project(project: &str) -> Result<String, HubError> {
    let path = Path::new(project)
        .canonicalize()
        .map_err(|_| HubError::new("bad_request", "プロジェクトのフォルダが見つかりません"))?;
    if !path.is_dir() {
        return Err(HubError::new(
            "bad_request",
            "プロジェクトにはフォルダを指定してください",
        ));
    }
    let path = path.to_string_lossy();
    Ok(if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else {
        path.strip_prefix(r"\\?\").unwrap_or(&path).into()
    })
}
fn normalized_projects(projects: Vec<String>) -> Result<Vec<String>, HubError> {
    let mut normalized = Vec::new();
    for project in projects {
        let project = normalize_project(&project)?;
        if !normalized
            .iter()
            .any(|p: &String| project_key(p) == project_key(&project))
        {
            normalized.push(project);
        }
    }
    Ok(normalized)
}

impl Orchestra {
    pub fn new(
        config_path: PathBuf,
        gateway_path: PathBuf,
        tasks_dir: PathBuf,
        sink: Arc<dyn Sink>,
    ) -> Self {
        let mut tasks = HashMap::new();
        for mut task in store::load(&tasks_dir) {
            let mut recovered = false;
            for run in &mut task.runs {
                if !run.status.finished() {
                    run.status = if run.status == RunStatus::Queued {
                        RunStatus::Cancelled
                    } else {
                        RunStatus::Killed
                    };
                    run.error = Some(RunError {
                        kind: ErrorKind::Interrupted,
                        message: "アプリが終了していました".into(),
                    });
                    run.ended_at = Some(now());
                    recovered = true;
                }
            }
            if recovered {
                task.updated_at = now();
                let _ = store::save(&tasks_dir, &task);
            }
            tasks.insert(task.id.clone(), task);
        }
        let gateway = std::fs::read(&gateway_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<GatewaySettings>(&bytes).ok())
            .filter(|g| g.version == 1)
            .and_then(|mut g| {
                g.allowed_projects = normalized_projects(g.allowed_projects).ok()?;
                Some(g)
            })
            .unwrap_or_default();
        let registry = config::load(&config_path);
        Self {
            inner: Arc::new(Inner {
                config_path,
                gateway_path,
                tasks_dir,
                sink,
                submissions: tokio::sync::Mutex::new(()),
                wake: Arc::new(Notify::new()),
                changed: watch::channel(0).0,
                shutdown: Cancel::new(),
                core: Mutex::new(Core {
                    registry,
                    gateway,
                    gateway_status: GatewayStatus::Starting,
                    tasks,
                    queue: VecDeque::new(),
                    live: None,
                    logs: HashMap::new(),
                    log_nodes: VecDeque::new(),
                    streaks: HashMap::new(),
                    project_counts: HashMap::new(),
                    probes: HashMap::new(),
                    worker_started: false,
                }),
            }),
        }
    }
    fn notify(&self) {
        self.inner.changed.send_modify(|v| *v = v.wrapping_add(1));
    }
    pub fn registry(&self) -> Registry {
        lock(&self.inner.core).registry.clone()
    }
    pub fn save_registry(&self, registry: Registry) -> Result<Registry, String> {
        let mut core = lock(&self.inner.core);
        config::save(&self.inner.config_path, &registry)?;
        core.registry = registry.clone();
        core.probes.clear();
        Ok(registry)
    }
    pub fn reset_registry(&self) -> Result<Registry, String> {
        self.save_registry(config::defaults())
    }
    pub async fn preflight(&self, project: &Path) -> Preflight {
        git::preflight(project).await
    }
    pub fn gateway_settings(&self) -> GatewaySettings {
        lock(&self.inner.core).gateway.clone()
    }
    pub fn set_gateway_status(&self, status: GatewayStatus) {
        let mut core = lock(&self.inner.core);
        core.gateway_status = status;
        self.gateway_event(&core, None);
    }
    fn gateway_event(&self, core: &Core, rejected: Option<Rejection>) {
        self.inner.sink.event(&OrchestratorEvent::Gateway {
            status: core.gateway_status.clone(),
            accepting: core.gateway.accepting,
            rejected,
        });
    }
    pub fn gateway_view(&self) -> serde_json::Value {
        let core = lock(&self.inner.core);
        let pipe = match &core.gateway_status {
            GatewayStatus::Listening { pipe } => Some(pipe.clone()),
            _ => super::ipc::pipe_name().ok(),
        };
        let cli_path = std::env::current_exe()
            .ok()
            .and_then(|path| path.parent().map(|p| p.join("gbd.exe")))
            .filter(|path| path.is_file())
            .map(|p| p.to_string_lossy().into_owned());
        serde_json::json!({"accepting":core.gateway.accepting,"allowedProjects":core.gateway.allowed_projects,
            "status":core.gateway_status,"pipe":pipe,"cliPath":cli_path})
    }
    pub fn gateway_save(
        &self,
        accepting: bool,
        projects: Vec<String>,
    ) -> Result<serde_json::Value, String> {
        let projects = normalized_projects(projects).map_err(|e| e.message)?;
        let settings = GatewaySettings {
            version: 1,
            accepting,
            allowed_projects: projects,
        };
        {
            let mut core = lock(&self.inner.core);
            self.write_gateway(&settings)?;
            core.gateway = settings;
            self.gateway_event(&core, None);
        }
        Ok(self.gateway_view())
    }
    fn write_gateway(&self, settings: &GatewaySettings) -> Result<(), String> {
        let parent = self
            .inner
            .gateway_path
            .parent()
            .unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("設定フォルダを作れませんでした: {e}"))?;
        let temp = parent.join(format!(".gateway-{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| {
            let bytes = serde_json::to_vec_pretty(settings).map_err(|e| e.to_string())?;
            std::fs::write(&temp, bytes).map_err(|e| e.to_string())?;
            std::fs::rename(&temp, &self.inner.gateway_path).map_err(|e| e.to_string())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result.map_err(|e| format!("ゲートウェイの設定を保存できませんでした: {e}"))
    }
    fn check_agent(core: &Core, job: &JobSpec, source: Source) -> Result<AgentSpec, HubError> {
        if source == Source::Bot && !core.gateway.accepting {
            return Err(HubError::new(
                "not_accepting",
                "Grok Bot からの受付は停止しています",
            ));
        }
        let agent = core
            .registry
            .agents
            .iter()
            .find(|a| a.id == job.agent)
            .ok_or_else(|| HubError::new("agent_not_found", "エージェントが見つかりません"))?;
        if !agent.enabled {
            return Err(HubError::new(
                "agent_disabled",
                "このエージェントは無効です",
            ));
        }
        if !agent.allow_dangerous
            && let Some(flag) = config::dangerous_flag(&agent.args)
        {
            return Err(HubError::new(
                "agent_disabled",
                format!("危険なフラグ「{flag}」は許可されていません"),
            ));
        }
        Ok(agent.clone())
    }
    fn check_project(
        &self,
        core: &Core,
        job: &JobSpec,
        source: Source,
        project: &str,
    ) -> Result<(), HubError> {
        if source == Source::Bot
            && !core
                .gateway
                .allowed_projects
                .iter()
                .any(|p| project_key(p) == project_key(project))
        {
            self.gateway_event(
                core,
                Some(Rejection {
                    project: project.into(),
                    agent: job.agent.clone(),
                    code: "project_not_allowed".into(),
                    at: now(),
                }),
            );
            return Err(HubError::new(
                "project_not_allowed",
                "このプロジェクトは Grok Bot に許可されていません",
            ));
        }
        Ok(())
    }
    pub async fn submit(&self, mut job: JobSpec, source: Source) -> Result<Submitted, HubError> {
        job.prompt = job.prompt.trim().into();
        if job.prompt.is_empty() || job.prompt.chars().count() > 20000 {
            return Err(HubError::new(
                "bad_request",
                "プロンプトは 1〜20000 文字で入力してください",
            ));
        }
        let _serial = self.inner.submissions.lock().await;
        {
            Self::check_agent(&lock(&self.inner.core), &job, source)?;
        }
        let project = normalize_project(&job.project)?;
        {
            self.check_project(&lock(&self.inner.core), &job, source, &project)?;
        }
        let preflight = git::preflight(Path::new(&project)).await;
        if !preflight.is_git || !preflight.has_commit {
            return Err(HubError::new(
                "not_git_repo",
                "1 つ以上のコミットがある git リポジトリを指定してください",
            ));
        }
        let mut core = lock(&self.inner.core);
        let agent = Self::check_agent(&core, &job, source)?;
        self.check_project(&core, &job, source, &project)?;
        let max_timeout = agent
            .timeout_secs
            .unwrap_or(core.registry.limits.default_timeout_secs);
        let timeout_secs = match job.timeout_secs {
            Some(t) if t < 10 => {
                return Err(HubError::new(
                    "bad_request",
                    "timeout_secs は 10 秒以上にしてください",
                ));
            }
            Some(t) => t.min(max_timeout),
            None => max_timeout,
        };
        let existing = if let Some(id) = &job.task_id {
            // Grok Bot sees only its own tasks; a task made on the screen does not exist for it.
            let task = core
                .tasks
                .get(id)
                .filter(|task| source == Source::Manual || task.source == Source::Bot)
                .ok_or_else(|| HubError::new("not_found", "タスクが見つかりません"))?;
            if project_key(&task.project) != project_key(&project) {
                return Err(HubError::new(
                    "project_mismatch",
                    "タスクのプロジェクトが一致しません",
                ));
            }
            match task.status {
                TaskStatus::Closed => {
                    return Err(HubError::new("task_closed", "このタスクは終了しています"));
                }
                TaskStatus::Paused => {
                    return Err(HubError::new(
                        "task_paused",
                        format!(
                            "タスクは一時停止しています: {}",
                            task.pause_reason.as_deref().unwrap_or_default()
                        ),
                    ));
                }
                TaskStatus::Open => {}
            }
            Some(task.clone())
        } else {
            None
        };
        let key = project_key(&project);
        if source == Source::Bot
            && core.project_counts.get(&key).copied().unwrap_or(0)
                >= core.registry.limits.max_consecutive_per_project
        {
            self.gateway_event(
                &core,
                Some(Rejection {
                    project: project.clone(),
                    agent: job.agent.clone(),
                    code: "project_paused".into(),
                    at: now(),
                }),
            );
            return Err(HubError::new(
                "project_paused",
                "プロジェクトの連続実行が上限に達しました。画面から再開してください",
            ));
        }
        if let Some(task) = &existing
            && task.dispatches >= task.dispatch_limit
        {
            let reason = format!("ディスパッチの上限 {} 回に達しました", task.dispatch_limit);
            self.pause_locked(&mut core, &task.id, &reason)?;
            return Err(HubError::new("task_limit", reason));
        }
        if core.queue.len() >= core.registry.limits.max_queue {
            return Err(HubError::new(
                "queue_full",
                "実行待ちのキューがいっぱいです",
            ));
        }
        if self.inner.shutdown.is_cancelled() {
            return Err(HubError::new("internal", "アプリを終了しています"));
        }
        let title = job
            .title
            .as_deref()
            .map(|s| head(s, 80))
            .unwrap_or_else(|| {
                head(
                    job.prompt
                        .lines()
                        .find(|line| !line.trim().is_empty())
                        .unwrap_or_default(),
                    40,
                )
            });
        let mut task = existing.unwrap_or_else(|| TaskRecord {
            id: unique_id('t', |id| core.tasks.contains_key(id)),
            project: project.clone(),
            title: title.clone(),
            source,
            created_at: now(),
            updated_at: now(),
            status: TaskStatus::Open,
            pause_reason: None,
            dispatches: 0,
            dispatch_limit: core.registry.limits.max_dispatches_per_task,
            runs: Vec::new(),
        });
        let run = RunRecord {
            id: unique_id('j', |id| {
                core.tasks
                    .values()
                    .any(|t| t.runs.iter().any(|r| r.id == id))
            }),
            task_id: task.id.clone(),
            agent_id: job.agent,
            source,
            title,
            prompt: job.prompt,
            status: RunStatus::Queued,
            queued_at: now(),
            started_at: None,
            ended_at: None,
            timeout_secs,
            exit_code: None,
            summary: String::new(),
            error: None,
            denied: Vec::new(),
            diff_stat: String::new(),
            changed_files: Vec::new(),
            truncated: false,
            recovery_point: None,
        };
        let position = core.queue.len() + usize::from(core.live.is_some());
        task.dispatches += 1;
        task.updated_at = now();
        task.runs.push(run.clone());
        store::save(&self.inner.tasks_dir, &task)?;
        let count = if source == Source::Manual {
            0
        } else {
            core.project_counts.get(&key).copied().unwrap_or(0) + 1
        };
        core.project_counts.insert(key.clone(), count);
        core.queue.push_back((task.id.clone(), run.id.clone()));
        core.tasks.insert(task.id.clone(), task.clone());
        self.inner
            .sink
            .event(&OrchestratorEvent::Task { task: task.clone() });
        self.emit_run(&run);
        if !core.worker_started {
            core.worker_started = true;
            self.spawn_worker();
        }
        self.inner.wake.notify_one();
        self.notify();
        Ok(Submitted {
            job_id: run.id,
            task_id: task.id,
            position,
        })
    }
    fn emit_run(&self, run: &RunRecord) {
        self.inner.sink.event(&OrchestratorEvent::Run {
            task_id: run.task_id.clone(),
            run: run.clone(),
        });
    }
    fn pause_locked(&self, core: &mut Core, task_id: &str, reason: &str) -> Result<(), HubError> {
        let mut task = core
            .tasks
            .get(task_id)
            .cloned()
            .ok_or_else(|| HubError::new("not_found", "タスクが見つかりません"))?;
        if task.status == TaskStatus::Closed {
            return Ok(());
        }
        task.status = TaskStatus::Paused;
        task.pause_reason = Some(reason.into());
        task.updated_at = now();
        let cancelled = cancel_queued(&mut task, "タスクが一時停止したため取り消しました");
        store::save(&self.inner.tasks_dir, &task)?;
        core.queue.retain(|(id, _)| id != task_id);
        core.tasks.insert(task_id.into(), task.clone());
        for run in cancelled {
            self.emit_run(&run);
        }
        self.inner.sink.event(&OrchestratorEvent::Task { task });
        self.notify();
        Ok(())
    }
    fn spawn_worker(&self) {
        let weak = Arc::downgrade(&self.inner);
        let wake = self.inner.wake.clone();
        let shutdown = self.inner.shutdown.clone();
        tokio::spawn(async move {
            loop {
                let notified = wake.notified();
                let Some(inner) = weak.upgrade() else {
                    break;
                };
                let hub = Self { inner };
                if shutdown.is_cancelled() {
                    break;
                }
                let work = hub.take_next();
                if let Some((task, run, spec, cancel)) = work {
                    hub.execute(task, run, spec, cancel).await;
                } else {
                    drop(hub);
                    tokio::select! { _ = notified => {}, _ = shutdown.cancelled() => break }
                }
            }
        });
    }
    fn take_next(&self) -> Option<(TaskRecord, RunRecord, Option<AgentSpec>, Cancel)> {
        let mut core = lock(&self.inner.core);
        while let Some((task_id, job_id)) = core.queue.pop_front() {
            // A stale entry must not stop the worker from reaching the jobs behind it.
            let Some(mut task) = core.tasks.get(&task_id).cloned() else {
                continue;
            };
            let Some(run) = task.runs.iter_mut().find(|run| run.id == job_id) else {
                continue;
            };
            if run.status != RunStatus::Queued {
                continue;
            }
            run.status = RunStatus::Running;
            run.started_at = Some(now());
            let run = run.clone();
            task.updated_at = now();
            if let Err(error) = store::save(&self.inner.tasks_dir, &task) {
                let failed = task.runs.iter_mut().find(|r| r.id == job_id).unwrap();
                fail_run(failed, ErrorKind::Launch, error);
                self.emit_run(failed);
                core.tasks.insert(task_id, task.clone());
                self.inner.sink.event(&OrchestratorEvent::Task { task });
                self.notify();
                continue;
            }
            let spec = core
                .registry
                .agents
                .iter()
                .find(|a| a.id == run.agent_id)
                .cloned();
            let cancel = Cancel::new();
            core.live = Some(Live {
                task_id: task_id.clone(),
                job_id,
                cancel: cancel.clone(),
                stop_message: None,
            });
            core.tasks.insert(task_id, task.clone());
            self.emit_run(&run);
            self.notify();
            return Some((task, run, spec, cancel));
        }
        None
    }
    async fn execute(
        &self,
        task: TaskRecord,
        mut run: RunRecord,
        spec: Option<AgentSpec>,
        cancel: Cancel,
    ) {
        let limits = self.registry().limits;
        let logger = NodeLog::new(self.inner.clone(), &task.id, &run.id);
        let project = Path::new(&task.project);
        match git::snapshot(project).await {
            Err(error) => fail_run(
                &mut run,
                ErrorKind::Launch,
                format!("作業前の状態を記録できませんでした: {error}"),
            ),
            Ok(snapshot) => {
                run.recovery_point = snapshot.base.clone();
                let execution = async {
                    let spec = spec.ok_or_else(|| "エージェントが見つかりません".to_owned())?;
                    if !spec.enabled {
                        return Err("このエージェントは無効です".into());
                    }
                    let adapter = ConfiguredAdapter::new(spec.clone(), limits.log_tail_bytes);
                    let prompt_file = std::env::temp_dir()
                        .join("grok-build-desktop")
                        .join(&task.id)
                        .join(format!("{}.txt", run.id));
                    let plan = adapter.launch(&DispatchRequest {
                        cwd: project,
                        prompt: &run.prompt,
                        prompt_file: &prompt_file,
                    })?;
                    let safe_args =
                        config::expand(&spec.args, Some("<prompt>"), Some(&prompt_file), project);
                    logger.line(
                        LogStream::System,
                        format!("{} {}", adapter.spec().program, safe_args.join(" ")),
                    );
                    let mut acc = Accumulator::default();
                    let exit = runner::run(
                        RunSpec {
                            program: plan.program,
                            args: plan.args,
                            cwd: plan.cwd,
                            stdin: plan.stdin,
                            prompt_file: plan.prompt_file,
                            timeout: Duration::from_secs(run.timeout_secs),
                        },
                        cancel,
                        |stream, display, full| {
                            if let Some(line) = full {
                                adapter.observe(stream, line, &mut acc);
                            } else if stream != LogStream::System {
                                acc.note_overlong();
                            }
                            logger.line(stream, display);
                        },
                    )
                    .await?;
                    Ok::<_, String>((adapter.outcome(acc, &exit), exit))
                }
                .await;
                match execution {
                    Err(error) => fail_run(&mut run, ErrorKind::Launch, error),
                    Ok((outcome, exit)) => {
                        run.status = outcome.status;
                        run.summary = head(&outcome.summary, 4000);
                        run.denied = outcome.denied;
                        run.exit_code = exit.code;
                        let kind = match run.status {
                            RunStatus::TimedOut => Some(ErrorKind::Timeout),
                            RunStatus::Killed => Some(ErrorKind::Killed),
                            RunStatus::Failed if outcome.denied_failure => Some(ErrorKind::Denied),
                            RunStatus::Failed => Some(ErrorKind::Exit),
                            _ => None,
                        };
                        run.error = kind.map(|kind| RunError {
                            kind,
                            message: tail(
                                if outcome.stderr_tail.is_empty() {
                                    &outcome.summary
                                } else {
                                    &outcome.stderr_tail
                                },
                                2000,
                            ),
                        });
                    }
                }
                let changes = git::changes(project, &snapshot, 200)
                    .await
                    .unwrap_or_else(|error| git::Changes {
                        stat: format!("差分を取れませんでした: {error}"),
                        ..Default::default()
                    });
                run.truncated = changes.truncated || changes.stat.chars().count() > 4000;
                run.diff_stat = head(&changes.stat, 4000);
                run.changed_files = changes.files;
            }
        }
        if let Some(error) = &run.error {
            logger.line(LogStream::System, error.message.clone());
        }
        logger.line(
            LogStream::System,
            format!(
                "終了コード {}",
                run.exit_code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "不明".into())
            ),
        );
        drop(logger); // All batches must precede the terminal Run event and waiter notification.
        run.ended_at = Some(now());
        self.finish_run(run, &limits);
    }
    fn finish_run(&self, mut run: RunRecord, limits: &config::Limits) {
        let mut core = lock(&self.inner.core);
        if let Some(live) = &core.live
            && let Some(message) = &live.stop_message
        {
            run.status = RunStatus::Killed;
            run.error = Some(RunError {
                kind: ErrorKind::Killed,
                message: message.clone(),
            });
        }
        let Some(mut task) = core.tasks.get(&run.task_id).cloned() else {
            core.live = None;
            self.notify();
            return;
        };
        let mut pause = None;
        match run.status {
            RunStatus::Failed | RunStatus::TimedOut => {
                let signature = breaker::error_signature(
                    &run.agent_id,
                    run.exit_code,
                    run.error
                        .as_ref()
                        .map(|e| e.message.as_str())
                        .unwrap_or(&run.summary),
                );
                let streak = core
                    .streaks
                    .entry(task.id.clone())
                    .or_insert_with(|| (signature.clone(), 0));
                if streak.0 == signature {
                    streak.1 += 1;
                } else {
                    *streak = (signature, 1);
                }
                if streak.1 >= limits.same_error_limit {
                    pause = Some(format!("同じ失敗が {} 回続きました", streak.1));
                }
            }
            RunStatus::Succeeded => {
                core.streaks.remove(&task.id);
            }
            _ => {}
        }
        if limits.pause_on_deletion {
            let deleted: Vec<_> = run
                .changed_files
                .iter()
                .filter(|f| f.change == FileChange::Deleted)
                .take(5)
                .map(|f| f.path.as_str())
                .collect();
            if !deleted.is_empty() {
                pause = Some(format!(
                    "作業前からあったファイルが削除されました。確認してから再開してください: {}",
                    deleted.join(", ")
                ));
            }
        }
        if task.status != TaskStatus::Closed
            && let Some(reason) = pause
        {
            task.status = TaskStatus::Paused;
            task.pause_reason = Some(reason);
        }
        let cancelled = if task.status == TaskStatus::Paused {
            cancel_queued(&mut task, "タスクが一時停止したため取り消しました")
        } else {
            Vec::new()
        };
        if task.status != TaskStatus::Open {
            core.queue.retain(|(id, _)| id != &task.id);
        }
        *task
            .runs
            .iter_mut()
            .find(|r| r.id == run.id)
            .expect("running record") = run.clone();
        task.updated_at = now();
        if let Err(error) = store::save(&self.inner.tasks_dir, &task) {
            task.status = TaskStatus::Paused;
            task.pause_reason = Some(error);
            for queued in cancel_queued(&mut task, "タスクが一時停止したため取り消しました")
            {
                self.emit_run(&queued);
            }
            core.queue.retain(|(id, _)| id != &task.id);
        }
        core.tasks.insert(task.id.clone(), task.clone());
        core.live = None;
        self.emit_run(&run);
        for queued in cancelled {
            self.emit_run(&queued);
        }
        self.inner.sink.event(&OrchestratorEvent::Task { task });
        self.notify();
    }
    pub fn tasks(&self, project: &Path) -> Vec<TaskRecord> {
        let normalized = normalize_project(&project.to_string_lossy())
            .unwrap_or_else(|_| project.to_string_lossy().into_owned());
        let mut tasks: Vec<_> = lock(&self.inner.core)
            .tasks
            .values()
            .filter(|t| project_key(&t.project) == project_key(&normalized))
            .cloned()
            .collect();
        tasks.sort_by_key(|t| std::cmp::Reverse(t.updated_at));
        tasks
    }
    pub fn task(&self, id: &str) -> Option<TaskRecord> {
        lock(&self.inner.core).tasks.get(id).cloned()
    }
    pub fn job(&self, id: &str) -> Result<JobView, HubError> {
        job_view(&lock(&self.inner.core), id)
    }
    pub async fn wait(&self, id: &str, timeout_secs: u64) -> Result<JobView, HubError> {
        if !(1..=1800).contains(&timeout_secs) {
            return Err(HubError::new(
                "bad_request",
                "待機時間は 1〜1800 秒にしてください",
            ));
        }
        let mut changed = self.inner.changed.subscribe();
        let deadline = tokio::time::sleep(Duration::from_secs(timeout_secs));
        tokio::pin!(deadline);
        loop {
            let view = self.job(id)?;
            if view.run.status.finished() {
                return Ok(view);
            }
            tokio::select! { _ = &mut deadline => return self.job(id), result = changed.changed() => { if result.is_err() { return self.job(id); } } }
        }
    }
    async fn stop(&self, id: &str, message: &str, emergency: bool) -> Result<JobView, HubError> {
        let running;
        {
            let mut core = lock(&self.inner.core);
            let view = job_view(&core, id)?;
            if view.run.status.finished() {
                return Ok(view);
            }
            running = view.run.status == RunStatus::Running;
            if running {
                let live = core
                    .live
                    .as_mut()
                    .filter(|l| l.job_id == id)
                    .ok_or_else(|| HubError::new("internal", "実行中のジョブが見つかりません"))?;
                live.stop_message = Some(message.into());
                live.cancel.cancel();
                if emergency {
                    self.pause_locked(&mut core, &view.task.id, "緊急停止しました")?;
                }
            } else {
                let mut task = view.task;
                let run = task.runs.iter_mut().find(|r| r.id == id).unwrap();
                cancel_run(run, message);
                let run = run.clone();
                task.updated_at = now();
                store::save(&self.inner.tasks_dir, &task)?;
                core.queue.retain(|(_, job_id)| job_id != id);
                core.tasks.insert(task.id.clone(), task.clone());
                self.emit_run(&run);
                self.inner.sink.event(&OrchestratorEvent::Task { task });
                self.notify();
            }
        }
        if running {
            self.wait(id, 1800).await
        } else {
            self.job(id)
        }
    }
    pub async fn kill(&self, id: &str) -> Result<(), HubError> {
        self.stop(id, "画面から緊急停止しました", true)
            .await
            .map(|_| ())
    }
    /// The emergency stop on the screen. Under one lock it refuses further Bot jobs, cancels
    /// every queued job and kills the running one, so the worker cannot start the next job
    /// in between. Bot intake stays off in memory even when the setting cannot be written.
    pub async fn stop_all(&self) -> Result<(), HubError> {
        let (running, saved) = {
            let mut core = lock(&self.inner.core);
            core.gateway.accepting = false;
            let settings = core.gateway.clone();
            let saved = self.write_gateway(&settings);
            self.gateway_event(&core, None);
            let queued: Vec<_> = core.queue.drain(..).collect();
            let mut touched: Vec<String> = Vec::new();
            let mut cancelled = Vec::new();
            for (task_id, job_id) in queued {
                let Some(task) = core.tasks.get_mut(&task_id) else {
                    continue;
                };
                let Some(run) = task
                    .runs
                    .iter_mut()
                    .find(|r| r.id == job_id && r.status == RunStatus::Queued)
                else {
                    continue;
                };
                cancel_run(run, "緊急停止で取り消しました");
                cancelled.push(run.clone());
                task.updated_at = now();
                if !touched.contains(&task_id) {
                    touched.push(task_id);
                }
            }
            for run in &cancelled {
                self.emit_run(run);
            }
            for task_id in &touched {
                if let Some(task) = core.tasks.get(task_id) {
                    let _ = store::save(&self.inner.tasks_dir, task);
                    self.inner
                        .sink
                        .event(&OrchestratorEvent::Task { task: task.clone() });
                }
            }
            let running = core.live.as_mut().map(|live| {
                live.stop_message = Some("緊急停止しました".into());
                live.cancel.cancel();
                (live.task_id.clone(), live.job_id.clone())
            });
            if let Some((task_id, _)) = &running {
                let _ = self.pause_locked(&mut core, task_id, "緊急停止しました");
            }
            self.notify();
            (running.map(|(_, job_id)| job_id), saved)
        };
        if let Some(id) = running {
            self.wait(&id, 1800).await?;
        }
        saved.map_err(|error| HubError::new("internal", error))
    }
    pub async fn cancel(&self, id: &str, source: Source) -> Result<JobView, HubError> {
        let running = self.job(id)?.run.status == RunStatus::Running;
        let message = match (source, running) {
            (Source::Bot, true) => "Grok Bot が止めました",
            (Source::Bot, false) => "Grok Bot が取り消しました",
            (Source::Manual, true) => "画面で止めました",
            (Source::Manual, false) => "画面で取り消しました",
        };
        self.stop(id, message, false).await
    }
    pub fn resume(&self, id: &str) -> Result<(), HubError> {
        let mut core = lock(&self.inner.core);
        let mut task = core
            .tasks
            .get(id)
            .cloned()
            .ok_or_else(|| HubError::new("not_found", "タスクが見つかりません"))?;
        if task.status == TaskStatus::Closed {
            return Err(HubError::new("task_closed", "このタスクは終了しています"));
        }
        task.status = TaskStatus::Open;
        task.pause_reason = None;
        if task.dispatches >= task.dispatch_limit {
            task.dispatch_limit = task
                .dispatches
                .saturating_add(core.registry.limits.max_dispatches_per_task);
        }
        task.updated_at = now();
        store::save(&self.inner.tasks_dir, &task)?;
        core.streaks.remove(id);
        core.project_counts.insert(project_key(&task.project), 0);
        core.tasks.insert(id.into(), task.clone());
        self.inner.sink.event(&OrchestratorEvent::Task { task });
        self.notify();
        Ok(())
    }
    pub fn resume_project(&self, project: &str) -> Result<(), HubError> {
        let project = normalize_project(project)?;
        lock(&self.inner.core)
            .project_counts
            .insert(project_key(&project), 0);
        Ok(())
    }
    pub async fn close(&self, id: &str) -> Result<(), HubError> {
        let running;
        {
            let mut core = lock(&self.inner.core);
            let mut task = core
                .tasks
                .get(id)
                .cloned()
                .ok_or_else(|| HubError::new("not_found", "タスクが見つかりません"))?;
            task.status = TaskStatus::Closed;
            task.pause_reason = None;
            task.updated_at = now();
            let cancelled = cancel_queued(&mut task, "タスクを閉じました");
            store::save(&self.inner.tasks_dir, &task)?;
            core.queue.retain(|(task_id, _)| task_id != id);
            running = core
                .live
                .as_ref()
                .filter(|l| l.task_id == id)
                .map(|l| l.job_id.clone());
            if let Some(live) = core.live.as_mut().filter(|l| l.task_id == id) {
                live.stop_message = Some("タスクを閉じました".into());
                live.cancel.cancel();
            }
            core.tasks.insert(id.into(), task.clone());
            for run in cancelled {
                self.emit_run(&run);
            }
            self.inner.sink.event(&OrchestratorEvent::Task { task });
            self.notify();
        }
        if let Some(job) = running {
            self.wait(&job, 1800).await?;
        }
        Ok(())
    }
    pub fn delete(&self, id: &str) -> Result<(), String> {
        let mut core = lock(&self.inner.core);
        let task = core.tasks.get(id).ok_or("タスクが見つかりません")?;
        if task.runs.iter().any(|r| !r.status.finished()) {
            return Err("実行中または実行待ちのジョブがあるタスクは削除できません".into());
        }
        store::delete(&self.inner.tasks_dir, id)?;
        let task = core.tasks.remove(id).unwrap();
        for run in task.runs {
            core.logs.remove(&run.id);
            core.log_nodes.retain(|node| node != &run.id);
        }
        core.streaks.remove(id);
        self.notify();
        Ok(())
    }
    /// What a reloaded screen needs: the running and queued jobs with their tasks, and the gateway.
    pub fn snapshot(&self) -> serde_json::Value {
        let core = lock(&self.inner.core);
        let active: Vec<_> = core
            .tasks
            .values()
            .filter(|t| t.runs.iter().any(|r| !r.status.finished()))
            .collect();
        serde_json::json!({"running":core.live.as_ref().map(|l| &l.job_id),
            "queued":core.queue.iter().map(|(_,id)| id).collect::<Vec<_>>(), "active":active,
            "gateway":{"status":core.gateway_status,"accepting":core.gateway.accepting}})
    }
    pub fn logs(&self, id: &str) -> Vec<LogLine> {
        lock(&self.inner.core)
            .logs
            .get(id)
            .map(|lines| lines.iter().cloned().collect())
            .unwrap_or_default()
    }
    pub fn log_tail(&self, id: &str, n: usize) -> Result<LogTail, HubError> {
        if !(1..=200).contains(&n) {
            return Err(HubError::new(
                "bad_request",
                "ログの行数は 1〜200 にしてください",
            ));
        }
        let core = lock(&self.inner.core);
        job_view(&core, id)?;
        let lines = core.logs.get(id);
        let total = lines.map_or(0, VecDeque::len);
        let result: Vec<_> = lines
            .into_iter()
            .flatten()
            .skip(total.saturating_sub(n))
            .map(|line| LogLine {
                text: head(&line.text, 1000),
                ..line.clone()
            })
            .collect();
        let truncated = result.first().is_some_and(|line| line.seq > 1);
        Ok(LogTail {
            lines: result,
            truncated,
        })
    }
    pub async fn probe(&self, id: &str) -> ProbeResult {
        {
            let core = lock(&self.inner.core);
            if let Some((at, result)) = core.probes.get(id)
                && at.elapsed() < Duration::from_secs(600)
            {
                return result.clone();
            }
        }
        let spec = self.registry().agents.into_iter().find(|s| s.id == id);
        let mut result = ProbeResult {
            id: id.into(),
            resolved: None,
            version: None,
            error: None,
        };
        if let Some(spec) = &spec {
            match runner::resolve_program(&spec.program) {
                Err(error) => result.error = Some(error),
                Ok(program) => {
                    result.resolved = Some(program.to_string_lossy().into_owned());
                    let exit = runner::run(
                        RunSpec {
                            program,
                            args: vec!["--version".into()],
                            cwd: std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir()),
                            stdin: None,
                            prompt_file: None,
                            timeout: Duration::from_secs(10),
                        },
                        Cancel::new(),
                        |stream, text, _| {
                            if stream != LogStream::System
                                && result.version.is_none()
                                && !text.trim().is_empty()
                            {
                                result.version = Some(head(&text, 200));
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
                    if result.error.is_some() {
                        result.version = None;
                    }
                }
            }
        } else {
            result.error = Some("エージェントが見つかりません".into());
        }
        let mut core = lock(&self.inner.core);
        if core.registry.agents.iter().find(|s| s.id == id) == spec.as_ref() {
            core.probes
                .insert(id.into(), (Instant::now(), result.clone()));
        }
        result
    }
    pub async fn agents(&self) -> Vec<AgentView> {
        let agents = self.registry().agents;
        let mut probes = tokio::task::JoinSet::new();
        for (index, agent) in agents.iter().enumerate() {
            let hub = self.clone();
            let id = agent.id.clone();
            probes.spawn(async move { (index, hub.probe(&id).await) });
        }
        let mut results = HashMap::new();
        while let Some(Ok((index, probe))) = probes.join_next().await {
            results.insert(index, probe);
        }
        agents
            .into_iter()
            .enumerate()
            .map(|(i, a)| {
                let probe = results.remove(&i);
                AgentView {
                    id: a.id,
                    name: a.name,
                    kind: a.kind,
                    enabled: a.enabled,
                    role: a.role,
                    available: probe.as_ref().map(|p| p.resolved.is_some()),
                    version: probe.and_then(|p| p.version),
                }
            })
            .collect()
    }
    pub fn shutdown(&self) {
        self.inner.shutdown.cancel();
        let core = lock(&self.inner.core);
        if let Some(live) = &core.live {
            live.cancel.cancel();
        }
        self.inner.wake.notify_waiters();
    }
}
fn unique_id(prefix: char, exists: impl Fn(&str) -> bool) -> String {
    loop {
        let id = format!(
            "{prefix}{}",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        );
        if !exists(&id) {
            return id;
        }
    }
}
fn fail_run(run: &mut RunRecord, kind: ErrorKind, message: String) {
    run.status = RunStatus::Failed;
    run.summary = head(&message, 4000);
    run.error = Some(RunError {
        kind,
        message: tail(&message, 2000),
    });
    run.ended_at = Some(now());
}
fn cancel_run(run: &mut RunRecord, message: &str) {
    run.status = RunStatus::Cancelled;
    run.ended_at = Some(now());
    run.error = Some(RunError {
        kind: ErrorKind::Cancelled,
        message: message.into(),
    });
}
fn cancel_queued(task: &mut TaskRecord, message: &str) -> Vec<RunRecord> {
    task.runs
        .iter_mut()
        .filter(|r| r.status == RunStatus::Queued)
        .map(|r| {
            cancel_run(r, message);
            r.clone()
        })
        .collect()
}
fn job_view(core: &Core, id: &str) -> Result<JobView, HubError> {
    for task in core.tasks.values() {
        if let Some(run) = task.runs.iter().find(|r| r.id == id) {
            let position = match run.status {
                RunStatus::Running => Some(0),
                RunStatus::Queued => core
                    .queue
                    .iter()
                    .position(|(_, j)| j == id)
                    .map(|p| p + usize::from(core.live.is_some())),
                _ => None,
            };
            return Ok(JobView {
                task: task.clone(),
                run: run.clone(),
                position,
            });
        }
    }
    Err(HubError::new("not_found", "ジョブが見つかりません"))
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
        {
            // Memory holds the logs of the most recent jobs only; older ones are dropped whole.
            let mut core = lock(&inner.core);
            if !core.logs.contains_key(node_id) {
                core.logs.insert(node_id.into(), VecDeque::new());
                core.log_nodes.push_back(node_id.into());
                while core.log_nodes.len() > LOG_NODES {
                    if let Some(old) = core.log_nodes.pop_front() {
                        core.logs.remove(&old);
                    }
                }
            }
        }
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
            if let Some(lines) = core.logs.get_mut(&self.buffer.node_id) {
                lines.push_back(line.clone());
                while lines.len() > LOG_LINES {
                    lines.pop_front();
                }
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
    use super::super::testkit::{Kit, Record, git, make_repo};
    use super::*;

    const EDIT: &str = "$p = [Console]::In.ReadToEnd().Trim(); Set-Content -Path tracked.txt -Value 'changed'; Set-Content -Path new.txt -Value 'new'; [Console]::Out.WriteLine(\"done: $p\")";
    const QUICK: &str = "[Console]::Out.WriteLine('quick')";
    const SLOW: &str = "Start-Sleep -Milliseconds 1500; [Console]::Out.WriteLine('slow')";
    const SLEEP: &str = "[Console]::Out.WriteLine('ready'); Start-Sleep 60";
    const FAIL: &str = "[Console]::Error.WriteLine('boom at C:\\work\\1.txt'); exit 2";
    const REMOVE: &str = "Remove-Item tracked.txt; [Console]::Out.WriteLine('removed')";
    const NOISY: &str = "for ($i = 1; $i -le 2500; $i++) { [Console]::Out.WriteLine(\"line $i\") }";
    const MARK: &str = "Set-Content -Path ran.txt -Value 'x'";

    fn run_events(records: &[Record], job: &str) -> Vec<RunStatus> {
        records
            .iter()
            .filter_map(|record| match record {
                Record::Event(OrchestratorEvent::Run { run, .. }) if run.id == job => {
                    Some(run.status)
                }
                _ => None,
            })
            .collect()
    }

    fn rejected(records: &[Record]) -> Vec<String> {
        records
            .iter()
            .filter_map(|record| match record {
                Record::Event(OrchestratorEvent::Gateway {
                    rejected: Some(rejection),
                    ..
                }) => Some(rejection.code.clone()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn bot_job_succeeds_with_changes_events_and_logs() {
        let kit = Kit::new(&[("edit", EDIT)], |_| {}).await;
        let submitted = kit
            .hub
            .submit(kit.job("edit", "  日本語の依頼  "), Source::Bot)
            .await
            .unwrap();
        assert_eq!(submitted.position, 0);
        let view = kit.finish(&submitted.job_id).await;
        let run = &view.run;
        assert_eq!(run.status, RunStatus::Succeeded, "{run:?}");
        assert_eq!(run.summary, "done: 日本語の依頼");
        assert_eq!(run.title, "日本語の依頼");
        assert_eq!(run.exit_code, Some(0));
        assert!(run.error.is_none());
        assert!(
            run.recovery_point
                .as_deref()
                .is_some_and(|id| id.len() == 40)
        );
        let changes: Vec<_> = run
            .changed_files
            .iter()
            .map(|file| (file.path.as_str(), file.change))
            .collect();
        assert_eq!(
            changes,
            [
                ("new.txt", FileChange::Untracked),
                ("tracked.txt", FileChange::Modified)
            ]
        );
        assert!(run.diff_stat.contains("tracked.txt"));
        assert!(run.id.starts_with('j') && run.id.len() == 13);
        assert!(view.task.id.starts_with('t') && view.task.id.len() == 13);
        assert_eq!(
            (view.task.dispatches, view.task.status),
            (1, TaskStatus::Open)
        );

        let records = kit.records();
        assert_eq!(
            run_events(&records, &run.id),
            [RunStatus::Queued, RunStatus::Running, RunStatus::Succeeded]
        );
        let events: Vec<_> = records
            .iter()
            .filter_map(|record| match record {
                Record::Event(
                    event @ (OrchestratorEvent::Task { .. } | OrchestratorEvent::Run { .. }),
                ) => Some(event),
                _ => None,
            })
            .collect();
        assert!(matches!(
            events.first(),
            Some(OrchestratorEvent::Task { .. })
        ));
        assert!(matches!(
            events.last(),
            Some(OrchestratorEvent::Task { .. })
        ));
        let finished_at = records
            .iter()
            .position(|record| matches!(record, Record::Event(OrchestratorEvent::Run { run: r, .. }) if r.id == run.id && r.status.finished()))
            .unwrap();
        let last_batch = records
            .iter()
            .rposition(|record| matches!(record, Record::Logs(batch) if batch.node_id == run.id))
            .unwrap();
        assert!(
            last_batch < finished_at,
            "every log batch precedes the final Run event"
        );
        let logged: Vec<String> = records
            .iter()
            .filter_map(|record| match record {
                Record::Logs(batch) if batch.node_id == run.id => Some(
                    batch
                        .lines
                        .iter()
                        .map(|line| line.text.clone())
                        .collect::<Vec<_>>(),
                ),
                _ => None,
            })
            .flatten()
            .collect();
        assert!(logged.iter().any(|line| line == "done: 日本語の依頼"));
        assert!(
            !logged
                .iter()
                .any(|line| line.contains("日本語の依頼") && line.contains("powershell")),
            "the prompt is not echoed into the command line log"
        );
        // The record survives a restart.
        let saved = store::load(&kit.root.join("tasks"));
        assert_eq!(saved[0].runs[0].status, RunStatus::Succeeded);
    }

    #[tokio::test]
    async fn second_job_waits_its_turn() {
        let kit = Kit::new(&[("slow", SLOW), ("quick", QUICK)], |_| {}).await;
        let first = kit.submit("slow", "1").await;
        kit.until_status(&first, RunStatus::Running).await;
        let second = kit
            .hub
            .submit(kit.job("quick", "2"), Source::Bot)
            .await
            .unwrap();
        assert_eq!(second.position, 1);
        assert_eq!(kit.hub.job(&second.job_id).unwrap().position, Some(1));
        let first = kit.finish(&first).await.run;
        let second = kit.finish(&second.job_id).await.run;
        assert_eq!(second.status, RunStatus::Succeeded);
        assert!(second.started_at.unwrap() >= first.ended_at.unwrap());
    }

    #[tokio::test]
    async fn rejections_and_manual_bypass() {
        let kit = Kit::new(&[("quick", QUICK), ("off", QUICK)], |registry| {
            registry.agents[1].enabled = false;
        })
        .await;
        let code = |result: Result<Submitted, HubError>| result.err().unwrap().code;
        let mut job = kit.job("quick", " ");
        assert_eq!(
            code(kit.hub.submit(job.clone(), Source::Bot).await),
            "bad_request"
        );
        job.prompt = "x".repeat(20001);
        assert_eq!(
            code(kit.hub.submit(job.clone(), Source::Bot).await),
            "bad_request"
        );
        job.prompt = "ok".into();
        job.timeout_secs = Some(5);
        assert_eq!(
            code(kit.hub.submit(job.clone(), Source::Bot).await),
            "bad_request"
        );
        job.timeout_secs = None;
        assert_eq!(
            code(kit.hub.submit(kit.job("nobody", "x"), Source::Bot).await),
            "agent_not_found"
        );
        assert_eq!(
            code(kit.hub.submit(kit.job("off", "x"), Source::Bot).await),
            "agent_disabled"
        );

        let other = kit.root.join("other");
        make_repo(&other);
        let mut elsewhere = kit.job("quick", "x");
        elsewhere.project = other.to_string_lossy().into_owned();
        assert_eq!(
            code(kit.hub.submit(elsewhere.clone(), Source::Bot).await),
            "project_not_allowed"
        );
        assert_eq!(rejected(&kit.records()), ["project_not_allowed"]);
        let mut missing = kit.job("quick", "x");
        missing.project = kit.root.join("missing").to_string_lossy().into_owned();
        assert_eq!(
            code(kit.hub.submit(missing, Source::Bot).await),
            "bad_request"
        );

        let plain = kit.root.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let mut not_git = kit.job("quick", "x");
        not_git.project = plain.to_string_lossy().into_owned();
        assert_eq!(
            code(kit.hub.submit(not_git, Source::Manual).await),
            "not_git_repo"
        );

        let first = kit.submit("quick", "x").await;
        let task_id = kit.finish(&first).await.task.id;
        let mut mismatch = elsewhere.clone();
        mismatch.task_id = Some(task_id);
        assert_eq!(
            code(kit.hub.submit(mismatch, Source::Manual).await),
            "project_mismatch"
        );
        let mut unknown = kit.job("quick", "x");
        unknown.task_id = Some("t000000000000".into());
        assert_eq!(
            code(kit.hub.submit(unknown, Source::Bot).await),
            "not_found"
        );

        kit.hub.gateway_save(false, vec![kit.project()]).unwrap();
        assert_eq!(
            code(kit.hub.submit(kit.job("quick", "x"), Source::Bot).await),
            "not_accepting"
        );
        // The screen is not limited by the gateway switch or the allowed folders.
        let manual = kit.hub.submit(elsewhere, Source::Manual).await.unwrap();
        let view = kit.finish(&manual.job_id).await;
        assert_eq!(view.run.status, RunStatus::Succeeded);
        assert_eq!(view.run.source, Source::Manual);
    }

    #[tokio::test]
    async fn task_limit_pauses_and_resume_raises_it() {
        let kit = Kit::new(&[("quick", QUICK)], |registry| {
            registry.limits.max_dispatches_per_task = 2;
        })
        .await;
        let first = kit.submit("quick", "1").await;
        let task_id = kit.finish(&first).await.task.id;
        let mut next = kit.job("quick", "2");
        next.task_id = Some(task_id.clone());
        kit.finish(
            &kit.hub
                .submit(next.clone(), Source::Bot)
                .await
                .unwrap()
                .job_id,
        )
        .await;
        let error = kit
            .hub
            .submit(next.clone(), Source::Bot)
            .await
            .err()
            .unwrap();
        assert_eq!(error.code, "task_limit");
        let task = kit.hub.task(&task_id).unwrap();
        assert_eq!(task.status, TaskStatus::Paused);
        assert_eq!(
            kit.hub
                .submit(next.clone(), Source::Bot)
                .await
                .err()
                .unwrap()
                .code,
            "task_paused"
        );
        kit.hub.resume(&task_id).unwrap();
        let task = kit.hub.task(&task_id).unwrap();
        assert_eq!((task.status, task.dispatch_limit), (TaskStatus::Open, 4));
        let accepted = kit.hub.submit(next, Source::Bot).await.unwrap();
        assert_eq!(kit.finish(&accepted.job_id).await.task.dispatches, 3);
        kit.hub.close(&task_id).await.unwrap();
        let mut closed = kit.job("quick", "4");
        closed.task_id = Some(task_id);
        assert_eq!(
            kit.hub
                .submit(closed, Source::Bot)
                .await
                .err()
                .unwrap()
                .code,
            "task_closed"
        );
    }

    #[tokio::test]
    async fn same_failure_twice_pauses_and_cancels_queued_jobs() {
        let kit = Kit::new(&[("fail", FAIL), ("quick", QUICK)], |_| {}).await;
        let first = kit.submit("fail", "1").await;
        kit.until_status(&first, RunStatus::Running).await;
        let task_id = kit.hub.job(&first).unwrap().task.id;
        let mut again = kit.job("fail", "2");
        again.task_id = Some(task_id.clone());
        let second = kit.hub.submit(again, Source::Bot).await.unwrap().job_id;
        let mut after = kit.job("quick", "3");
        after.task_id = Some(task_id.clone());
        let third = kit.hub.submit(after, Source::Bot).await.unwrap().job_id;
        let first = kit.finish(&first).await.run;
        assert_eq!(first.status, RunStatus::Failed);
        let error = first.error.unwrap();
        assert_eq!(error.kind, ErrorKind::Exit);
        assert!(error.message.contains("boom"));
        assert_eq!(kit.finish(&second).await.run.status, RunStatus::Failed);
        let third = kit.hub.job(&third).unwrap();
        assert_eq!(third.run.status, RunStatus::Cancelled);
        assert_eq!(third.run.error.unwrap().kind, ErrorKind::Cancelled);
        assert_eq!(third.task.status, TaskStatus::Paused);
        assert!(third.task.pause_reason.unwrap().contains("同じ失敗が 2 回"));
    }

    #[tokio::test]
    async fn consecutive_bot_jobs_pause_the_project_until_a_human_acts() {
        let kit = Kit::new(&[("quick", QUICK)], |registry| {
            registry.limits.max_consecutive_per_project = 2;
        })
        .await;
        for prompt in ["1", "2"] {
            let job = kit.submit("quick", prompt).await;
            kit.finish(&job).await;
        }
        let error = kit
            .hub
            .submit(kit.job("quick", "3"), Source::Bot)
            .await
            .err()
            .unwrap();
        assert_eq!(error.code, "project_paused");
        assert_eq!(rejected(&kit.records()), ["project_paused"]);
        let manual = kit
            .hub
            .submit(kit.job("quick", "by hand"), Source::Manual)
            .await
            .unwrap();
        kit.finish(&manual.job_id).await;
        let job = kit.submit("quick", "4").await;
        kit.finish(&job).await;
        kit.finish(&kit.submit("quick", "5").await).await;
        assert_eq!(
            kit.hub
                .submit(kit.job("quick", "6"), Source::Bot)
                .await
                .err()
                .unwrap()
                .code,
            "project_paused"
        );
        kit.hub.resume_project(&kit.project()).unwrap();
        kit.finish(&kit.submit("quick", "7").await).await;
    }

    #[tokio::test]
    async fn queue_has_a_limit() {
        let kit = Kit::new(&[("sleep", SLEEP), ("quick", QUICK)], |registry| {
            registry.limits.max_queue = 1;
        })
        .await;
        let running = kit.submit("sleep", "1").await;
        kit.until_status(&running, RunStatus::Running).await;
        kit.submit("quick", "2").await;
        let error = kit
            .hub
            .submit(kit.job("quick", "3"), Source::Bot)
            .await
            .err()
            .unwrap();
        assert_eq!(error.code, "queue_full");
        kit.hub.cancel(&running, Source::Manual).await.unwrap();
    }

    #[tokio::test]
    async fn kill_pauses_but_cancel_does_not() {
        let kit = Kit::new(&[("sleep", SLEEP), ("quick", QUICK)], |_| {}).await;
        let killed = kit.submit("sleep", "1").await;
        kit.until_log(&killed, "ready").await;
        kit.hub.kill(&killed).await.unwrap();
        let view = kit.hub.job(&killed).unwrap();
        assert_eq!(view.run.status, RunStatus::Killed);
        assert_eq!(view.run.error.unwrap().kind, ErrorKind::Killed);
        assert_eq!(view.task.status, TaskStatus::Paused);

        let stopped = kit.submit("sleep", "2").await;
        kit.until_log(&stopped, "ready").await;
        let queued = kit.submit("quick", "3").await;
        let cancelled = kit.hub.cancel(&queued, Source::Bot).await.unwrap();
        assert_eq!(cancelled.run.status, RunStatus::Cancelled);
        let view = kit.hub.cancel(&stopped, Source::Bot).await.unwrap();
        assert_eq!(view.run.status, RunStatus::Killed);
        assert_eq!(view.task.status, TaskStatus::Open);
        assert!(kit.hub.delete(&view.task.id).is_ok());
        assert!(kit.hub.task(&view.task.id).is_none());
        assert!(kit.hub.logs(&stopped).is_empty());
    }

    #[tokio::test]
    async fn emergency_stop_refuses_the_bot_and_stops_every_job() {
        let kit = Kit::new(&[("sleep", SLEEP), ("quick", QUICK)], |_| {}).await;
        let running = kit.submit("sleep", "1").await;
        kit.until_log(&running, "ready").await;
        let queued = kit.submit("quick", "2").await;
        let other = kit.submit("quick", "3").await;
        kit.hub.stop_all().await.unwrap();
        let view = kit.hub.job(&running).unwrap();
        assert_eq!(view.run.status, RunStatus::Killed);
        assert_eq!(view.task.status, TaskStatus::Paused);
        for id in [&queued, &other] {
            let run = kit.hub.job(id).unwrap().run;
            assert_eq!(run.status, RunStatus::Cancelled);
            assert_eq!(run.error.unwrap().kind, ErrorKind::Cancelled);
        }
        assert!(!kit.hub.gateway_settings().accepting);
        let refused = kit
            .hub
            .submit(kit.job("quick", "4"), Source::Bot)
            .await
            .unwrap_err();
        assert_eq!(refused.code, "not_accepting");
        // The screen can still start work by hand.
        let manual = kit
            .hub
            .submit(kit.job("quick", "5"), Source::Manual)
            .await
            .unwrap();
        assert_eq!(
            kit.finish(&manual.job_id).await.run.status,
            RunStatus::Succeeded
        );
    }

    #[tokio::test]
    async fn timeout_stops_the_job() {
        let kit = Kit::new(&[("sleep", SLEEP)], |_| {}).await;
        let mut job = kit.job("sleep", "1");
        job.timeout_secs = Some(10);
        let id = kit.hub.submit(job, Source::Bot).await.unwrap().job_id;
        let run = kit.finish(&id).await.run;
        assert_eq!(run.status, RunStatus::TimedOut);
        assert_eq!(run.timeout_secs, 10);
        assert_eq!(run.error.unwrap().kind, ErrorKind::Timeout);
    }

    #[tokio::test]
    async fn deleting_a_tracked_file_pauses_the_task() {
        let kit = Kit::new(&[("remove", REMOVE)], |_| {}).await;
        let view = kit.finish(&kit.submit("remove", "1").await).await;
        assert_eq!(view.run.status, RunStatus::Succeeded);
        assert_eq!(view.run.changed_files[0].change, FileChange::Deleted);
        assert_eq!(view.task.status, TaskStatus::Paused);
        assert!(view.task.pause_reason.unwrap().contains("tracked.txt"));
    }

    #[tokio::test]
    async fn wait_returns_on_finish_or_timeout() {
        let kit = Kit::new(&[("sleep", SLEEP), ("quick", QUICK)], |_| {}).await;
        let sleeping = kit.submit("sleep", "1").await;
        kit.until_status(&sleeping, RunStatus::Running).await;
        let started = Instant::now();
        let view = kit.hub.wait(&sleeping, 1).await.unwrap();
        assert_eq!(view.run.status, RunStatus::Running);
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_eq!(
            kit.hub.wait(&sleeping, 0).await.err().unwrap().code,
            "bad_request"
        );
        let quick = kit.submit("quick", "2").await;
        kit.hub.cancel(&sleeping, Source::Manual).await.unwrap();
        let started = Instant::now();
        let view = kit.hub.wait(&quick, 60).await.unwrap();
        assert_eq!(view.run.status, RunStatus::Succeeded);
        assert!(started.elapsed() < Duration::from_secs(20));
        assert_eq!(
            kit.hub.wait("j000000000000", 1).await.err().unwrap().code,
            "not_found"
        );
    }

    #[tokio::test]
    async fn logs_are_batched_in_order_and_capped() {
        let kit = Kit::new(&[("noisy", NOISY)], |_| {}).await;
        let id = kit.submit("noisy", "1").await;
        kit.finish(&id).await;
        let kept = kit.hub.logs(&id);
        assert_eq!(kept.len(), LOG_LINES);
        assert!(kept.iter().any(|line| line.text == "line 2500"));
        assert!(!kept.iter().any(|line| line.text == "line 1"));
        let sent: Vec<LogLine> = kit
            .records()
            .into_iter()
            .filter_map(|record| match record {
                Record::Logs(batch) if batch.node_id == id => Some(batch.lines),
                _ => None,
            })
            .flatten()
            .collect();
        assert!(sent.windows(2).all(|pair| pair[0].seq + 1 == pair[1].seq));
        let printed: Vec<_> = sent
            .iter()
            .filter(|line| line.stream == LogStream::Stdout)
            .collect();
        assert_eq!(printed.len(), 2500);
        assert_eq!(printed[0].text, "line 1");
        let tail = kit.hub.log_tail(&id, 3).unwrap();
        assert_eq!(tail.lines.len(), 3);
        assert!(tail.truncated);
        assert!(kit.hub.log_tail(&id, 201).is_err());
    }

    #[tokio::test]
    async fn snapshot_failure_stops_before_the_agent_runs() {
        let kit = Kit::new(&[("mark", MARK)], |_| {}).await;
        let repo = &kit.repo;
        assert!(git(repo, &["checkout", "--quiet", "-b", "other"]));
        std::fs::write(repo.join("tracked.txt"), "other\n").unwrap();
        assert!(git(repo, &["commit", "--quiet", "-am", "other"]));
        assert!(git(repo, &["checkout", "--quiet", "-"]));
        std::fs::write(repo.join("tracked.txt"), "main\n").unwrap();
        assert!(git(repo, &["commit", "--quiet", "-am", "main"]));
        assert!(!git(repo, &["merge", "other"]));
        let run = kit.finish(&kit.submit("mark", "1").await).await.run;
        assert_eq!(run.status, RunStatus::Failed);
        let error = run.error.unwrap();
        assert_eq!(error.kind, ErrorKind::Launch);
        assert!(
            error
                .message
                .starts_with("作業前の状態を記録できませんでした")
        );
        assert!(!repo.join("ran.txt").exists());
    }

    #[tokio::test]
    async fn restart_marks_unfinished_jobs_interrupted() {
        let kit = Kit::new(&[("quick", QUICK)], |_| {}).await;
        let done = kit.submit("quick", "1").await;
        let mut task = kit.finish(&done).await.task;
        let template = task.runs[0].clone();
        for (id, status) in [
            ("j00000000000a", RunStatus::Running),
            ("j00000000000b", RunStatus::Queued),
        ] {
            task.runs.push(RunRecord {
                id: id.into(),
                status,
                ended_at: None,
                ..template.clone()
            });
        }
        let tasks_dir = kit.root.join("tasks");
        store::save(&tasks_dir, &task).unwrap();
        let reopened = Orchestra::new(
            kit.root.join("config").join("agents.json"),
            kit.root.join("config").join("gateway.json"),
            tasks_dir.clone(),
            kit.sink.clone(),
        );
        let task = reopened.task(&task.id).unwrap();
        let statuses: Vec<_> = task
            .runs
            .iter()
            .map(|run| (run.status, run.error.as_ref().map(|e| e.kind)))
            .collect();
        assert_eq!(
            statuses,
            [
                (RunStatus::Succeeded, None),
                (RunStatus::Killed, Some(ErrorKind::Interrupted)),
                (RunStatus::Cancelled, Some(ErrorKind::Interrupted)),
            ]
        );
        assert!(task.runs.iter().all(|run| run.ended_at.is_some()));
        assert_eq!(
            store::load(&tasks_dir)[0].runs[2].status,
            RunStatus::Cancelled
        );
        assert!(reopened.gateway_settings().accepting);
    }
}
