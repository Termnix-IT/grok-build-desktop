use std::{
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    time::Duration,
};

use super::{
    adapter::AgentOutcome,
    config::{AgentSpec, OrchestratorSettings},
    decision::SCHEMA,
    engine::{LogStream, NodeStatus},
    git::StepDiff,
    runner::{self, Cancel, RunSpec},
};

pub const RULES: &str = "You are the orchestrator inside a desktop app. You plan and judge; CLI coding agents do the work.\nNever edit files or run commands yourself. You may read files in the project to plan.\nEvery reply must be only the JSON object required by the schema.\n- \"dispatch\": pick exactly one agent id from the roster as target_agent and write converted_prompt: a complete, self-contained instruction for that agent. The agent cannot see this conversation, so include the goal, the relevant files, constraints, and what done means. Set step_title to a short label such as 設計, 実装, テスト, レビュー.\n- \"complete\": only when the results show the goal is achieved. Put the outcome in summary.\n- \"ask_human\": when you need a decision or information only the user has, or when the same approach keeps failing. Put the question in question.\nAfter each dispatch you receive the agent's status, exit code, its final message, the tail of stderr and the git diff of that step. Judge the result yourself instead of trusting the agent's claims.\nNever ask an agent to commit, push, publish, deploy, delete branches, change git history, or use flags that skip permission checks.\nWrite reasoning, step_title, question and summary in the same language as the user's goal.";

pub struct PlanRequest {
    pub task_id: String,
    pub node_id: String,
    pub project: PathBuf,
    pub session: String,
    pub resume: bool,
    pub prompt: String,
    pub settings: OrchestratorSettings,
    pub cancel: Cancel,
}

pub type PlanFuture<'a> = Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>>;

pub trait Planner: Send + Sync {
    fn decide<'a>(
        &'a self,
        req: PlanRequest,
        log: &'a (dyn Fn(LogStream, String) + Send + Sync),
    ) -> PlanFuture<'a>;
}

pub struct GrokPlanner;

fn launch_args(req: &PlanRequest, prompt_file: &Path) -> Vec<String> {
    let mut args = vec![
        "--prompt-file".into(),
        prompt_file.to_string_lossy().into_owned(),
        "--json-schema".into(),
        SCHEMA.into(),
        "--permission-mode".into(),
        "plan".into(),
        "--no-subagents".into(),
        "--no-memory".into(),
        "--cwd".into(),
        req.project.to_string_lossy().into_owned(),
        "--rules".into(),
        RULES.into(),
        if req.resume { "-r".into() } else { "-s".into() },
        req.session.clone(),
    ];
    args.extend(req.settings.planner_args.iter().cloned());
    args
}

fn append_tail(buffer: &mut String, line: &str, limit: usize) {
    buffer.push_str(line);
    buffer.push('\n');
    if buffer.len() > limit {
        let mut start = buffer.len() - limit;
        while !buffer.is_char_boundary(start) {
            start += 1;
        }
        buffer.drain(..start);
    }
}

fn failure_message(stdout: &str, stderr: &str) -> String {
    fn error(value: serde_json::Value) -> Option<String> {
        (value.get("type").and_then(|v| v.as_str()) == Some("error"))
            .then(|| {
                value
                    .get("message")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned)
            })
            .flatten()
    }
    let message = serde_json::from_str(stdout)
        .ok()
        .and_then(error)
        .or_else(|| {
            stdout
                .lines()
                .rev()
                .filter_map(|line| serde_json::from_str(line).ok())
                .find_map(error)
        })
        .unwrap_or_else(|| {
            if stderr.trim().is_empty() {
                "司令塔の実行に失敗しました".into()
            } else {
                stderr.trim().into()
            }
        });
    let lower = message.to_lowercase();
    if lower.contains("not signed in") || lower.contains("not logged") {
        format!(
            "Grok にログインしていません。画面の「ログイン」から grok login を開いてください。\n{message}"
        )
    } else {
        message
    }
}

