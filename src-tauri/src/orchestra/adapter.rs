use std::path::{Path, PathBuf};

use serde_json::Value;

use super::breaker;
use super::config::{self, AgentSpec, OutputFormat, PromptVia};
use super::engine::{LogStream, NodeStatus};
use super::runner;

pub struct DispatchRequest<'a> {
    pub cwd: &'a Path,
    pub prompt: &'a str,
    pub prompt_file: &'a Path,
}

pub struct LaunchPlan {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub stdin: Option<String>,
    pub prompt_file: Option<(PathBuf, String)>,
}

pub trait AgentAdapter: Send + Sync {
    fn spec(&self) -> &AgentSpec;
    fn launch(&self, req: &DispatchRequest) -> Result<LaunchPlan, String>;
    fn observe(&self, stream: LogStream, line: &str, acc: &mut Accumulator);
    fn outcome(&self, acc: Accumulator, exit: &ExitInfo) -> AgentOutcome;
}

pub struct ExitInfo {
    pub code: Option<i32>,
    pub killed: bool,
    pub timed_out: bool,
}

pub struct AgentOutcome {
    pub status: NodeStatus,
    pub summary: String,
    pub stderr_tail: String,
    pub error_signature: Option<String>,
}

#[derive(Default)]
pub struct Accumulator {
    stdout_tail: String,
    stderr_tail: String,
    message: Option<String>,
    result: Option<String>,
    failure: Option<String>,
    failed: bool,
    grok_message_id: Option<String>,
    grok_chunks: String,
    grok_json: String,
}

pub struct ConfiguredAdapter {
    spec: AgentSpec,
    log_tail_bytes: usize,
}

impl ConfiguredAdapter {
    pub fn new(spec: AgentSpec, log_tail_bytes: usize) -> Self {
        Self {
            spec,
            log_tail_bytes,
        }
    }
}

impl AgentAdapter for ConfiguredAdapter {
    fn spec(&self) -> &AgentSpec {
        &self.spec
    }

    fn launch(&self, req: &DispatchRequest) -> Result<LaunchPlan, String> {
        let program = runner::resolve_program(&self.spec.program)?;
        if !self.spec.allow_dangerous {
            if let Some(flag) = config::dangerous_flag(&self.spec.args) {
                return Err(format!(
                    "エージェント「{}」の危険なフラグ「{flag}」は許可されていません",
                    self.spec.id
                ));
            }
        }
        let batch = program
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("cmd") || ext.eq_ignore_ascii_case("bat"));
        if batch && self.spec.prompt_via == PromptVia::Arg {
            return Err(format!(
                "エージェント「{}」は .cmd/.bat にプロンプトを引数で渡せません。cmd.exe が引数を解釈し直すためです",
                self.spec.id
            ));
        }
        let prompt = (self.spec.prompt_via == PromptVia::Arg).then_some(req.prompt);
        let prompt_file = (self.spec.prompt_via == PromptVia::File).then_some(req.prompt_file);
        Ok(LaunchPlan {
            program,
            args: config::expand(&self.spec.args, prompt, prompt_file, req.cwd),
            cwd: req.cwd.to_owned(),
            stdin: (self.spec.prompt_via == PromptVia::Stdin).then(|| req.prompt.to_owned()),
            prompt_file: prompt_file.map(|path| (path.to_owned(), req.prompt.to_owned())),
        })
    }

    fn observe(&self, stream: LogStream, line: &str, acc: &mut Accumulator) {
        match stream {
            LogStream::Stderr => {
                append_line(&mut acc.stderr_tail, line);
                retain_tail_bytes(&mut acc.stderr_tail, self.log_tail_bytes);
                return;
            }
            LogStream::System => return,
            LogStream::Stdout => {}
        }
        append_line(&mut acc.stdout_tail, line);
        retain_tail_chars(&mut acc.stdout_tail, 2000);
        if self.spec.output == OutputFormat::GrokJson {
            append_line(&mut acc.grok_json, line);
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            return;
        };
        match self.spec.output {
            OutputFormat::Text => {}
            OutputFormat::ClaudeStreamJson => observe_claude(&value, acc),
            OutputFormat::CodexJsonl => observe_codex(&value, acc),
            OutputFormat::GrokJson => observe_grok(&value, acc),
        }
    }

    fn outcome(&self, mut acc: Accumulator, exit: &ExitInfo) -> AgentOutcome {
        if self.spec.output == OutputFormat::GrokJson
            && acc.message.is_none()
            && acc.result.is_none()
        {
            if let Ok(value) = serde_json::from_str::<Value>(&acc.grok_json) {
                observe_grok(&value, &mut acc);
            }
        }
        let status = if exit.timed_out {
            NodeStatus::TimedOut
        } else if exit.killed {
            NodeStatus::Killed
        } else if exit.code != Some(0) || acc.failed {
            NodeStatus::Failed
        } else {
            NodeStatus::Succeeded
        };
        let mut summary = acc
            .result
            .or(acc.message)
            .filter(|text| !text.is_empty())
            .or_else(|| acc.failure.clone())
            .unwrap_or_else(|| acc.stdout_tail.trim_end_matches(['\r', '\n']).to_owned());
        if let Some(error) = acc.failure {
            summary = if summary.is_empty() || summary == error {
                error
            } else {
                format!("{error}\n{summary}")
            };
        }
        let stderr_tail = acc.stderr_tail.trim_end_matches(['\r', '\n']).to_owned();
        let error_signature = (status != NodeStatus::Succeeded).then(|| {
            breaker::error_signature(
                &self.spec.id,
                exit.code,
                &format!("{summary}\n{stderr_tail}"),
            )
        });
        AgentOutcome {
            status,
            summary,
            stderr_tail,
            error_signature,
        }
    }
}

