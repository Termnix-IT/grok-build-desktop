use std::{
    borrow::Cow,
    path::{Path, PathBuf},
};

use serde_json::Value;

use super::config::{self, AgentSpec, OutputFormat, PromptVia};
use super::engine::{LogStream, RunStatus, head};
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
    /// `line` is the whole output line (up to the runner's frame limit), not the shortened log text.
    fn observe(&self, stream: LogStream, line: &str, acc: &mut Accumulator);
    fn outcome(&self, acc: Accumulator, exit: &ExitInfo) -> AgentOutcome;
}

pub struct ExitInfo {
    pub code: Option<i32>,
    pub killed: bool,
    pub timed_out: bool,
}

pub struct AgentOutcome {
    pub status: RunStatus,
    pub summary: String,
    pub stderr_tail: String,
    /// Actions the agent wanted but the headless CLI refused.
    pub denied: Vec<String>,
    /// The run failed and the CLI reported refused actions, so the refusal is the likely cause.
    pub denied_failure: bool,
}

const WHOLE_LIMIT: usize = 1024 * 1024;
const CHUNK_LIMIT: usize = 16000;
const DENIED_LIMIT: usize = 20;
const DENIED_CHARS: usize = 1000;
const GROK_DENIED_PREFIX: &str = "permission required: ";

#[derive(Default)]
pub struct Accumulator {
    stdout_tail: String,
    stderr_tail: String,
    message: Option<String>,
    result: Option<String>,
    failure: Option<String>,
    failed: bool,
    denied: Vec<String>,
    overlong: bool,
    grok_message_id: Option<String>,
    grok_chunks: String,
    /// Counts tool calls in Grok's streaming JSON; text after the last one is the answer.
    grok_segment: u32,
    /// Raw stdout for formats that may print one JSON value over several lines.
    whole: String,
}

impl Accumulator {
    /// The runner dropped a line that was too long to parse.
    pub fn note_overlong(&mut self) {
        self.overlong = true;
    }

    fn deny(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() || self.denied.len() >= DENIED_LIMIT {
            return;
        }
        let entry: String = if text.chars().count() > DENIED_CHARS {
            let mut short: String = text.chars().take(DENIED_CHARS - 1).collect();
            short.push('…');
            short
        } else {
            text.into()
        };
        if !self.denied.contains(&entry) {
            self.denied.push(entry);
        }
    }
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

    fn keeps_whole(&self) -> bool {
        matches!(
            self.spec.output,
            OutputFormat::GrokJson | OutputFormat::AntigravityJson
        )
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
        let prompt = if self.spec.prompt_prefix.is_empty() {
            Cow::Borrowed(req.prompt)
        } else {
            Cow::Owned(format!("{}\n\n{}", self.spec.prompt_prefix, req.prompt))
        };
        let prompt_arg = (self.spec.prompt_via == PromptVia::Arg).then_some(prompt.as_ref());
        let prompt_file = (self.spec.prompt_via == PromptVia::File).then_some(req.prompt_file);
        Ok(LaunchPlan {
            program,
            args: config::expand(&self.spec.args, prompt_arg, prompt_file, req.cwd),
            cwd: req.cwd.to_owned(),
            stdin: (self.spec.prompt_via == PromptVia::Stdin).then(|| prompt.as_ref().to_owned()),
            prompt_file: prompt_file.map(|path| (path.to_owned(), prompt.into_owned())),
        })
    }

    fn observe(&self, stream: LogStream, line: &str, acc: &mut Accumulator) {
        match stream {
            LogStream::Stderr => {
                if self.spec.output == OutputFormat::GrokJson
                    && let Some(action) = line.trim_start().strip_prefix(GROK_DENIED_PREFIX)
                {
                    acc.deny(action);
                }
                append_line(&mut acc.stderr_tail, line);
                retain_tail_bytes(&mut acc.stderr_tail, self.log_tail_bytes);
                return;
            }
            LogStream::System => return,
            LogStream::Stdout => {}
        }
        append_line(&mut acc.stdout_tail, line);
        retain_tail_chars(&mut acc.stdout_tail, 2000);
        if self.keeps_whole() {
            // The result is the last JSON value, so the end of the output is kept. Cutting only
            // at twice the limit keeps a long output from being copied on every line.
            append_line(&mut acc.whole, line);
            if acc.whole.len() > 2 * WHOLE_LIMIT {
                retain_tail_bytes(&mut acc.whole, WHOLE_LIMIT);
            }
        }
        if self.spec.output == OutputFormat::AntigravityJson {
            // Parsed once at the end: the JSON may span lines and follow warning lines.
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            return;
        };
        match self.spec.output {
            OutputFormat::Text | OutputFormat::AntigravityJson => {}
            OutputFormat::ClaudeStreamJson => observe_claude(&value, acc),
            OutputFormat::CodexJsonl => observe_codex(&value, acc),
            OutputFormat::GrokJson => observe_grok(&value, acc),
        }
    }