impl Planner for GrokPlanner {
    fn decide<'a>(
        &'a self,
        req: PlanRequest,
        log: &'a (dyn Fn(LogStream, String) + Send + Sync),
    ) -> PlanFuture<'a> {
        Box::pin(async move {
            let program = runner::resolve_program(&req.settings.planner_program)?;
            let prompt_file = std::env::temp_dir()
                .join("grok-build-desktop")
                .join(&req.task_id)
                .join(format!("{}.txt", req.node_id.replace(':', "_")));
            let args = launch_args(&req, &prompt_file);
            let displayed = args
                .iter()
                .map(|arg| {
                    if arg == RULES {
                        "<rules>".to_owned()
                    } else {
                        format!("{arg:?}")
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");
            log(
                LogStream::System,
                format!("起動: {} {displayed}", program.display()),
            );
            let mut stderr = String::new();
            let tail_limit = req.settings.log_tail_bytes;
            let result = runner::run_capture(
                RunSpec {
                    program,
                    args,
                    cwd: req.project,
                    stdin: None,
                    prompt_file: Some((prompt_file, req.prompt)),
                    timeout: Duration::from_secs(req.settings.planner_timeout_secs),
                },
                req.cancel,
                |stream, line| {
                    match stream {
                        LogStream::Stderr => append_tail(&mut stderr, &line, tail_limit),
                        LogStream::Stdout | LogStream::System => (),
                    }
                    log(stream, line);
                },
            )
            .await;
            let (exit, stdout) = match result {
                Ok(result) => result,
                Err(err) => {
                    log(LogStream::System, format!("起動に失敗しました: {err}"));
                    return Err(err);
                }
            };
            if exit.timed_out {
                log(LogStream::System, "時間切れで止めました".into());
                return Err("司令塔が時間切れで停止しました".into());
            }
            if exit.killed {
                log(LogStream::System, "強制終了しました".into());
                return Err("司令塔を強制終了しました".into());
            }
            log(
                LogStream::System,
                format!(
                    "終了コード {}",
                    exit.code
                        .map_or_else(|| "none".into(), |code| code.to_string())
                ),
            );
            if exit.code != Some(0) {
                Err(failure_message(&stdout, &stderr))
            } else {
                Ok(stdout)
            }
        })
    }
}

pub fn first_prompt(goal: &str, project: &Path, agents: &[&AgentSpec], max_turns: u32) -> String {
    let roster = agents
        .iter()
        .map(|agent| format!("- {} ({}): {}", agent.id, agent.name, agent.role))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "# Goal\n{goal}\n\n# Project\n{}\n\n# Agents\n{roster}\n\nYou have at most {max_turns} turns. Decide the first step.",
        project.display()
    )
}

pub fn result_prompt(
    step_no: usize,
    agent_id: &str,
    outcome: &AgentOutcome,
    exit_code: Option<i32>,
    diff: &StepDiff,
    turn: u32,
    max_turns: u32,
) -> String {
    let status = match outcome.status {
        NodeStatus::Running => "running",
        NodeStatus::Succeeded => "succeeded",
        NodeStatus::Failed => "failed",
        NodeStatus::Killed => "killed",
        NodeStatus::TimedOut => "timedOut",
    };
    let code = exit_code.map_or_else(|| "none".into(), |code| code.to_string());
    let stderr = if outcome.stderr_tail.trim().is_empty() {
        "(empty)"
    } else {
        &outcome.stderr_tail
    };
    let stat = if diff.stat.trim().is_empty() {
        "(no changes)"
    } else {
        &diff.stat
    };
    let truncated = if diff.truncated {
        "\n... (truncated)"
    } else {
        ""
    };
    let untracked = if diff.untracked.is_empty() {
        "(none)".into()
    } else {
        diff.untracked.join("\n")
    };
    format!(
        "# Result of step {step_no} ({agent_id})\nstatus: {status}, exit code: {code}\n\n## Agent's final message\n{}\n\n## stderr (tail)\n{stderr}\n\n## git diff --stat\n{stat}\n\n## git diff\n{}{truncated}\n\n## New untracked files\n{untracked}\n\nTurns used: {turn}/{max_turns}. Decide the next step.",
        outcome.summary, diff.patch
    )
}

pub fn reply_prompt(pending: Option<&str>, text: &str) -> String {
    let message = format!("# Message from the user\n{text}\n\nDecide the next step.");
    match pending {
        Some(pending) => format!("{pending}\n\n{message}"),
        None => message,
    }
}

pub fn repair_prompt(error: &str) -> String {
    format!(
        "Your previous reply could not be used: {error}. Reply again with only the JSON object required by the schema."
    )
}

#[cfg(test)]
mod tests {
    use super::super::config;
    use super::*;