fn append_line(target: &mut String, line: &str) {
    if !target.is_empty() {
        target.push('\n');
    }
    target.push_str(line);
}

fn retain_tail_chars(text: &mut String, limit: usize) {
    let count = text.chars().count();
    if count > limit {
        let offset = text
            .char_indices()
            .nth(count - limit)
            .map(|(index, _)| index)
            .unwrap_or(text.len());
        text.drain(..offset);
    }
}

fn retain_tail_bytes(text: &mut String, limit: usize) {
    if text.len() > limit {
        let mut offset = text.len() - limit;
        while !text.is_char_boundary(offset) {
            offset += 1;
        }
        text.drain(..offset);
    }
}

fn observe_claude(value: &Value, acc: &mut Accumulator) {
    match value.get("type").and_then(Value::as_str) {
        Some("result") => {
            if let Some(text) = value.get("result").and_then(Value::as_str) {
                acc.result = Some(text.into());
            }
            if value.get("is_error").and_then(Value::as_bool) == Some(true)
                || value.get("subtype").and_then(Value::as_str) != Some("success")
            {
                acc.failed = true;
            }
        }
        Some("assistant") => {
            if let Some(content) = value.pointer("/message/content").and_then(Value::as_array) {
                let texts: Vec<_> = content
                    .iter()
                    .filter(|item| item.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|item| item.get("text").and_then(Value::as_str))
                    .collect();
                if !texts.is_empty() {
                    acc.message = Some(texts.join(""));
                }
            }
        }
        _ => {}
    }
}

