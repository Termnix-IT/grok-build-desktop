use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["next_action","reasoning","target_agent","converted_prompt","step_title","question","summary"],"properties":{"next_action":{"type":"string","enum":["dispatch","complete","ask_human"]},"reasoning":{"type":"string"},"target_agent":{"type":["string","null"]},"converted_prompt":{"type":["string","null"]},"step_title":{"type":["string","null"]},"question":{"type":["string","null"]},"summary":{"type":["string","null"]}}}"#;

#[derive(Deserialize, Debug, Clone)]
pub struct RawDecision {
    pub next_action: String,
    #[serde(default)]
    pub reasoning: Option<String>,
    #[serde(default)]
    pub target_agent: Option<String>,
    #[serde(default)]
    pub converted_prompt: Option<String>,
    #[serde(default)]
    pub step_title: Option<String>,
    #[serde(default)]
    pub question: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum NextAction {
    Dispatch,
    Complete,
    AskHuman,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Decision {
    pub next_action: NextAction,
    pub reasoning: String,
    pub target_agent: Option<String>,
    pub converted_prompt: Option<String>,
    pub step_title: Option<String>,
    pub question: Option<String>,
    pub summary: Option<String>,
}

const UNREADABLE: &str = "司令塔の返事から判断を読み取れませんでした";

pub fn parse_planner_output(stdout: &str) -> Result<RawDecision, String> {
    if let Ok(value) = serde_json::from_str::<Value>(stdout) {
        return parse_value(&value).unwrap_or_else(|| Err(UNREADABLE.into()));
    }
    for line in stdout.lines().rev().filter(|line| !line.trim().is_empty()) {
        if let Ok(value) = serde_json::from_str::<Value>(line)
            && let Some(result) = parse_value(&value)
        {
            return result;
        }
    }
    Err(UNREADABLE.into())
}

fn parse_value(value: &Value) -> Option<Result<RawDecision, String>> {
    if let Some(decision) = find_object(value, 0)
        .cloned()
        .or_else(|| find_string_object(value, 0))
    {
        return Some(
            serde_json::from_value(decision)
                .map_err(|error| format!("司令塔の判断の形式が正しくありません: {error}")),
        );
    }
    if value.get("type").and_then(Value::as_str) == Some("error") {
        return Some(Err(value
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or(UNREADABLE)
            .to_owned()));
    }
    None
}

fn find_object(value: &Value, depth: usize) -> Option<&Value> {
    if depth > 4 {
        return None;
    }
    match value {
        Value::Object(map) => {
            if map.contains_key("next_action") {
                Some(value)
            } else {
                map.values().find_map(|child| find_object(child, depth + 1))
            }
        }
        Value::Array(items) => items.iter().find_map(|child| find_object(child, depth + 1)),
        _ => None,
    }
}

fn find_string_object(value: &Value, depth: usize) -> Option<Value> {
    if depth > 4 {
        return None;
    }
    match value {
        Value::Object(map) => {
            for key in [
                "result",
                "structured_output",
                "output",
                "response",
                "content",
                "text",
                "message",
            ] {
                if let Some(text) = map.get(key).and_then(Value::as_str)
                    && let Ok(parsed) = serde_json::from_str::<Value>(strip_fence(text))
                    && let Some(found) = find_object(&parsed, 0)
                {
                    return Some(found.clone());
                }
            }
            map.values()
                .find_map(|child| find_string_object(child, depth + 1))
        }
        Value::Array(items) => items
            .iter()
            .find_map(|child| find_string_object(child, depth + 1)),
        _ => None,
    }
}

fn strip_fence(text: &str) -> &str {
    let text = text.trim();
    if let Some(inner) = text
        .strip_prefix("```json")
        .or_else(|| text.strip_prefix("```"))
        && let Some(inner) = inner.trim().strip_suffix("```")
    {
        return inner.trim();
    }
    text
}

pub fn validate(raw: RawDecision, enabled_agents: &[&str]) -> Result<Decision, String> {
    fn clean(value: Option<String>) -> Option<String> {
        value
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    }
    let reasoning = clean(raw.reasoning).ok_or("司令塔の reasoning が空です")?;
    let next_action = match raw.next_action.trim().to_ascii_lowercase().as_str() {
        "dispatch" => NextAction::Dispatch,
        "complete" => NextAction::Complete,
        "ask_human" => NextAction::AskHuman,
        _ => {
            return Err(
                "司令塔の next_action が dispatch・complete・ask_human のいずれでもありません"
                    .into(),
            );
        }
    };
    let mut decision = Decision {
        next_action,
        reasoning,
        target_agent: clean(raw.target_agent),
        converted_prompt: clean(raw.converted_prompt),
        step_title: clean(raw.step_title).map(|title| title.chars().take(40).collect()),
        question: clean(raw.question),
        summary: clean(raw.summary),
    };
    match next_action {
        NextAction::Dispatch => {
            let target = decision
                .target_agent
                .as_deref()
                .ok_or("司令塔の target_agent が空です")?;
            if !enabled_agents.contains(&target) {
                return Err(format!(
                    "司令塔が指定したエージェント「{target}」は有効ではありません"
                ));
            }
            let prompt = decision
                .converted_prompt
                .as_ref()
                .ok_or("司令塔の converted_prompt が空です")?;
            if prompt.len() > 32768 {
                return Err("司令塔の converted_prompt は 32768 バイト以下にしてください".into());
            }
        }
        NextAction::AskHuman if decision.question.is_none() => {
            return Err("司令塔の question が空です".into());
        }
        NextAction::Complete if decision.summary.is_none() => {
            decision.summary = Some(decision.reasoning.clone())
        }
        _ => {}
    }
    Ok(decision)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn raw() -> RawDecision {
        RawDecision {
            next_action: "dispatch".into(),
            reasoning: Some("判断の理由".into()),
            target_agent: Some("codex".into()),
            converted_prompt: Some("実装してください".into()),
            step_title: None,
            question: None,
            summary: None,
        }
    }

    #[test]
    fn schema_is_one_line_json() {
        assert!(!SCHEMA.contains('\n'));
        let value: Value = serde_json::from_str(SCHEMA).unwrap();
        assert_eq!(value["required"].as_array().unwrap().len(), 7);
    }

    #[test]
    fn reads_common_wrappers_and_json_lines() {
        let decision = json!({"next_action":"complete","reasoning":"done"});
        for value in [
            decision.clone(),
            json!({"result":decision.to_string()}),
            json!({"result":format!("  ```json\n{}\n```  ", decision)}),
            json!({"structured_output":decision}),
            json!({"response":{"content":[{"text":decision.to_string()}]}}),
            json!({"items":[{"output":{"nested":decision}}]}),
        ] {
            let parsed = parse_planner_output(&value.to_string()).unwrap();
            assert_eq!(parsed.next_action, "complete");
        }
        let lines = format!("not json\n{{\"next_action\":\"dispatch\"}}\n{decision}\n");
        assert_eq!(
            parse_planner_output(&lines).unwrap().next_action,
            "complete"
        );
    }

    #[test]
    fn rejects_errors_unreadable_and_deep_outputs() {
        assert_eq!(
            parse_planner_output(r#"{"type":"error","message":"Not signed in"}"#).unwrap_err(),
            "Not signed in"
        );
        for text in [
            "nothing",
            "{}",
            "[]",
            "{\"next_action\":123}",
            r#"{"a":{"b":{"c":{"d":{"e":{"next_action":"complete"}}}}}}"#,
        ] {
            assert!(parse_planner_output(text).is_err(), "{text}");
        }
        assert_eq!(parse_planner_output("nothing").unwrap_err(), UNREADABLE);
    }

    #[test]
    fn validates_dispatch_and_trims_all_strings() {
        let mut value = raw();
        value.next_action = " DISPATCH ".into();
        value.reasoning = Some("  reason  ".into());
        value.target_agent = Some(" codex ".into());
        value.converted_prompt = Some("  work  ".into());
        value.step_title = Some(format!(" {} ", "設".repeat(41)));
        value.question = Some("  ".into());
        value.summary = Some(" summary ".into());
        let decision = validate(value, &["codex"]).unwrap();
        assert_eq!(decision.next_action, NextAction::Dispatch);
        assert_eq!(decision.reasoning, "reason");
        assert_eq!(decision.target_agent.as_deref(), Some("codex"));
        assert_eq!(decision.converted_prompt.as_deref(), Some("work"));
        assert_eq!(decision.step_title.unwrap().chars().count(), 40);
        assert_eq!(decision.question, None);
        assert_eq!(decision.summary.as_deref(), Some("summary"));
        assert!(validate(raw(), &["claude"]).is_err());
        let mut value = raw();
        value.converted_prompt = Some(" ".into());
        assert!(validate(value, &["codex"]).is_err());
        let mut value = raw();
        value.converted_prompt = Some("あ".repeat(10923));
        assert!(validate(value, &["codex"]).is_err());
        let mut value = raw();
        value.converted_prompt = Some("a".repeat(32768));
        assert!(validate(value, &["codex"]).is_ok());
        let mut value = raw();
        value.reasoning = None;
        assert!(validate(value, &["codex"]).is_err());
        let mut value = raw();
        value.next_action = "bad".into();
        assert!(validate(value, &["codex"]).is_err());
    }

    #[test]
    fn validates_questions_and_completion() {
        let mut value = raw();
        value.next_action = "ask_human".into();
        assert!(validate(value.clone(), &[]).is_err());
        value.question = Some(" どちらですか？ ".into());
        assert_eq!(
            validate(value, &[]).unwrap().question.as_deref(),
            Some("どちらですか？")
        );
        let mut value = raw();
        value.next_action = "COMPLETE".into();
        let decision = validate(value, &[]).unwrap();
        assert_eq!(decision.summary.as_deref(), Some("判断の理由"));
        assert_eq!(
            serde_json::to_value(decision).unwrap()["nextAction"],
            "complete"
        );
    }
}
