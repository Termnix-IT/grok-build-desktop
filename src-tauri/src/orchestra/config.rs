use std::{collections::HashSet, fs, path::Path};

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Registry {
    pub version: u32,
    #[serde(default)]
    pub limits: Limits,
    pub agents: Vec<AgentSpec>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Limits {
    pub max_dispatches_per_task: u32,
    pub max_consecutive_per_project: u32,
    pub same_error_limit: u32,
    pub default_timeout_secs: u64,
    pub max_queue: usize,
    pub pause_on_deletion: bool,
    pub log_tail_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_dispatches_per_task: 10,
            max_consecutive_per_project: 30,
            same_error_limit: 2,
            default_timeout_secs: 1200,
            max_queue: 20,
            pause_on_deletion: true,
            log_tail_bytes: 4096,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgentSpec {
    pub id: String,
    pub name: String,
    pub kind: AgentKind,
    pub enabled: bool,
    pub program: String,
    pub args: Vec<String>,
    pub prompt_via: PromptVia,
    pub output: OutputFormat,
    pub role: String,
    pub allow_dangerous: bool,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum AgentKind {
    Claude,
    Codex,
    Grok,
    Antigravity,
    Custom,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PromptVia {
    Stdin,
    Arg,
    File,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum OutputFormat {
    Text,
    ClaudeStreamJson,
    CodexJsonl,
    GrokJson,
    AntigravityJson,
}

pub fn defaults() -> Registry {
    let agent = |id: &str,
                 name: &str,
                 kind,
                 enabled,
                 program: &str,
                 args: &[&str],
                 prompt_via,
                 output,
                 role: &str| AgentSpec {
        id: id.into(),
        name: name.into(),
        kind,
        enabled,
        program: program.into(),
        args: args.iter().map(|arg| (*arg).into()).collect(),
        prompt_via,
        output,
        role: role.into(),
        allow_dangerous: false,
        timeout_secs: None,
    };
    Registry {
        version: 1,
        limits: Limits::default(),
        agents: vec![
            agent(
                "claude",
                "Claude Code",
                AgentKind::Claude,
                true,
                "claude",
                &[
                    "-p",
                    "--output-format",
                    "stream-json",
                    "--verbose",
                    "--permission-mode",
                    "acceptEdits",
                    "--permission-prompts",
                    "none",
                ],
                PromptVia::Stdin,
                OutputFormat::ClaudeStreamJson,
                "設計、複数のファイルにまたがる実装、リファクタリング、文章の作成が得意。確認が必要な操作（シェルの実行など）は自動で拒否される。",
            ),
            agent(
                "codex",
                "Codex",
                AgentKind::Codex,
                true,
                "codex",
                &[
                    "exec",
                    "--json",
                    "-s",
                    "workspace-write",
                    "-C",
                    "{cwd}",
                    "-",
                ],
                PromptVia::Stdin,
                OutputFormat::CodexJsonl,
                "仕様が決まったバックエンドの実装、テストの追加と実行、バグの原因調査が得意。作業フォルダの中だけで書き込みとコマンド実行ができる。",
            ),
            agent(
                "grok",
                "Grok",
                AgentKind::Grok,
                true,
                "grok",
                &[
                    "--prompt-file",
                    "{promptFile}",
                    "--output-format",
                    "streaming-json",
                    "--permission-mode",
                    "acceptEdits",
                    "--cwd",
                    "{cwd}",
                ],
                PromptVia::File,
                OutputFormat::GrokJson,
                "コードベースの調査、小さな修正、別の視点からのレビューが得意。読み取りだけのコマンドは実行でき、ファイルを消すなどのコマンドは自動で拒否される。",
            ),
            agent(
                "antigravity",
                "Antigravity",
                AgentKind::Antigravity,
                true,
                "agy",
                &[
                    "--output-format",
                    "json",
                    "--mode",
                    "accept-edits",
                    "--sandbox",
                    "-p={prompt}",
                ],
                PromptVia::Arg,
                OutputFormat::AntigravityJson,
                "Google の Antigravity CLI。ファイルの編集ができる。シェルのコマンドは自動で拒否される。",
            ),
        ],
    }
}

pub fn dangerous_flag(args: &[String]) -> Option<String> {
    for (index, arg) in args.iter().enumerate() {
        let normalized = decode_json_escapes(arg);
        let next = args.get(index + 1).map(|value| decode_json_escapes(value));
        for candidate in [arg.as_str(), normalized.as_str()] {
            if dangerous_value(candidate, next.as_deref()) {
                return Some(arg.clone());
            }
            for json in [
                Some(candidate),
                candidate.split_once('=').map(|(_, value)| value),
            ]
            .into_iter()
            .flatten()
            {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(json)
                    && dangerous_json(&value)
                {
                    return Some(arg.clone());
                }
            }
        }
    }
    None
}

fn dangerous_value(text: &str, next: Option<&str>) -> bool {
    const MARKERS: &[&str] = &[
        "dangerously-",
        "--yolo",
        "--always-approve",
        "bypasspermissions",
        "danger-full-access",
    ];
    let lower = text.to_ascii_lowercase();
    let (flag, value) = lower
        .split_once('=')
        .map_or((lower.as_str(), None), |(flag, value)| (flag, Some(value)));
    MARKERS.iter().any(|marker| lower.contains(marker))
        || [Some(flag), value]
            .into_iter()
            .flatten()
            .any(|part| part.trim_matches(['"', '\'']) == "yolo")
        || (flag == "--approve"
            && (value == Some("all")
                || (value.is_none() && next.is_some_and(|next| next.eq_ignore_ascii_case("all")))))
}

fn dangerous_json(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(text) => dangerous_value(&decode_json_escapes(text), None),
        serde_json::Value::Array(values) => values.iter().any(dangerous_json),
        serde_json::Value::Object(values) => values.iter().any(|(key, value)| {
            dangerous_value(&decode_json_escapes(key), None) || dangerous_json(value)
        }),
        _ => false,
    }
}

fn decode_json_escapes(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        if rest.starts_with("\\/") {
            output.push('/');
            rest = &rest[2..];
            continue;
        }
        if rest.starts_with("\\u") {
            // serde also validates UTF-16 surrogate pairs; malformed escapes remain literal.
            let mut consumed = false;
            for length in [12, 6] {
                if let Some(escape) = rest.get(..length)
                    && let Ok(decoded) = serde_json::from_str::<String>(&format!("\"{escape}\""))
                {
                    output.push_str(&decoded);
                    rest = &rest[length..];
                    consumed = true;
                    break;
                }
            }
            if consumed {
                continue;
            }
        }
        let ch = rest.chars().next().expect("nonempty text");
        output.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    output
}

pub fn validate(registry: &Registry) -> Result<(), String> {
    if registry.version != 1 {
        return Err("version は 1 にしてください".into());
    }
    let settings = &registry.limits;
    for (name, value, min, max) in [
        (
            "maxDispatchesPerTask",
            u64::from(settings.max_dispatches_per_task),
            1,
            100,
        ),
        (
            "maxConsecutivePerProject",
            u64::from(settings.max_consecutive_per_project),
            1,
            500,
        ),
        (
            "sameErrorLimit",
            u64::from(settings.same_error_limit),
            1,
            10,
        ),
        (
            "defaultTimeoutSecs",
            settings.default_timeout_secs,
            30,
            7200,
        ),
        ("maxQueue", settings.max_queue as u64, 1, 100),
        ("logTailBytes", settings.log_tail_bytes as u64, 256, 65536),
    ] {
        if !(min..=max).contains(&value) {
            return Err(format!(
                "limits.{name} は {min}〜{max} の範囲にしてください"
            ));
        }
    }
    if registry.agents.len() > 16 {
        return Err("agents は 16 個以下にしてください".into());
    }
    let mut ids = HashSet::new();
    for (index, agent) in registry.agents.iter().enumerate() {
        let item = format!("agents[{}]（{}）", index + 1, agent.id);
        let valid_id =
            !agent.id.is_empty()
                && agent.id.len() <= 32
                && agent.id.bytes().enumerate().all(|(i, c)| {
                    c.is_ascii_lowercase() || c.is_ascii_digit() || (i > 0 && c == b'-')
                });
        if !valid_id {
            return Err(format!(
                "{item}.id は英小文字・数字で始まる 1〜32 文字の英小文字・数字・ハイフンにしてください"
            ));
        }
        if !ids.insert(&agent.id) {
            return Err(format!("{item}.id が重複しています"));
        }
        if agent.name.trim().is_empty() || agent.name.chars().count() > 40 {
            return Err(format!("{item}.name は 1〜40 文字にしてください"));
        }
        if agent.program.trim().is_empty() {
            return Err(format!("{item}.program を入力してください"));
        }
        if agent.role.chars().count() > 400 {
            return Err(format!("{item}.role は 400 文字以下にしてください"));
        }
        if let Some(timeout) = agent.timeout_secs
            && !(30..=7200).contains(&timeout)
        {
            return Err(format!(
                "{item}.timeoutSecs は 30〜7200 の範囲にしてください"
            ));
        }
        validate_args(&format!("{item}.args"), &agent.args, 64)?;
        let prompt = agent.args.iter().any(|arg| arg.contains("{prompt}"));
        let file = agent.args.iter().any(|arg| arg.contains("{promptFile}"));
        if prompt != (agent.prompt_via == PromptVia::Arg)
            || file != (agent.prompt_via == PromptVia::File)
        {
            return Err(format!(
                "{item}.promptVia と args の {{prompt}}・{{promptFile}} が一致していません"
            ));
        }
        let program = agent.program.to_ascii_lowercase();
        if agent.prompt_via == PromptVia::Arg
            && (program.ends_with(".cmd") || program.ends_with(".bat"))
        {
            return Err(format!(
                "{item}.promptVia に arg は使えません。.cmd/.bat は cmd.exe が引数を解釈し直すため、stdin または file を使ってください"
            ));
        }
        if !agent.allow_dangerous
            && let Some(flag) = dangerous_flag(&agent.args)
        {
            return Err(format!(
                "{item}.args の危険な指定「{flag}」には allowDangerous が必要です"
            ));
        }
    }
    Ok(())
}

fn validate_args(name: &str, args: &[String], limit: usize) -> Result<(), String> {
    if args.len() > limit {
        return Err(format!("{name} は {limit} 個以下にしてください"));
    }
    if let Some(index) = args.iter().position(|arg| arg.chars().count() > 4096) {
        return Err(format!(
            "{name}[{}] は 4096 文字以下にしてください",
            index + 1
        ));
    }
    Ok(())
}

pub fn expand(
    args: &[String],
    prompt: Option<&str>,
    prompt_file: Option<&Path>,
    cwd: &Path,
) -> Vec<String> {
    args.iter()
        .map(|arg| {
            // Substitute only the original template, never placeholders inside the prompt.
            let mut expanded = String::new();
            let mut rest = arg.as_str();
            while let Some(index) = rest.find('{') {
                expanded.push_str(&rest[..index]);
                rest = &rest[index..];
                let matched = if let Some(tail) = rest.strip_prefix("{prompt}") {
                    expanded.push_str(prompt.unwrap_or("{prompt}"));
                    Some(tail)
                } else if let Some(tail) = rest.strip_prefix("{promptFile}") {
                    expanded.push_str(
                        &prompt_file
                            .map(|path| path.to_string_lossy())
                            .unwrap_or_else(|| "{promptFile}".into()),
                    );
                    Some(tail)
                } else if let Some(tail) = rest.strip_prefix("{cwd}") {
                    expanded.push_str(&cwd.to_string_lossy());
                    Some(tail)
                } else {
                    None
                };
                if let Some(tail) = matched {
                    rest = tail;
                } else {
                    expanded.push('{');
                    rest = &rest[1..];
                }
            }
            expanded.push_str(rest);
            expanded
        })
        .collect()
}

pub fn load(path: &Path) -> Registry {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Registry>(&text).ok())
        .filter(|registry| validate(registry).is_ok())
        .unwrap_or_else(defaults)
}

pub fn save(path: &Path, registry: &Registry) -> Result<(), String> {
    validate(registry)?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .map_err(|error| format!("設定フォルダを作れませんでした: {error}"))?;
    let temp = parent.join(format!(".agents-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let text = serde_json::to_string_pretty(registry)
            .map_err(|error| format!("設定を変換できませんでした: {error}"))?;
        fs::write(&temp, text).map_err(|error| format!("設定を書き込めませんでした: {error}"))?;
        fs::rename(&temp, path).map_err(|error| format!("設定を保存できませんでした: {error}"))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).into()).collect()
    }

    #[test]
    fn defaults_and_json_contract() {
        let registry = defaults();
        assert!(validate(&registry).is_ok());
        let value = serde_json::to_value(&registry).unwrap();
        assert_eq!(value["limits"]["maxDispatchesPerTask"], 10);
        assert_eq!(value["agents"][0]["promptVia"], "stdin");
        assert_eq!(value["agents"][0]["allowDangerous"], false);
        assert_eq!(value["agents"][0]["output"], "claudeStreamJson");
        assert_eq!(value["agents"][3]["enabled"], true);
        assert_eq!(value["agents"][3]["output"], "antigravityJson");
        assert_eq!(
            value["agents"][3]["args"],
            serde_json::json!([
                "--output-format",
                "json",
                "--mode",
                "accept-edits",
                "--sandbox",
                "-p={prompt}"
            ])
        );
        assert_eq!(registry.limits, Limits::default());
        let mut old = value;
        old.as_object_mut().unwrap().remove("limits");
        for agent in old["agents"].as_array_mut().unwrap() {
            agent.as_object_mut().unwrap().remove("timeoutSecs");
        }
        assert_eq!(serde_json::from_value::<Registry>(old).unwrap(), registry);
    }

    #[test]
    fn dangerous_flags_require_explicit_opt_in() {
        for args in [
            vec!["--dangerously-skip-permissions"],
            vec!["--allow-dangerously-skip-permissions=true"],
            vec!["--dangerously-bypass-approvals-and-sandbox"],
            vec!["--dangerously-bypass-hook-trust"],
            vec!["--YOLO"],
            vec!["--always-approve=false"],
            vec!["--permission-mode", "bypassPermissions"],
            vec!["--mode=danger-full-access"],
            vec!["danger-full-access"],
            vec!["--approve", "ALL"],
            vec!["--approve=all"],
            vec!["-c", "sandbox_mode=\"danger-full-access\""],
            vec![
                "--settings",
                r#"{"permissions":{"defaultMode":"bypassPermissions"}}"#,
            ],
            vec!["--approval-mode", "yolo"],
            vec!["--approval-mode='yolo'"],
            vec![
                "--settings",
                r#"{"permissions":{"defaultMode":"bypass\u0050ermissions"}}"#,
            ],
            vec![r#"--settings={"sandbox":"danger\u002dfull\u002daccess"}"#],
            vec![r#"{"\u0062ypassPermissions":true}"#],
        ] {
            let args = strings(&args);
            assert!(dangerous_flag(&args).is_some(), "{args:?}");
            let mut registry = defaults();
            registry.agents[0].args = args.clone();
            assert!(validate(&registry).is_err(), "{args:?}");
            registry.agents[0].allow_dangerous = true;
            assert!(validate(&registry).is_ok(), "{args:?}");
        }
        assert!(
            dangerous_flag(&strings(&[
                "--approve",
                "safe",
                "--sandbox",
                "workspace-write"
            ]))
            .is_none()
        );
        assert!(dangerous_flag(&strings(&["--permission-mode", "auto"])).is_none());
    }

    #[test]
    fn validates_ids_and_prompt_delivery() {
        for id in [
            "",
            "Bad",
            "-bad",
            "has_space",
            "あ",
            "012345678901234567890123456789012",
        ] {
            let mut registry = defaults();
            registry.agents[0].id = id.into();
            assert!(validate(&registry).is_err(), "{id}");
        }
        let mut registry = defaults();
        registry.agents[1].id = registry.agents[0].id.clone();
        assert!(validate(&registry).is_err());
        for (via, args) in [
            (PromptVia::Arg, vec![]),
            (PromptVia::File, vec![]),
            (PromptVia::Stdin, vec!["{prompt}"]),
            (PromptVia::Stdin, vec!["{promptFile}"]),
            (PromptVia::Arg, vec!["{prompt}", "{promptFile}"]),
            (PromptVia::File, vec!["{prompt}", "{promptFile}"]),
        ] {
            let mut registry = defaults();
            registry.agents[0].program = "test.exe".into();
            registry.agents[0].prompt_via = via;
            registry.agents[0].args = strings(&args);
            assert!(validate(&registry).is_err());
        }
        for program in ["script.cmd", "script.BAT"] {
            let mut registry = defaults();
            registry.agents[0].program = program.into();
            registry.agents[0].prompt_via = PromptVia::Arg;
            registry.agents[0].args = strings(&["{prompt}"]);
            assert!(validate(&registry).unwrap_err().contains("cmd.exe"));
        }
    }

    #[test]
    fn validates_lengths_and_numeric_bounds() {
        let invalid: Vec<Box<dyn Fn(&mut Registry)>> = vec![
            Box::new(|r| r.version = 2),
            Box::new(|r| r.limits.max_dispatches_per_task = 0),
            Box::new(|r| r.limits.max_dispatches_per_task = 101),
            Box::new(|r| r.limits.max_consecutive_per_project = 0),
            Box::new(|r| r.limits.max_consecutive_per_project = 501),
            Box::new(|r| r.limits.max_queue = 0),
            Box::new(|r| r.limits.max_queue = 101),
            Box::new(|r| r.limits.same_error_limit = 0),
            Box::new(|r| r.limits.same_error_limit = 11),
            Box::new(|r| r.limits.default_timeout_secs = 29),
            Box::new(|r| r.limits.default_timeout_secs = 7201),
            Box::new(|r| r.agents[0].timeout_secs = Some(29)),
            Box::new(|r| r.agents[0].timeout_secs = Some(7201)),
            Box::new(|r| r.limits.log_tail_bytes = 255),
            Box::new(|r| r.agents = vec![r.agents[0].clone(); 17]),
            Box::new(|r| r.agents[0].name.clear()),
            Box::new(|r| r.agents[0].name = "あ".repeat(41)),
            Box::new(|r| r.agents[0].program.clear()),
            Box::new(|r| r.agents[0].args = vec!["x".into(); 65]),
            Box::new(|r| r.agents[0].args = vec!["あ".repeat(4097)]),
            Box::new(|r| r.agents[0].role = "あ".repeat(401)),
        ];
        for change in invalid {
            let mut registry = defaults();
            change(&mut registry);
            assert!(validate(&registry).is_err());
        }
        let mut empty = defaults();
        empty.agents.clear();
        assert!(validate(&empty).is_ok());
        let mut edges = defaults();
        for (dispatches, consecutive, errors, timeout, queue, tail) in
            [(1, 1, 1, 30, 1, 256), (100, 500, 10, 7200, 100, 65536)]
        {
            edges.limits = Limits {
                max_dispatches_per_task: dispatches,
                max_consecutive_per_project: consecutive,
                same_error_limit: errors,
                default_timeout_secs: timeout,
                max_queue: queue,
                pause_on_deletion: false,
                log_tail_bytes: tail,
            };
            edges.agents[0].timeout_secs = Some(timeout);
            assert!(validate(&edges).is_ok());
        }
    }

    #[test]
    fn expansion_preserves_other_braces_and_prompt_contents() {
        let args = strings(&[
            "{prompt}",
            "prefix={promptFile}",
            "{cwd}/{unknown}",
            "{\"json\": true}",
            "{{cwd}}",
        ]);
        assert_eq!(
            expand(
                &args,
                Some("keep {cwd}"),
                Some(Path::new("prompt.txt")),
                Path::new("project")
            ),
            strings(&[
                "keep {cwd}",
                "prefix=prompt.txt",
                "project/{unknown}",
                "{\"json\": true}",
                "{project}"
            ])
        );
    }

    #[test]
    fn save_load_and_invalid_file_fallback() {
        let dir = std::env::temp_dir().join(format!("gbd-config-{}", uuid::Uuid::new_v4()));
        let path = dir.join("nested").join("agents.json");
        let mut registry = defaults();
        registry.limits.max_dispatches_per_task = 23;
        assert_eq!(load(&path), defaults());
        save(&path, &registry).unwrap();
        assert_eq!(load(&path), registry);
        registry.limits.max_dispatches_per_task = 24;
        save(&path, &registry).unwrap();
        assert_eq!(load(&path), registry);
        fs::write(&path, "broken").unwrap();
        assert_eq!(load(&path), defaults());
        assert_eq!(fs::read_to_string(&path).unwrap(), "broken");
        registry.version = 9;
        fs::write(&path, serde_json::to_string(&registry).unwrap()).unwrap();
        assert_eq!(load(&path), defaults());
        assert!(save(&path, &registry).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
