//! Token usage, file links, and active-session parsing.
//! These functions do not launch Grok.

use std::path::Path;

use agent_client_protocol::schema::v1::{ContentBlock, ResourceLink, TextContent};
use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub cached_read_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveSession {
    pub session_id: String,
    pub pid: u32,
    pub cwd: String,
}

pub fn usage_from_value(value: &Value) -> Option<TurnUsage> {
    match value {
        Value::Object(map) => usage_from_object(map).or_else(|| {
            for key in ["usage", "turn_completed", "turnCompleted"] {
                if let Some(child) = map.get(key)
                    && let Some(usage) = usage_from_value(child)
                {
                    return Some(usage);
                }
            }
            for (key, child) in map {
                if key == "usage" || key == "turn_completed" || key == "turnCompleted" {
                    continue;
                }
                if let Some(usage) = usage_from_value(child) {
                    return Some(usage);
                }
            }
            None
        }),
        Value::Array(items) => items.iter().find_map(usage_from_value),
        _ => None,
    }
}

fn usage_from_object(map: &Map<String, Value>) -> Option<TurnUsage> {
    let input_tokens = first_num(map, &["inputTokens", "input_tokens"]);
    let output_tokens = first_num(map, &["outputTokens", "output_tokens"]);
    let total_tokens = first_num(map, &["totalTokens", "total_tokens"]);
    let cached_read_tokens = first_num(map, &["cachedReadTokens", "cached_read_tokens"]);
    let reasoning_tokens = first_num(map, &["reasoningTokens", "reasoning_tokens"]);
    if input_tokens.is_none() && output_tokens.is_none() && total_tokens.is_none() {
        return None;
    }
    Some(TurnUsage {
        input_tokens,
        output_tokens,
        total_tokens,
        cached_read_tokens,
        reasoning_tokens,
    })
}

fn first_num(map: &Map<String, Value>, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|key| map.get(*key).and_then(as_u64))
}

fn as_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().filter(|n| *n >= 0).map(|n| n as u64))
        .or_else(|| value.as_f64().filter(|n| n.is_finite() && *n >= 0.0).map(|n| n as u64))
}

/// Context occupancy uses input tokens when the turn reports them.
/// `totalTokens` includes output, so it is only a fallback.
pub fn used_tokens(usage: &TurnUsage) -> Option<u64> {
    usage.input_tokens.or(usage.total_tokens)
}

pub fn context_window(meta: &Value) -> Option<u64> {
    let state = meta.get("modelState").or_else(|| meta.get("model_state"))?;
    positive_u64(state.get("totalContextTokens"))
        .or_else(|| positive_u64(state.get("total_context_tokens")))
        .or_else(|| current_model_window(state))
}

pub fn current_model(meta: &Value) -> Option<String> {
    let state = meta.get("modelState").or_else(|| meta.get("model_state"))?;
    model_id_text(state.get("currentModelId"))
        .or_else(|| model_id_text(state.get("current")))
        .or_else(|| model_id_text(state.get("currentModel")))
}

fn current_model_window(state: &Value) -> Option<u64> {
    let models = state
        .get("availableModels")
        .or_else(|| state.get("available_models"))?
        .as_array()?;
    let current = model_id_text(state.get("currentModelId"))
        .or_else(|| model_id_text(state.get("current")))
        .or_else(|| model_id_text(state.get("currentModel")));
    let chosen = models.iter().find(|model| {
        model_id_text(model.get("modelId")).or_else(|| model_id_text(model.get("id"))) == current
    });
    let chosen = chosen.or_else(|| models.first())?;
    let meta = chosen.get("_meta").or_else(|| chosen.get("meta"))?;
    positive_u64(meta.get("totalContextTokens"))
        .or_else(|| positive_u64(meta.get("total_context_tokens")))
}

fn model_id_text(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::to_string)
}

fn positive_u64(value: Option<&Value>) -> Option<u64> {
    value.and_then(as_u64).filter(|size| *size > 0)
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn free_percent(used: u64, size: u64) -> u64 {
    if size == 0 {
        return 0;
    }
    size.saturating_sub(used).saturating_mul(100) / size
}

/// `/compact` and `/context` on stdio can finish immediately with no agent text.
pub fn turn_reported_nothing(usage: Option<&TurnUsage>, saw_compaction: bool, saw_agent_text: bool) -> bool {
    if saw_compaction || saw_agent_text {
        return false;
    }
    match usage {
        None => true,
        Some(usage) => {
            usage.total_tokens.unwrap_or(0) == 0 && usage.input_tokens.unwrap_or(0) == 0
        }
    }
}

pub fn file_uri(path: &str) -> String {
    let mut text = path.replace('\\', "/");
    if let Some(rest) = text.strip_prefix("//?/") {
        text = rest.to_string();
    }
    if let Some(unc) = text.strip_prefix("//") {
        return format!("file://{}", encode_path(unc));
    }
    format!("file:///{}", encode_path(&text))
}

fn encode_path(text: &str) -> String {
    let mut uri = String::new();
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b':' | b'-' | b'_' | b'.' | b'~' => {
                uri.push(byte as char);
            }
            _ => uri.push_str(&format!("%{byte:02X}")),
        }
    }
    uri
}

pub fn prompt_blocks(text: &str, files: &[String]) -> Vec<ContentBlock> {
    let mut blocks = vec![ContentBlock::Text(TextContent::new(text))];
    for file in files {
        let path = Path::new(file);
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .unwrap_or(file);
        blocks.push(ContentBlock::ResourceLink(ResourceLink::new(
            name,
            file_uri(file),
        )));
    }
    blocks
}