    #[test]
    fn first_prompt_has_goal_project_and_only_the_given_roster() {
        let registry = config::defaults();
        let prompt = first_prompt(
            "機能を追加",
            Path::new("C:/project"),
            &[&registry.agents[0]],
            10,
        );
        assert!(prompt.starts_with("# Goal\n機能を追加\n\n# Project\nC:/project\n\n# Agents\n"));
        assert!(prompt.contains("- claude (Claude Code):"));
        assert!(!prompt.contains("- codex"));
        assert!(prompt.ends_with("You have at most 10 turns. Decide the first step."));
    }

    #[test]
    fn result_reply_and_repair_prompts_keep_the_evidence() {
        let mut outcome = AgentOutcome {
            status: NodeStatus::TimedOut,
            summary: "変更しました".into(),
            stderr_tail: "警告".into(),
            error_signature: None,
        };
        let mut diff = StepDiff {
            stat: "file.rs | 1 +".into(),
            patch: "+追加".into(),
            truncated: true,
            untracked: vec!["new.rs".into(), "next.rs".into()],
        };
        let prompt = result_prompt(3, "custom", &outcome, Some(3), &diff, 3, 10);
        for expected in [
            "step 3 (custom)",
            "status: timedOut, exit code: 3",
            "変更しました",
            "警告",
            "file.rs | 1 +",
            "+追加\n... (truncated)",
            "new.rs\nnext.rs",
            "Turns used: 3/10",
        ] {
            assert!(prompt.contains(expected), "{expected}");
        }
        assert_eq!(
            reply_prompt(Some(&prompt), "続けて"),
            format!("{prompt}\n\n# Message from the user\n続けて\n\nDecide the next step.")
        );
        assert_eq!(
            reply_prompt(None, "続けて"),
            "# Message from the user\n続けて\n\nDecide the next step."
        );
        assert!(repair_prompt("invalid").contains("could not be used: invalid."));
        outcome.stderr_tail.clear();
        diff.stat.clear();
        diff.untracked.clear();
        diff.truncated = false;
        let empty = result_prompt(1, "custom", &outcome, None, &diff, 1, 10);
        for expected in ["exit code: none", "(empty)", "(no changes)", "(none)"] {
            assert!(empty.contains(expected));
        }
    }

    #[test]
    fn command_uses_plan_mode_file_and_the_correct_session_flag() {
        let mut req = PlanRequest {
            task_id: "t1-test".into(),
            node_id: "t1-test-s1:plan".into(),
            project: PathBuf::from("C:/project"),
            session: "session-uuid".into(),
            resume: false,
            prompt: "SECRET PROMPT".into(),
            settings: config::defaults().orchestrator,
            cancel: Cancel::new(),
        };
        req.settings.planner_args = vec!["-m".into(), "grok-4".into()];
        let args = launch_args(&req, Path::new("C:/prompt.txt"));
        for expected in [
            ["--permission-mode", "plan"],
            ["-s", "session-uuid"],
            ["--json-schema", SCHEMA],
            ["--rules", RULES],
        ] {
            assert!(
                args.windows(2)
                    .any(|pair| pair[0] == expected[0] && pair[1] == expected[1])
            );
        }
        assert!(!args.iter().any(|arg| arg.contains("SECRET PROMPT")));
        assert!(!args.iter().any(|arg| arg == "--output-format"));
        assert_eq!(&args[args.len() - 2..], ["-m", "grok-4"]);
        req.resume = true;
        assert!(
            launch_args(&req, Path::new("C:/prompt.txt"))
                .windows(2)
                .any(|pair| pair == ["-r", "session-uuid"])
        );
    }

    #[test]
    fn failures_use_structured_message_and_explain_login() {
        assert_eq!(
            failure_message(r#"{"type":"error","message":"failure"}"#, "ignored"),
            "failure"
        );
        assert_eq!(failure_message("unreadable", "stderr\n"), "stderr");
        assert!(
            failure_message(r#"{"type":"error","message":"Not signed in"}"#, "")
                .starts_with("Grok にログインしていません。")
        );
        assert!(failure_message("", "not logged in").contains("grok login"));
        let mut tail = String::new();
        append_tail(&mut tail, "日本語123", 5);
        assert!(tail.len() <= 5);
        assert!(tail.ends_with("123\n"));
    }
}