    fn outcome(&self, mut acc: Accumulator, exit: &ExitInfo) -> AgentOutcome {
        retain_tail_bytes(&mut acc.whole, WHOLE_LIMIT);
        match self.spec.output {
            // One JSON value printed over several lines; streamed chunks were already read.
            OutputFormat::GrokJson
                if acc.message.is_none() && acc.result.is_none() && acc.grok_chunks.is_empty() =>
            {
                if let Some(value) = last_json_object(&acc.whole) {
                    observe_grok(&value, &mut acc);
                }
            }
            OutputFormat::AntigravityJson => {
                if let Some(value) = last_json_object(&acc.whole) {
                    observe_antigravity(&value, &mut acc);
                } else {
                    // Without the result its status and refused actions are unknown.
                    acc.failed = true;
                    acc.failure = Some(format!(
                        "Antigravity の結果の JSON を読み取れませんでした\n{}",
                        acc.stdout_tail.trim_end_matches(['\r', '\n'])
                    ));
                }
            }
            _ => {}
        }
        if acc.message.is_none() && !acc.grok_chunks.is_empty() {
            acc.message = Some(std::mem::take(&mut acc.grok_chunks));
        }
        let status = if exit.timed_out {
            RunStatus::TimedOut
        } else if exit.killed {
            RunStatus::Killed
        } else if exit.code != Some(0) || acc.failed {
            RunStatus::Failed
        } else {
            RunStatus::Succeeded
        };
        let answer = acc
            .result
            .take()
            .or(acc.message.take())
            .filter(|text| !text.trim().is_empty());
        let mut summary = match (&answer, &acc.failure) {
            (Some(answer), _) => answer.clone(),
            (None, Some(failure)) => failure.clone(),
            (None, None) if !acc.denied.is_empty() => format!(
                "ヘッドレスの CLI が操作を拒否しました: {}",
                acc.denied.join("、")
            ),
            (None, None) if acc.overlong => "出力の行が長すぎて読み取れませんでした".into(),
            (None, None) if self.spec.output == OutputFormat::GrokJson && acc.grok_segment > 0 => {
                "Grok はツールを使ったあと、最後に文章を返しませんでした".into()
            }
            (None, None) => acc.stdout_tail.trim_end_matches(['\r', '\n']).to_owned(),
        };
        if let (Some(_), Some(failure)) = (&answer, &acc.failure)
            && summary != *failure
        {
            summary = format!("{failure}\n{summary}");
        }
        let stderr_tail = acc.stderr_tail.trim_end_matches(['\r', '\n']).to_owned();
        AgentOutcome {
            denied_failure: status == RunStatus::Failed && !acc.denied.is_empty(),
            status,
            summary,
            stderr_tail,
            denied: acc.denied,
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

/// The last top-level JSON object in `text`. A top-level object starts at the beginning of a
/// line, so warning lines printed before it and indented nested objects are skipped.
fn last_json_object(text: &str) -> Option<Value> {
    let starts = text
        .match_indices('{')
        .map(|(index, _)| index)
        .filter(|&index| index == 0 || text.as_bytes()[index - 1] == b'\n');
    let candidates: Vec<usize> = starts.collect();
    candidates.into_iter().rev().find_map(|start| {
        serde_json::from_str::<Value>(text[start..].trim_end())
            .ok()
            .filter(Value::is_object)
    })
}

fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

const BRIEF_CHARS: usize = 1000;

/// Produces readable stdout independently of the adapter's final result.
pub struct Brief {
    output: OutputFormat,
    grok_text: String,
    antigravity_pending: String,
    antigravity_result: Option<Value>,
}

impl Brief {
    pub fn new(output: OutputFormat) -> Self {
        Self {
            output,
            grok_text: String::new(),
            antigravity_pending: String::new(),
            antigravity_result: None,
        }
    }

    pub fn feed(&mut self, stream: LogStream, text: &str) -> Vec<String> {
        match stream {
            LogStream::Stderr => return vec![head(&format!("[stderr] {text}"), BRIEF_CHARS)],
            LogStream::System => return vec![brief_line("[system]", text)],
            LogStream::Stdout => {}
        }
        match self.output {
            OutputFormat::Text => vec![head(text, BRIEF_CHARS)],
            OutputFormat::AntigravityJson => self.feed_antigravity(text),
            format => {
                let value = serde_json::from_str::<Value>(text);
                match (format, value) {
                    (OutputFormat::ClaudeStreamJson, Ok(value)) => brief_claude(&value),
                    (OutputFormat::CodexJsonl, Ok(value)) => brief_codex(&value),
                    (OutputFormat::GrokJson, Ok(value)) => self.feed_grok(&value),
                    (_, Err(_)) => {
                        let mut lines = self.take_grok_text();
                        lines.push(head(text, BRIEF_CHARS));
                        lines
                    }
                    _ => Vec::new(),
                }
            }
        }
    }

    pub fn finish(&mut self) -> Vec<String> {
        let mut lines = self.take_grok_text();
        lines.extend(self.take_antigravity_pending());
        if let Some(value) = self.antigravity_result.take() {
            lines.push(brief_line(
                "[結果]",
                str_field(&value, "response").unwrap_or(""),
            ));
            lines.extend(
                entries(&value, "denied_actions")
                    .map(|action| brief_line("[拒否]", &antigravity_denial(action))),
            );
        }
        lines
    }

    fn take_grok_text(&mut self) -> Vec<String> {
        let text = std::mem::take(&mut self.grok_text);
        if text.is_empty() {
            return Vec::new();
        }
        vec![brief_line("[文章]", &text)]
    }

    fn feed_grok(&mut self, value: &Value) -> Vec<String> {
        let kind = str_field(value, "type");
        if kind == Some("text") {
            let room = BRIEF_CHARS.saturating_sub(self.grok_text.chars().count());
            if let Some(text) = str_field(value, "data") {
                self.grok_text.extend(text.chars().take(room));
            }
            return Vec::new();
        }
        let mut lines = self.take_grok_text();
        match kind {
            Some("tool_call") => {
                let name = str_field(value, "title")
                    .or_else(|| str_field(value, "toolName"))
                    .unwrap_or("?");
                let input = value.get("rawInput").unwrap_or(&Value::Null);
                let parsed = input
                    .as_str()
                    .and_then(|text| serde_json::from_str(text).ok());
                let input = parsed.as_ref().unwrap_or(input);
                let path = input_detail(input, &["path", "file_path", "filePath"]);
                lines.push(brief_line("[ツール]", &format!("{name} {path}")));
            }
            Some("tool_call_update") if str_field(value, "status") == Some("failed") => {
                lines.push(brief_line(
                    "[エラー]",
                    &error_detail(value, "tool_call failed"),
                ));
            }
            Some("error") => lines.push(brief_line("[エラー]", &error_detail(value, "error"))),
            Some("end") => {
                lines.push(brief_line(
                    "[終了]",
                    str_field(value, "stopReason").unwrap_or(""),
                ));
            }
            _ => {}
        }
        lines
    }

    fn take_antigravity_pending(&mut self) -> Vec<String> {
        std::mem::take(&mut self.antigravity_pending)
            .lines()
            .map(|text| head(text, BRIEF_CHARS))
            .collect()
    }

    fn feed_antigravity(&mut self, text: &str) -> Vec<String> {
        if self.antigravity_pending.is_empty() && !text.trim_start().starts_with('{') {
            return if serde_json::from_str::<Value>(text).is_ok() {
                Vec::new()
            } else {
                vec![head(text, BRIEF_CHARS)]
            };
        }
        append_line(&mut self.antigravity_pending, text);
        if self.antigravity_pending.len() > WHOLE_LIMIT {
            return self.take_antigravity_pending();
        }
        // Pretty-printed JSON ends with an unindented brace. Also handle a complete
        // object on one line and fall back to the original text for malformed output.
        if !text.trim_end().ends_with('}') && self.antigravity_pending.contains('\n') {
            return Vec::new();
        }
        match serde_json::from_str::<Value>(&self.antigravity_pending) {
            Ok(value) => {
                self.antigravity_pending.clear();
                if value.get("response").is_some()
                    || value.get("denied_actions").is_some()
                    || value.get("status").is_some()
                {
                    self.antigravity_result = Some(value);
                }
                Vec::new()
            }
            Err(error) if error.is_eof() => Vec::new(),
            Err(_) => self.take_antigravity_pending(),
        }
    }
}

fn brief_line(label: &str, text: &str) -> String {
    let text = text.replace(['\r', '\n'], " ");
    head(&format!("{label} {}", text.trim()), BRIEF_CHARS)
        .trim_end()
        .to_owned()
}

fn entries<'a>(value: &'a Value, key: &str) -> impl Iterator<Item = &'a Value> {
    value
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

fn input_detail<'a>(input: &'a Value, keys: &[&str]) -> &'a str {
    keys.iter()
        .find_map(|key| str_field(input, key))
        .unwrap_or("")
}