pub fn parse_active_sessions(bytes: &[u8]) -> Vec<ActiveSession> {
    let value: Value = match serde_json::from_slice(bytes) {
        Ok(value) => value,
        Err(_) => return Vec::new(),
    };
    let items = match &value {
        Value::Array(items) => items,
        Value::Object(map) => match map.get("sessions").and_then(Value::as_array) {
            Some(items) => items,
            None => return Vec::new(),
        },
        _ => return Vec::new(),
    };
    items
        .iter()
        .filter_map(|item| {
            let session_id = item.get("session_id")?.as_str()?.to_string();
            let pid = as_u64(item.get("pid")?)?;
            let cwd = item.get("cwd").and_then(Value::as_str).unwrap_or("").to_string();
            Some(ActiveSession {
                session_id,
                pid: pid as u32,
                cwd,
            })
        })
        .collect()
}

pub fn foreign_owner<'a>(
    sessions: &'a [ActiveSession],
    session_id: &str,
    our_pid: Option<u32>,
) -> Option<&'a ActiveSession> {
    sessions.iter().find(|session| {
        session.session_id == session_id && Some(session.pid) != our_pid
    })
}

pub fn search_files(root: &Path, query: &str, limit: usize) -> Vec<String> {
    const SKIP: &[&str] = &["node_modules", "target", ".git", "dist"];
    let needle = query.to_lowercase();
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    let mut visited = 0usize;
    while let Some(dir) = stack.pop() {
        if out.len() >= limit || visited > 8000 {
            break;
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            visited += 1;
            if out.len() >= limit || visited > 8000 {
                break;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if SKIP.contains(&name.as_str()) {
                continue;
            }
            let path = entry.path();
            let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
            if is_dir {
                stack.push(path);
                continue;
            }
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_lowercase();
            if needle.is_empty() || relative.contains(&needle) || name.to_lowercase().contains(&needle) {
                out.push(path.to_string_lossy().to_string());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_turn_usage_from_prompt_meta() {
        let meta = json!({
            "inputTokens": 22585,
            "outputTokens": 593,
            "totalTokens": 23178,
            "cachedReadTokens": 1664,
            "reasoningTokens": 592,
            "costUsdTicks": 157188800
        });
        let usage = usage_from_value(&meta).unwrap();
        assert_eq!(usage.input_tokens, Some(22585));
        assert_eq!(usage.output_tokens, Some(593));
        assert_eq!(used_tokens(&usage), Some(22585));
        assert_eq!(free_percent(22585, 256_000), 91);
    }

    #[test]
    fn reads_nested_turn_completed_usage() {
        let value = json!({
            "turn_completed": { "usage": { "inputTokens": 10, "totalTokens": 12 } }
        });
        let usage = usage_from_value(&value).unwrap();
        assert_eq!(usage.input_tokens, Some(10));
        assert_eq!(usage.total_tokens, Some(12));
    }

    #[test]
    fn reads_context_window_from_model_state() {
        let meta = json!({
            "modelState": {
                "current": "grok-4.7",
                "totalContextTokens": 256000,
                "models": [{ "id": "grok-4.7", "contextWindows": [256000, 500000] }]
            }
        });
        assert_eq!(context_window(&meta), Some(256_000));
        assert_eq!(current_model(&meta).as_deref(), Some("grok-4.7"));
    }

    #[test]
    fn reads_context_window_from_the_current_model_meta() {
        let meta = json!({
            "modelState": {
                "currentModelId": "grok-4.7",
                "availableModels": [{
                    "modelId": "grok-4.7",
                    "_meta": { "totalContextTokens": 256000 }
                }]
            }
        });
        assert_eq!(context_window(&meta), Some(256_000));
        assert_eq!(current_model(&meta).as_deref(), Some("grok-4.7"));
    }

    #[test]
    fn empty_slash_turn_is_a_noop_until_the_agent_reports_work() {
        assert!(turn_reported_nothing(None, false, false));
        assert!(turn_reported_nothing(
            Some(&TurnUsage {
                input_tokens: Some(0),
                output_tokens: None,
                total_tokens: Some(0),
                cached_read_tokens: None,
                reasoning_tokens: None,
            }),
            false,
            false
        ));
        assert!(!turn_reported_nothing(None, true, false));
        assert!(!turn_reported_nothing(None, false, true));
    }

    #[test]
    fn prompt_blocks_use_resource_links_for_files() {
        let blocks = prompt_blocks("見て", &[r"C:\work\a b.txt".into()]);
        let json = serde_json::to_value(&blocks).unwrap();
        assert_eq!(json[0]["type"], "text");
        assert_eq!(json[0]["text"], "見て");
        assert_eq!(json[1]["type"], "resource_link");
        assert_eq!(json[1]["name"], "a b.txt");
        assert_eq!(json[1]["uri"], "file:///C:/work/a%20b.txt");
    }

    #[test]
    fn active_session_warning_skips_our_child_pid() {
        let raw = br#"[{"session_id":"abc","pid":10,"cwd":"C:\\work","opened_at":"t"}]"#;
        let sessions = parse_active_sessions(raw);
        assert!(foreign_owner(&sessions, "abc", Some(10)).is_none());
        let owner = foreign_owner(&sessions, "abc", Some(99)).unwrap();
        assert_eq!(owner.pid, 10);
        assert_eq!(owner.cwd, r"C:\work");
    }
}