fn observe_codex(value: &Value, acc: &mut Accumulator) {
    match value.get("type").and_then(Value::as_str) {
        Some("item.completed") => {
            let kind = value.pointer("/item/type").and_then(Value::as_str);
            if matches!(kind, Some("agent_message" | "assistant_message")) {
                if let Some(text) = value.pointer("/item/text").and_then(Value::as_str) {
                    acc.message = Some(text.into());
                }
            }
        }
        Some("turn.failed") => {
            acc.failed = true;
            acc.failure = value
                .pointer("/error/message")
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
        Some("error") => {
            acc.failed = true;
            acc.failure = value
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
        _ => {}
    }
}

fn observe_grok(value: &Value, acc: &mut Accumulator) {
    if value.get("type").and_then(Value::as_str) == Some("error") {
        acc.failed = true;
        acc.failure = value
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_owned);
        return;
    }
    let update = value.get("update").unwrap_or(value);
    let kind = update
        .get("sessionUpdate")
        .or_else(|| update.get("session_update"))
        .and_then(Value::as_str);
    if kind == Some("agent_message_chunk") {
        if let Some(text) = update.pointer("/content/text").and_then(Value::as_str) {
            let id = update
                .get("messageId")
                .or_else(|| value.get("messageId"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            if id != acc.grok_message_id {
                acc.grok_chunks.clear();
                acc.grok_message_id = id;
            }
            acc.grok_chunks.push_str(text);
            acc.message = Some(acc.grok_chunks.clone());
        }
    } else if let Some(text) = value
        .get("result")
        .or_else(|| value.get("text"))
        .and_then(Value::as_str)
    {
        acc.result = Some(text.into());
    }
}

#[cfg(test)]
mod tests {
    use super::super::config::AgentKind;
    use super::*;

    fn spec(output: OutputFormat) -> AgentSpec {
        AgentSpec {
            id: "fake".into(),
            name: "Fake".into(),
            kind: AgentKind::Custom,
            enabled: true,
            program: "unused".into(),
            args: vec![],
            prompt_via: PromptVia::Stdin,
            output,
            role: String::new(),
            allow_dangerous: false,
        }
    }

    fn parsed(output: OutputFormat, lines: &[&str], code: i32) -> AgentOutcome {
        let adapter = ConfiguredAdapter::new(spec(output), 4096);
        let mut acc = Accumulator::default();
        for line in lines {
            adapter.observe(LogStream::Stdout, line, &mut acc);
        }
        adapter.outcome(
            acc,
            &ExitInfo {
                code: Some(code),
                killed: false,
                timed_out: false,
            },
        )
    }

    #[test]
    fn text_tails_and_exit_status() {
        let adapter = ConfiguredAdapter::new(spec(OutputFormat::Text), 7);
        assert_eq!(adapter.spec().id, "fake");
        let mut acc = Accumulator::default();
        adapter.observe(LogStream::Stdout, &"あ".repeat(2100), &mut acc);
        adapter.observe(LogStream::Stderr, "先頭末尾", &mut acc);
        adapter.observe(LogStream::System, "ignored", &mut acc);
        let result = adapter.outcome(
            acc,
            &ExitInfo {
                code: Some(0),
                killed: false,
                timed_out: false,
            },
        );
        assert_eq!(result.status, NodeStatus::Succeeded);
        assert_eq!(result.summary.chars().count(), 2000);
        assert_eq!(result.stderr_tail, "末尾");
        assert!(result.error_signature.is_none());
        assert_eq!(
            parsed(OutputFormat::Text, &["failed"], 3).status,
            NodeStatus::Failed
        );
        for (killed, timed_out, expected) in [
            (true, false, NodeStatus::Killed),
            (true, true, NodeStatus::TimedOut),
        ] {
            let result = adapter.outcome(
                Accumulator::default(),
                &ExitInfo {
                    code: Some(0),
                    killed,
                    timed_out,
                },
            );
            assert_eq!(result.status, expected);
            assert!(result.error_signature.is_some());
        }
    }

    #[test]
    fn claude_result_fallback_and_error() {
        let lines = [
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"途中"},{"type":"text","text":"の返事"}]}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"完了"}"#,
        ];
        let result = parsed(OutputFormat::ClaudeStreamJson, &lines, 0);
        assert_eq!(result.status, NodeStatus::Succeeded);
        assert_eq!(result.summary, "完了");
        assert_eq!(
            parsed(OutputFormat::ClaudeStreamJson, &lines[..1], 0).summary,
            "途中の返事"
        );
        for line in [
            r#"{"type":"result","subtype":"error_max_turns","result":"上限"}"#,
            r#"{"type":"result","subtype":"success","is_error":true,"result":"失敗"}"#,
        ] {
            assert_eq!(
                parsed(OutputFormat::ClaudeStreamJson, &[line], 0).status,
                NodeStatus::Failed
            );
        }
    }

    #[test]
    fn codex_messages_and_failures() {
        let lines = [
            r#"{"type":"item.completed","item":{"type":"assistant_message","text":"古い形"}}"#,
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"最後の返事"}}"#,
        ];
        let result = parsed(OutputFormat::CodexJsonl, &lines, 0);
        assert_eq!(result.status, NodeStatus::Succeeded);
        assert_eq!(result.summary, "最後の返事");
        for error in [
            r#"{"type":"turn.failed","error":{"message":"失敗しました"}}"#,
            r#"{"type":"error","message":"失敗しました"}"#,
        ] {
            let result = parsed(OutputFormat::CodexJsonl, &[lines[1], error], 0);
            assert_eq!(result.status, NodeStatus::Failed);
            assert_eq!(result.summary, "失敗しました\n最後の返事");
        }
    }

    #[test]
    fn grok_chunks_message_ids_whole_json_and_errors() {
        let lines = [
            r#"{"sessionUpdate":"agent_message_chunk","messageId":"one","content":{"text":"前の返事"}}"#,
            r#"{"update":{"session_update":"agent_message_chunk","messageId":"two","content":{"text":"最後の"}}}"#,
            r#"{"update":{"sessionUpdate":"agent_message_chunk","messageId":"two","content":{"text":"返事"}}}"#,
        ];
        let result = parsed(OutputFormat::GrokJson, &lines, 0);
        assert_eq!(result.status, NodeStatus::Succeeded);
        assert_eq!(result.summary, "最後の返事");
        assert_eq!(
            parsed(
                OutputFormat::GrokJson,
                &["{", r#""result": "複数行のJSON""#, "}"],
                0
            )
            .summary,
            "複数行のJSON"
        );
        assert_eq!(
            parsed(OutputFormat::GrokJson, &[r#"{"text":"text field"}"#], 0).summary,
            "text field"
        );
        let result = parsed(
            OutputFormat::GrokJson,
            &[r#"{"type":"error","message":"失敗"}"#],
            0,
        );
        assert_eq!(result.status, NodeStatus::Failed);
        assert!(result.summary.starts_with("失敗"));
    }

    #[test]
    fn invalid_json_falls_back_to_text() {
        for format in [
            OutputFormat::ClaudeStreamJson,
            OutputFormat::CodexJsonl,
            OutputFormat::GrokJson,
        ] {
            assert_eq!(
                parsed(format, &["not json", "最後"], 0).summary,
                "not json\n最後"
            );
        }
    }

    #[test]
    fn launch_modes_and_runtime_validation() {
        let path = std::env::temp_dir().join(format!("gbd-adapter-{}.exe", uuid::Uuid::new_v4()));
        std::fs::write(&path, "").unwrap();
        let cwd = path.parent().unwrap();
        let prompt_file = cwd.join("prompt.txt");
        let req = DispatchRequest {
            cwd,
            prompt: "こんにちは & < >",
            prompt_file: &prompt_file,
        };
        let mut agent = spec(OutputFormat::Text);
        agent.program = path.to_string_lossy().into_owned();
        for (via, placeholder) in [
            (PromptVia::Stdin, "{cwd}"),
            (PromptVia::File, "{promptFile}"),
            (PromptVia::Arg, "{prompt}"),
        ] {
            agent.prompt_via = via;
            agent.args = vec![placeholder.into()];
            let plan = ConfiguredAdapter::new(agent.clone(), 4096)
                .launch(&req)
                .unwrap();
            assert_eq!(plan.cwd, cwd);
            match via {
                PromptVia::Stdin => assert_eq!(plan.stdin.as_deref(), Some(req.prompt)),
                PromptVia::File => assert_eq!(
                    plan.prompt_file,
                    Some((prompt_file.clone(), req.prompt.into()))
                ),
                PromptVia::Arg => assert_eq!(plan.args, vec![req.prompt]),
            }
        }
        agent.prompt_via = PromptVia::Stdin;
        agent.args = vec!["--yolo".into()];
        assert!(
            ConfiguredAdapter::new(agent.clone(), 4096)
                .launch(&req)
                .is_err()
        );
        agent.allow_dangerous = true;
        assert!(
            ConfiguredAdapter::new(agent.clone(), 4096)
                .launch(&req)
                .is_ok()
        );
        let batch_path = path.with_extension("cmd");
        std::fs::write(&batch_path, "").unwrap();
        agent.program = batch_path.to_string_lossy().into_owned();
        agent.prompt_via = PromptVia::Arg;
        agent.args = vec!["{prompt}".into()];
        assert!(
            ConfiguredAdapter::new(agent, 4096)
                .launch(&req)
                .err()
                .unwrap()
                .contains("cmd.exe")
        );
        std::fs::remove_file(path).unwrap();
        std::fs::remove_file(batch_path).unwrap();
    }
}