fn error_detail(value: &Value, fallback: &str) -> String {
    value
        .pointer("/error/message")
        .and_then(Value::as_str)
        .or_else(|| str_field(value, "message"))
        .or_else(|| str_field(value, "error"))
        .or_else(|| str_field(value, "text"))
        .or_else(|| str_field(value, "title"))
        .or_else(|| str_field(value, "toolCallId"))
        .unwrap_or(fallback)
        .to_owned()
}

fn brief_claude(value: &Value) -> Vec<String> {
    match (str_field(value, "type"), str_field(value, "subtype")) {
        (Some("system"), Some("init")) => vec![brief_line(
            "[開始]",
            str_field(value, "model").unwrap_or(""),
        )],
        (Some("result"), _) => {
            let mut lines = vec![brief_line(
                "[結果]",
                str_field(value, "result").unwrap_or(""),
            )];
            lines.extend(
                entries(value, "permission_denials")
                    .map(|denial| brief_line("[拒否]", &claude_denial(denial))),
            );
            lines
        }
        (Some("assistant" | "user"), _) => value
            .get("message")
            .into_iter()
            .flat_map(|message| entries(message, "content"))
            .filter_map(|item| match str_field(item, "type") {
                Some("text") if str_field(value, "type") == Some("assistant") => {
                    Some(brief_line("[文章]", str_field(item, "text")?))
                }
                Some("tool_use") => {
                    let name = str_field(item, "name").unwrap_or("?");
                    let detail = input_detail(
                        &item["input"],
                        &["file_path", "path", "command", "pattern", "url"],
                    );
                    Some(brief_line("[ツール]", &format!("{name} {detail}")))
                }
                Some("tool_result") if item["is_error"].as_bool() == Some(true) => {
                    let detail = str_field(item, "content")
                        .map(str::to_owned)
                        .unwrap_or_else(|| {
                            entries(item, "content")
                                .filter_map(|entry| str_field(entry, "text"))
                                .collect::<Vec<_>>()
                                .join(" ")
                        });
                    Some(brief_line("[エラー]", &detail))
                }
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn brief_codex(value: &Value) -> Vec<String> {
    let item = &value["item"];
    let line = match str_field(value, "type") {
        Some("turn.completed") => "[完了]".into(),
        Some(kind @ ("turn.failed" | "error")) => {
            brief_line("[エラー]", &error_detail(value, kind))
        }
        Some("item.started" | "item.completed") => match str_field(item, "type") {
            Some("agent_message" | "assistant_message") => {
                brief_line("[文章]", str_field(item, "text").unwrap_or(""))
            }
            Some("command_execution") => {
                let command = str_field(item, "command").unwrap_or("");
                let detail = item["exit_code"].as_i64().map_or_else(
                    || command.to_owned(),
                    |code| format!("{command} (exit {code})"),
                );
                brief_line("[コマンド]", &detail)
            }
            Some("file_change") => {
                let paths = entries(item, "changes")
                    .filter_map(|change| str_field(change, "path"))
                    .collect::<Vec<_>>()
                    .join(", ");
                brief_line("[変更]", &paths)
            }
            Some("mcp_tool_call") => {
                let server = str_field(item, "server").unwrap_or("?");
                let tool = str_field(item, "tool").unwrap_or("?");
                brief_line("[ツール]", &format!("{server}.{tool}"))
            }
            Some("error") => brief_line("[エラー]", &error_detail(item, "error")),
            _ => return Vec::new(),
        },
        _ => return Vec::new(),
    };
    vec![line]
}

fn claude_denial(denial: &Value) -> String {
    let tool = str_field(denial, "tool_name").unwrap_or("?");
    let input = denial.get("tool_input");
    let detail = input
        .and_then(|input| str_field(input, "command").or_else(|| str_field(input, "file_path")))
        .map(str::to_owned)
        .or_else(|| input.map(Value::to_string))
        .unwrap_or_default();
    format!("{tool}: {detail}")
}

fn antigravity_denial(action: &Value) -> String {
    let name = str_field(action, "display_name").unwrap_or("?");
    match str_field(action, "action") {
        Some(kind) => format!("{name} ({kind})"),
        None => name.to_owned(),
    }
}

fn observe_claude(value: &Value, acc: &mut Accumulator) {
    match str_field(value, "type") {
        Some("result") => {
            if let Some(text) = str_field(value, "result") {
                acc.result = Some(text.into());
            }
            if value.get("is_error").and_then(Value::as_bool) == Some(true)
                || str_field(value, "subtype") != Some("success")
            {
                acc.failed = true;
            }
            for denial in entries(value, "permission_denials") {
                acc.deny(&claude_denial(denial));
            }
        }
        Some("assistant") => {
            if let Some(content) = value.pointer("/message/content").and_then(Value::as_array) {
                let texts: Vec<_> = content
                    .iter()
                    .filter(|item| str_field(item, "type") == Some("text"))
                    .filter_map(|item| str_field(item, "text"))
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
    match str_field(value, "type") {
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
            acc.failure = str_field(value, "message").map(str::to_owned);
        }
        _ => {}
    }
}

fn observe_grok(value: &Value, acc: &mut Accumulator) {
    if str_field(value, "type") == Some("error") {
        acc.failed = true;
        acc.failure = str_field(value, "message").map(str::to_owned);
        return;
    }
    let update = value.get("update").unwrap_or(value);
    if [value, update]
        .iter()
        .any(|item| str_field(item, "stopReason") == Some("cancelled"))
    {
        // A headless run stops with "cancelled" when it needed an approval it could not ask for.
        acc.failed = true;
    }
    // `--output-format streaming-json`: {"type":"text","data":"…"} pieces, with tool calls
    // between the turns, then {"type":"end","stopReason":…}.
    match str_field(value, "type") {
        Some("text") => {
            if let Some(text) = str_field(value, "data") {
                let id = Some(format!("streaming-{}", acc.grok_segment));
                push_grok_chunk(acc, id, text);
            }
            return;
        }
        Some("tool_call") => {
            // Text written before a tool call is not the answer, even if nothing follows.
            acc.grok_segment += 1;
            acc.grok_chunks.clear();
            acc.grok_message_id = None;
            return;
        }
        // An update may arrive late, between pieces of the answer, so it starts no new segment.
        Some("tool_call_update") => return,
        Some("thought" | "usage" | "available_commands" | "end") => return,
        _ => {}
    }
    let kind = str_field(update, "sessionUpdate").or_else(|| str_field(update, "session_update"));
    if kind == Some("agent_message_chunk") {
        if let Some(text) = update.pointer("/content/text").and_then(Value::as_str) {
            let id = str_field(update, "messageId")
                .or_else(|| str_field(value, "messageId"))
                .map(str::to_owned);
            push_grok_chunk(acc, id, text);
        }
    } else if let Some(text) = str_field(value, "result").or_else(|| str_field(value, "text")) {
        acc.result = Some(text.into());
    }
}

/// A chunk of a new message replaces the chunks of the one before it.
fn push_grok_chunk(acc: &mut Accumulator, id: Option<String>, text: &str) {
    if id != acc.grok_message_id {
        acc.grok_chunks.clear();
        acc.grok_message_id = id;
    }
    let room = CHUNK_LIMIT.saturating_sub(acc.grok_chunks.chars().count());
    acc.grok_chunks.extend(text.chars().take(room));
}

fn observe_antigravity(value: &Value, acc: &mut Accumulator) {
    if let Some(text) = str_field(value, "response") {
        acc.result = Some(text.trim_end_matches(['\r', '\n']).into());
    }
    let status = str_field(value, "status").unwrap_or_default();
    if status != "SUCCESS" {
        acc.failed = true;
        acc.failure = str_field(value, "error")
            .or_else(|| str_field(value, "message"))
            .map(str::to_owned)
            .or_else(|| Some(format!("Antigravity の状態: {status}")));
    }
    for action in entries(value, "denied_actions") {
        acc.deny(&antigravity_denial(action));
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
            prompt_prefix: String::new(),
            output,
            role: String::new(),
            allow_dangerous: false,
            timeout_secs: None,
        }
    }

    fn exit(code: i32) -> ExitInfo {
        ExitInfo {
            code: Some(code),
            killed: false,
            timed_out: false,
        }
    }

    fn run(output: OutputFormat, lines: &[(LogStream, &str)], code: i32) -> AgentOutcome {
        let adapter = ConfiguredAdapter::new(spec(output), 4096);
        let mut acc = Accumulator::default();
        for (stream, line) in lines {
            adapter.observe(*stream, line, &mut acc);
        }
        adapter.outcome(acc, &exit(code))
    }

    fn parsed(output: OutputFormat, lines: &[&str], code: i32) -> AgentOutcome {
        let lines: Vec<_> = lines
            .iter()
            .map(|line| (LogStream::Stdout, *line))
            .collect();
        run(output, &lines, code)
    }

    fn brief_lines(format: OutputFormat, stdout: &[&str]) -> Vec<String> {
        let mut brief = Brief::new(format);
        let mut lines: Vec<_> = stdout
            .iter()
            .flat_map(|line| brief.feed(LogStream::Stdout, line))
            .collect();
        lines.extend(brief.finish());
        lines
    }

    #[test]
    fn brief_claude_stream_summarizes_messages_tools_and_denials() {
        let input = [
            r#"{"type":"system","subtype":"init","model":"claude-sonnet"}"#,
            r#"{"type":"system","subtype":"hook_started","hook_name":"SessionStart"}"#,
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"確認します"},{"type":"tool_use","name":"Read","input":{"file_path":"src/lib.rs"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","is_error":true,"content":[{"type":"text","text":"missing"}]},{"type":"tool_result","is_error":false,"content":"omitted"}]}}"#,
            r#"{"type":"result","subtype":"success","result":"完了","permission_denials":[{"tool_name":"Bash","tool_input":{"command":"pwd"}},{"tool_name":"Write","tool_input":{"file_path":"blocked.txt"}}]}"#,
        ];
        assert_eq!(
            brief_lines(OutputFormat::ClaudeStreamJson, &input),
            [
                "[開始] claude-sonnet",
                "[文章] 確認します",
                "[ツール] Read src/lib.rs",
                "[エラー] missing",
                "[結果] 完了",
                "[拒否] Bash: pwd",
                "[拒否] Write: blocked.txt",
            ]
        );
    }

    #[test]
    fn brief_codex_summarizes_items_and_turns() {
        let input = [
            r#"{"type":"item.completed","item":{"id":"i0","type":"agent_message","text":"確認しました"}}"#,
            r#"{"type":"item.started","item":{"id":"i1","type":"command_execution","command":"Get-Location","exit_code":null}}"#,
            r#"{"type":"item.completed","item":{"id":"i1","type":"command_execution","command":"Get-Location","exit_code":0}}"#,
            r#"{"type":"item.completed","item":{"id":"i2","type":"file_change","changes":[{"path":"a.txt","kind":"update"},{"path":"b.txt","kind":"add"}]}}"#,
            r#"{"type":"item.completed","item":{"id":"i3","type":"mcp_tool_call","server":"docs","tool":"lookup"}}"#,
            r#"{"type":"item.completed","item":{"id":"i4","type":"error","message":"item failed"}}"#,
            r#"{"type":"error","message":"stream error"}"#,
            r#"{"type":"turn.failed","error":{"message":"turn failed"}}"#,
            r#"{"type":"turn.completed","usage":{}}"#,
        ];
        assert_eq!(
            brief_lines(OutputFormat::CodexJsonl, &input),
            [
                "[文章] 確認しました",
                "[コマンド] Get-Location",
                "[コマンド] Get-Location (exit 0)",
                "[変更] a.txt, b.txt",
                "[ツール] docs.lookup",
                "[エラー] item failed",
                "[エラー] stream error",
                "[エラー] turn failed",
                "[完了]",
            ]
        );
    }

    #[test]
    fn brief_grok_joins_chunks_until_an_event_or_finish() {
        let mut brief = Brief::new(OutputFormat::GrokJson);
        let feed = |brief: &mut Brief, line| brief.feed(LogStream::Stdout, line);
        assert!(feed(&mut brief, r#"{"type":"text","data":"読みま"}"#).is_empty());
        assert!(feed(&mut brief, r#"{"type":"text","data":"す"}"#).is_empty());
        assert_eq!(
            feed(
                &mut brief,
                r#"{"type":"tool_call","title":"read_file","rawInput":{"path":"a.txt"}}"#
            ),
            ["[文章] 読みます", "[ツール] read_file a.txt"]
        );
        assert!(feed(&mut brief, r#"{"type":"thought","data":"skip this"}"#).is_empty());
        assert_eq!(
            feed(
                &mut brief,
                r#"{"type":"tool_call_update","status":"failed","error":{"message":"missing"}}"#
            ),
            ["[エラー] missing"]
        );
        assert_eq!(
            feed(&mut brief, r#"{"type":"end","stopReason":"end_turn"}"#),
            ["[終了] end_turn"]
        );
        assert!(feed(&mut brief, r#"{"type":"text","data":"最"}"#).is_empty());
        assert!(feed(&mut brief, r#"{"type":"text","data":"後"}"#).is_empty());
        assert_eq!(brief.finish(), ["[文章] 最後"]);
        assert!(brief.finish().is_empty());
    }

    #[test]
    fn brief_antigravity_reads_final_pretty_json_and_passes_through_warnings() {
        let input = [
            "warning before",
            r#"{"status":"SUCCESS","response":"earlier"}"#,
            "{",
            r#"  "status": "SUCCESS","#,
            r#"  "response": "完了","#,
            r#"  "denied_actions": [{"display_name":"Bash","action":"command"},{"display_name":"Write"}]"#,
            "}",
            "warning after",
        ];
        assert_eq!(
            brief_lines(OutputFormat::AntigravityJson, &input),
            [
                "warning before",
                "warning after",
                "[結果] 完了",
                "[拒否] Bash (command)",
                "[拒否] Write",
            ]
        );
    }

    #[test]
    fn brief_text_and_other_streams_are_kept_with_unicode_limits() {
        let mut brief = Brief::new(OutputFormat::Text);
        assert_eq!(brief.feed(LogStream::Stdout, "  output  "), ["  output  "]);
        assert_eq!(
            brief.feed(LogStream::Stderr, "failure"),
            ["[stderr] failure"]
        );
        assert_eq!(brief.feed(LogStream::System, "status"), ["[system] status"]);
        for format in [
            OutputFormat::Text,
            OutputFormat::ClaudeStreamJson,
            OutputFormat::CodexJsonl,
            OutputFormat::GrokJson,
            OutputFormat::AntigravityJson,
        ] {
            let text = "長".repeat(1100);
            let lines = brief_lines(format, &[&text]);
            assert_eq!(lines, [format!("{}…", "長".repeat(999))]);
        }
        let command = "長".repeat(1100);
        let denied = serde_json::json!({
            "type": "result", "subtype": "success",
            "permission_denials": [{"tool_name": "Bash", "tool_input": {"command": command}}],
        })
        .to_string();
        let outcome = parsed(OutputFormat::ClaudeStreamJson, &[&denied], 0);
        assert_eq!(outcome.denied[0].chars().count(), 1000);
        assert!(outcome.denied[0].ends_with('…'));
    }

    #[test]
    fn text_tails_and_exit_status() {
        let adapter = ConfiguredAdapter::new(spec(OutputFormat::Text), 7);
        assert_eq!(adapter.spec().id, "fake");
        let mut acc = Accumulator::default();
        adapter.observe(LogStream::Stdout, &"あ".repeat(2100), &mut acc);
        adapter.observe(LogStream::Stderr, "先頭末尾", &mut acc);
        adapter.observe(LogStream::System, "ignored", &mut acc);
        let result = adapter.outcome(acc, &exit(0));
        assert_eq!(result.status, RunStatus::Succeeded);
        assert_eq!(result.summary.chars().count(), 2000);
        assert_eq!(result.stderr_tail, "末尾");
        assert!(result.denied.is_empty() && !result.denied_failure);
        assert_eq!(
            parsed(OutputFormat::Text, &["failed"], 3).status,
            RunStatus::Failed
        );
        for (killed, timed_out, expected) in [
            (true, false, RunStatus::Killed),
            (true, true, RunStatus::TimedOut),
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
        }
    }

    #[test]
    fn claude_result_fallback_error_and_denials() {
        let lines = [
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"途中"},{"type":"text","text":"の返事"}]}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"完了"}"#,
        ];
        let result = parsed(OutputFormat::ClaudeStreamJson, &lines, 0);
        assert_eq!(result.status, RunStatus::Succeeded);
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
                RunStatus::Failed
            );
        }
        let denied = parsed(
            OutputFormat::ClaudeStreamJson,
            &[
                r#"{"type":"result","subtype":"success","result":"一部できませんでした","permission_denials":[{"tool_name":"Bash","tool_input":{"command":"rm -rf build"}},{"tool_name":"Write","tool_input":{"file_path":"C:/x.txt"}},{"tool_name":"WebFetch","tool_input":{"url":"u"}}]}"#,
            ],
            0,
        );
        assert_eq!(denied.status, RunStatus::Succeeded);
        assert_eq!(
            denied.denied,
            [
                "Bash: rm -rf build",
                "Write: C:/x.txt",
                r#"WebFetch: {"url":"u"}"#
            ]
        );
        assert!(!denied.denied_failure);
    }

    #[test]
    fn codex_messages_and_failures() {
        let lines = [
            r#"{"type":"item.completed","item":{"type":"assistant_message","text":"古い形"}}"#,
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"最後の返事"}}"#,
        ];
        let result = parsed(OutputFormat::CodexJsonl, &lines, 0);
        assert_eq!(result.status, RunStatus::Succeeded);
        assert_eq!(result.summary, "最後の返事");
        for error in [
            r#"{"type":"turn.failed","error":{"message":"失敗しました"}}"#,
            r#"{"type":"error","message":"失敗しました"}"#,
        ] {
            let result = parsed(OutputFormat::CodexJsonl, &[lines[1], error], 0);
            assert_eq!(result.status, RunStatus::Failed);
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
        assert_eq!(result.status, RunStatus::Succeeded);
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
        assert_eq!(result.status, RunStatus::Failed);
        assert!(result.summary.starts_with("失敗"));
        let long = format!(
            r#"{{"sessionUpdate":"agent_message_chunk","content":{{"text":"{}"}}}}"#,
            "あ".repeat(CHUNK_LIMIT + 10)
        );
        assert_eq!(
            parsed(OutputFormat::GrokJson, &[&long, &long], 0)
                .summary
                .chars()
                .count(),
            CHUNK_LIMIT
        );
    }

    #[test]
    fn grok_streaming_json_keeps_the_text_after_the_last_tool_call() {
        // Shortened from a real `grok --output-format streaming-json` run (grok 1.0.92).
        let lines = [
            r#"{"type":"available_commands","tools":["read_file"]}"#,
            r#"{"type":"thought","data":"README.txt を読む。"}"#,
            r#"{"type":"text","data":"README"}"#,
            r#"{"type":"text","data":".txt を読みます。"}"#,
            r#"{"type":"usage","usage":{"input_tokens":1}}"#,
            r#"{"type":"tool_call","toolCallId":"c1","title":"read_file","kind":"read","status":"pending"}"#,
            r#"{"type":"tool_call_update","toolCallId":"c1","status":"completed"}"#,
            r#"{"type":"thought","data":"demo と書かれている。"}"#,
            r#"{"type":"text","data":"de"}"#,
            r#"{"type":"text","data":"mo"}"#,
            r#"{"type":"end","stopReason":"end_turn","sessionId":"s"}"#,
        ];
        let result = parsed(OutputFormat::GrokJson, &lines, 0);
        assert_eq!(result.status, RunStatus::Succeeded);
        assert_eq!(result.summary, "demo");
        let cancelled = parsed(
            OutputFormat::GrokJson,
            &[
                r#"{"type":"text","data":"消せませんでした"}"#,
                r#"{"type":"end","stopReason":"cancelled","sessionId":"s"}"#,
            ],
            0,
        );
        assert_eq!(cancelled.status, RunStatus::Failed);
        assert_eq!(cancelled.summary, "消せませんでした");
        let silent = parsed(
            OutputFormat::GrokJson,
            &[
                r#"{"type":"text","data":"これから変更します"}"#,
                r#"{"type":"tool_call","toolCallId":"c1","title":"edit","kind":"edit"}"#,
                r#"{"type":"tool_call_update","toolCallId":"c1","status":"completed"}"#,
                r#"{"type":"end","stopReason":"end_turn","sessionId":"s"}"#,
            ],
            0,
        );
        assert_eq!(
            silent.summary,
            "Grok はツールを使ったあと、最後に文章を返しませんでした"
        );
        let late = parsed(
            OutputFormat::GrokJson,
            &[
                r#"{"type":"tool_call","toolCallId":"c1","title":"edit","kind":"edit"}"#,
                r#"{"type":"text","data":"変更しました。"}"#,
                r#"{"type":"tool_call_update","toolCallId":"c1","status":"completed"}"#,
                r#"{"type":"text","data":"テストも成功しました。"}"#,
                r#"{"type":"end","stopReason":"end_turn","sessionId":"s"}"#,
            ],
            0,
        );
        assert_eq!(late.summary, "変更しました。テストも成功しました。");
    }

    #[test]
    fn grok_refusals_fail_the_run() {
        let result = run(
            OutputFormat::GrokJson,
            &[
                (
                    LogStream::Stderr,
                    "permission required: Execute `Remove-Item b.txt` was not run, as a headless run cannot ask for approval",
                ),
                (
                    LogStream::Stdout,
                    r#"{"text":"削除できませんでした","stopReason":"cancelled","sessionId":"s"}"#,
                ),
            ],
            0,
        );
        assert_eq!(result.status, RunStatus::Failed);
        assert!(result.denied_failure);
        assert_eq!(
            result.denied,
            ["Execute `Remove-Item b.txt` was not run, as a headless run cannot ask for approval"]
        );
        assert_eq!(result.summary, "削除できませんでした");
        let plain = parsed(
            OutputFormat::GrokJson,
            &[r#"{"text":"止まりました","stopReason":"cancelled"}"#],
            0,
        );
        assert_eq!(plain.status, RunStatus::Failed);
        assert!(!plain.denied_failure);
    }

    #[test]
    fn antigravity_json_after_warnings_and_over_lines() {
        let ok = parsed(
            OutputFormat::AntigravityJson,
            &[
                "warning: --mode auto is not available for this account",
                r#"{"conversation_id":"c","status":"SUCCESS","response":"作成しました\n","duration_seconds":12.3,"num_turns":1,"usage":{}}"#,
            ],
            0,
        );
        assert_eq!(ok.status, RunStatus::Succeeded);
        assert_eq!(ok.summary, "作成しました");
        let pretty = parsed(
            OutputFormat::AntigravityJson,
            &[
                "{",
                r#"  "status": "SUCCESS","#,
                r#"  "usage": {"a": 1},"#,
                r#"  "response": "複数行""#,
                "}",
            ],
            0,
        );
        assert_eq!(pretty.summary, "複数行");
        let denied = parsed(
            OutputFormat::AntigravityJson,
            &[
                r#"{"conversation_id":"c","status":"SUCCESS","response":"","num_turns":1,"usage":{},"denied_actions":[{"action":"command","display_name":"Bash"}]}"#,
            ],
            0,
        );
        assert_eq!(denied.status, RunStatus::Succeeded);
        assert_eq!(denied.denied, ["Bash (command)"]);
        assert_eq!(
            denied.summary,
            "ヘッドレスの CLI が操作を拒否しました: Bash (command)"
        );
        let failed = parsed(
            OutputFormat::AntigravityJson,
            &[
                r#"{"status":"ERROR","response":"","denied_actions":[{"action":"command","display_name":"Bash"}]}"#,
            ],
            0,
        );
        assert_eq!(failed.status, RunStatus::Failed);
        assert!(failed.denied_failure);
        assert_eq!(failed.summary, "Antigravity の状態: ERROR");
        let unreadable = parsed(OutputFormat::AntigravityJson, &["not json"], 0);
        assert_eq!(unreadable.status, RunStatus::Failed);
        assert_eq!(
            unreadable.summary,
            "Antigravity の結果の JSON を読み取れませんでした\nnot json"
        );
    }

    #[test]
    fn overlong_lines_and_denial_limits() {
        let adapter = ConfiguredAdapter::new(spec(OutputFormat::ClaudeStreamJson), 4096);
        let mut acc = Accumulator::default();
        acc.note_overlong();
        let result = adapter.outcome(acc, &exit(0));
        assert_eq!(result.summary, "出力の行が長すぎて読み取れませんでした");
        let mut acc = Accumulator::default();
        for index in 0..30 {
            acc.deny(&format!("{index} {}", "x".repeat(DENIED_CHARS + 1)));
        }
        acc.deny("");
        assert_eq!(acc.denied.len(), DENIED_LIMIT);
        assert!(
            acc.denied
                .iter()
                .all(|entry| entry.chars().count() == DENIED_CHARS)
        );
        // A long output keeps its end, where the result is.
        let big = "x".repeat(WHOLE_LIMIT / 2);
        let antigravity = ConfiguredAdapter::new(spec(OutputFormat::AntigravityJson), 4096);
        let mut acc = Accumulator::default();
        for _ in 0..7 {
            antigravity.observe(LogStream::Stdout, &big, &mut acc);
            assert!(acc.whole.len() <= 2 * WHOLE_LIMIT);
        }
        antigravity.observe(
            LogStream::Stdout,
            r#"{"status":"SUCCESS","response":"最後","denied_actions":[{"display_name":"Bash"}]}"#,
            &mut acc,
        );
        let result = antigravity.outcome(acc, &exit(0));
        assert_eq!((result.summary.as_str(), result.denied.len()), ("最後", 1));
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
            (PromptVia::Arg, "-p={prompt}"),
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
                PromptVia::Arg => assert_eq!(plan.args, vec![format!("-p={}", req.prompt)]),
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
