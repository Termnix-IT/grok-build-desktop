//! Decides which tool permissions can be approved without a card.
//! Safe work may use `allow_once` only. `allow_always` is never chosen here.
//! Anything that is not clearly safe asks the user.

use std::path::Path;

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Review {
    pub allow_once: Option<String>,
    pub reason: String,
    pub detail: String,
}

pub fn review(tool: &Value, options: &Value, folder: Option<&Path>) -> Review {
    let detail = detail_of(tool);
    let reason = match action_of(tool) {
        Action::Allow => String::new(),
        Action::Edit => edit_reason(tool, folder).unwrap_or_default(),
        Action::Execute => {
            execute_reason(&command_text(tool.get("rawInput")).unwrap_or_default()).unwrap_or_default()
        }
        Action::Ask(reason) => reason.to_string(),
    };
    let allow_once = if reason.is_empty() {
        allow_once_id(options)
    } else {
        None
    };
    Review {
        allow_once,
        reason,
        detail,
    }
}

enum Action {
    Allow,
    Edit,
    Execute,
    Ask(&'static str),
}

fn action_of(tool: &Value) -> Action {
    let name = tool.get("name").and_then(Value::as_str).unwrap_or("");
    let kind = tool.get("kind").and_then(Value::as_str).unwrap_or("");
    let name = program_name(name);
    if kind == "execute"
        || matches!(
            name.as_str(),
            "bash"
                | "shell"
                | "powershell"
                | "pwsh"
                | "cmd"
                | "execute"
                | "run_terminal_command"
                | "terminal"
        )
    {
        return Action::Execute;
    }
    if kind == "delete" || matches!(name.as_str(), "delete" | "remove" | "unlink") {
        return Action::Ask("ファイルを削除します");
    }
    if kind == "move" || matches!(name.as_str(), "move" | "rename" | "mv") {
        return Action::Ask("ファイルを移動します");
    }
    if kind == "fetch" || matches!(name.as_str(), "fetch" | "webfetch" | "web_fetch" | "http") {
        return Action::Ask("外部から取得します");
    }
    if kind == "switch_mode" || name.contains("switch_mode") {
        return Action::Ask("セッションのモードを変えます");
    }
    if kind == "edit" || matches!(name.as_str(), "edit" | "write" | "apply_patch" | "strreplace") {
        return Action::Edit;
    }
    if matches!(kind, "read" | "search" | "think")
        || matches!(
            name.as_str(),
            "read" | "read_file" | "grep" | "glob" | "search" | "list"
        )
    {
        return Action::Allow;
    }
    Action::Ask("操作の種類が分からないため、許可が必要です")
}

fn edit_reason(tool: &Value, folder: Option<&Path>) -> Option<String> {
    let folder = folder?;
    let root = norm_path(&folder.to_string_lossy());
    if root.is_empty() {
        return None;
    }
    let outside = paths_of(tool).into_iter().any(|path| {
        let path = path.trim();
        if path.is_empty() || Path::new(path).is_relative() {
            return false;
        }
        !within(&norm_path(path), &root)
    });
    if outside {
        Some("プロジェクトの外を変更するため、許可が必要です".to_string())
    } else {
        None
    }
}

fn allow_once_id(options: &Value) -> Option<String> {
    options.as_array().and_then(|items| {
        items.iter().find_map(|item| {
            if item.get("kind").and_then(Value::as_str) != Some("allow_once") {
                return None;
            }
            item.get("optionId")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_string)
        })
    })
}

fn detail_of(tool: &Value) -> String {
    if let Some(command) = command_text(tool.get("rawInput")) {
        return clip(&command);
    }
    paths_of(tool)
        .into_iter()
        .find(|path| !path.is_empty())
        .map(|path| clip(&path))
        .or_else(|| {
            tool.get("title")
                .and_then(Value::as_str)
                .filter(|title| !title.is_empty())
                .map(clip)
        })
        .unwrap_or_default()
}

fn paths_of(tool: &Value) -> Vec<String> {
    let mut paths = Vec::new();
    if let Some(locations) = tool.get("locations").and_then(Value::as_array) {
        for location in locations {
            if let Some(path) = location.get("path").and_then(Value::as_str) {
                paths.push(path.to_string());
            }
        }
    }
    collect_paths(tool.get("rawInput"), &mut paths);
    paths
}

fn collect_paths(value: Option<&Value>, paths: &mut Vec<String>) {
    let Some(object) = value.and_then(Value::as_object) else {
        return;
    };
    for key in ["path", "filePath", "file_path", "target", "uri", "absPath", "absolutePath"] {
        if let Some(path) = object.get(key).and_then(Value::as_str) {
            paths.push(path.to_string());
        }
    }
}

fn command_text(value: Option<&Value>) -> Option<String> {
    let value = value?;
    if let Some(text) = value.as_str() {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        return Some(text.to_string());
    }
    let object = value.as_object()?;
    for key in ["command", "cmd", "script", "commandLine", "command_line"] {
        let Some(raw) = object.get(key) else {
            continue;
        };
        if let Some(text) = raw.as_str() {
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            if key == "command" {
                if let Some(args) = object.get("args").and_then(Value::as_array) {
                    let mut parts = vec![text.to_string()];
                    for arg in args {
                        if let Some(arg) = arg.as_str() {
                            parts.push(arg.to_string());
                        }
                    }
                    return Some(parts.join(" "));
                }
            }
            return Some(text.to_string());
        }
        if let Some(items) = raw.as_array() {
            let parts: Vec<&str> = items.iter().filter_map(Value::as_str).collect();
            if !parts.is_empty() {
                return Some(parts.join(" "));
            }
        }
    }
    None
}

fn execute_reason(command: &str) -> Option<String> {
    let command = command.trim();
    if command.is_empty() {
        return Some("コマンドの内容を読めません".to_string());
    }
    if obfuscated(command) {
        return Some("コマンドの内容を確認できないため、許可が必要です".to_string());
    }
    let command = match unwrap_host(command) {
        Ok(inner) => inner,
        Err(reason) => return Some(reason),
    };
    if obfuscated(&command) {
        return Some("コマンドの内容を確認できないため、許可が必要です".to_string());
    }
    if has_redirect(&command) {
        return Some("ファイルへの書き出しを含むため、許可が必要です".to_string());
    }
    let segments = match split_segments(&command) {
        Ok(parts) => parts,
        Err(()) => return Some("コマンドの内容を確認できないため、許可が必要です".to_string()),
    };
    if segments.is_empty() {
        return Some("コマンドの内容を読めません".to_string());
    }
    for segment in segments {
        if let Some(reason) = segment_reason(&segment) {
            return Some(reason.to_string());
        }
    }
    None
}

fn unwrap_host(command: &str) -> Result<String, String> {
    let tokens = tokenize(command)
        .map_err(|()| "コマンドの内容を確認できないため、許可が必要です".to_string())?;
    let Some(host) = tokens.first() else {
        return Err("コマンドの内容を読めません".to_string());
    };
    let host = program_name(host);
    if host == "cmd" {
        let flag = tokens.get(1).map(|item| item.to_ascii_lowercase());
        if matches!(flag.as_deref(), Some("/c" | "/k")) {
            let inner = tokens[2..].join(" ");
            if inner.trim().is_empty() {
                return Err("コマンドの内容を読めません".to_string());
            }
            return Ok(inner);
        }
        return Ok(command.to_string());
    }
    if host == "powershell" || host == "pwsh" {
        if tokens.iter().any(|token| {
            let flag = token.to_ascii_lowercase();
            matches!(flag.as_str(), "-encodedcommand" | "-enc" | "-e")
        }) {
            return Err("コマンドの内容を確認できないため、許可が必要です".to_string());
        }
        if let Some(index) = tokens.iter().position(|token| {
            let flag = token.to_ascii_lowercase();
            matches!(flag.as_str(), "-command" | "-c")
        }) {
            let inner = tokens[index + 1..].join(" ");
            if inner.trim().is_empty() {
                return Err("コマンドの内容を読めません".to_string());
            }
            return Ok(inner);
        }
    }
    Ok(command.to_string())
}

fn obfuscated(input: &str) -> bool {
    let lower = input.to_ascii_lowercase();
    if lower.contains("$(") || input.contains('`') || lower.contains("invoke-expression") {
        return true;
    }
    loose_tokens(&lower).any(|token| {
        matches!(
            token,
            "iex" | "encodedcommand" | "-encodedcommand" | "-enc"
        )
    })
}

fn has_redirect(input: &str) -> bool {
    let mut quote = None;
    for ch in input.chars() {
        if let Some(mark) = quote {
            if ch == mark {
                quote = None;
            }
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            '>' | '<' => return true,
            _ => {}
        }
    }
    false
}

fn split_segments(input: &str) -> Result<Vec<String>, ()> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut chars = input.chars().peekable();
    let mut quote = None;
    while let Some(ch) = chars.next() {
        if let Some(mark) = quote {
            current.push(ch);
            if ch == mark {
                quote = None;
            }
            continue;
        }
        match ch {
            '\'' | '"' => {
                quote = Some(ch);
                current.push(ch);
            }
            '\n' | ';' => push_part(&mut parts, &mut current),
            '&' => {
                if chars.peek() == Some(&'&') {
                    chars.next();
                    push_part(&mut parts, &mut current);
                } else {
                    return Err(());
                }
            }
            '|' => {
                if chars.peek() == Some(&'|') {
                    chars.next();
                }
                push_part(&mut parts, &mut current);
            }
            _ => current.push(ch),
        }
    }
    if quote.is_some() {
        return Err(());
    }
    push_part(&mut parts, &mut current);
    Ok(parts)
}

fn push_part(parts: &mut Vec<String>, current: &mut String) {
    let part = current.trim();
    if !part.is_empty() {
        parts.push(part.to_string());
    }
    current.clear();
}

fn segment_reason(segment: &str) -> Option<&'static str> {
    let tokens = match tokenize(segment) {
        Ok(tokens) => tokens,
        Err(()) => return Some("コマンドの内容を確認できないため、許可が必要です"),
    };
    if tokens.is_empty() {
        return Some("コマンドの内容を読めません");
    }
    let program = program_name(&tokens[0]);
    if let Some(reason) = danger_program(&program) {
        return Some(reason);
    }
    match program.as_str() {
        "git" => git_reason(&tokens[1..]),
        "cargo" => cargo_reason(&tokens[1..]),
        "go" => go_reason(&tokens[1..]),
        "dotnet" => dotnet_reason(&tokens[1..]),
        "npm" | "pnpm" | "yarn" => package_reason(&tokens[1..]),
        "npx" => npx_reason(&tokens[1..]),
        "tsc" => tsc_reason(&tokens[1..]),
        "node" | "python" | "python3" | "py" => version_only(&tokens[1..]),
        "ls" | "dir" | "pwd" | "cd" | "echo" | "type" | "cat" | "head" | "tail" | "more"
        | "findstr" | "where" | "which" | "rg" | "grep" | "get-childitem" | "get-content"
        | "get-location" | "get-item" | "get-command" | "select-string" | "get-date"
        | "write-output" | "write-host" => None,
        _ => Some("このコマンドは自動では許可しません"),
    }
}

fn danger_program(program: &str) -> Option<&'static str> {
    match program {
        "rm" | "rmdir" | "del" | "erase" | "rd" | "unlink" | "remove-item" | "ri" | "shred" => {
            Some("ファイルを削除するコマンドのため、許可が必要です")
        }
        "format" | "diskpart" | "cipher" | "mkfs" | "dd" => {
            Some("ディスクを変更するコマンドのため、許可が必要です")
        }
        "shutdown" | "restart-computer" | "stop-computer" | "reboot" => {
            Some("システムの停止を含むため、許可が必要です")
        }
        "taskkill" | "stop-process" | "kill" | "pkill" => {
            Some("プロセスを止めるコマンドのため、許可が必要です")
        }
        "sudo" | "runas" | "gsudo" => Some("権限を上げるコマンドのため、許可が必要です"),
        "curl" | "wget" | "invoke-webrequest" | "invoke-restmethod" | "iwr" | "irm" | "ssh"
        | "scp" | "sftp" | "ftp" | "nc" | "telnet" => {
            Some("外部と通信するコマンドのため、許可が必要です")
        }
        "chmod" | "chown" | "icacls" | "takeown" | "cacls" => {
            Some("アクセス権を変えるコマンドのため、許可が必要です")
        }
        "reg" | "setx" => Some("システム設定を変えるコマンドのため、許可が必要です"),
        _ => None,
    }
}

fn git_reason(args: &[String]) -> Option<&'static str> {
    let Some(sub) = args.first() else {
        return Some("この Git の操作は自動では許可しません");
    };
    match sub.to_ascii_lowercase().as_str() {
        "status" | "diff" | "log" | "show" | "rev-parse" | "describe" | "blame" | "shortlog"
        | "ls-files" => None,
        "branch" => branch_reason(args),
        "remote" => remote_reason(args),
        "config" => config_reason(args),
        "push" | "fetch" | "pull" | "clone" => Some("外部と通信するコマンドのため、許可が必要です"),
        "reset" | "clean" | "checkout" | "restore" | "switch" => {
            Some("作業ツリーを変えるコマンドのため、許可が必要です")
        }
        "commit" | "rebase" | "merge" | "add" | "tag" | "stash" | "init" | "am" | "cherry-pick" => {
            Some("Git の履歴を変えるコマンドのため、許可が必要です")
        }
        _ => Some("この Git の操作は自動では許可しません"),
    }
}

fn branch_reason(args: &[String]) -> Option<&'static str> {
    let changes = args.iter().skip(1).any(|arg| {
        let lower = arg.to_ascii_lowercase();
        if !lower.starts_with('-') {
            return true;
        }
        matches!(lower.as_str(), "-d" | "-D" | "-m" | "-M" | "-f" | "--force")
            || lower.starts_with("--delete")
            || lower.starts_with("--move")
    });
    if changes {
        Some("ブランチを変えるコマンドのため、許可が必要です")
    } else {
        None
    }
}

fn remote_reason(args: &[String]) -> Option<&'static str> {
    let changes = args.iter().skip(1).any(|arg| {
        matches!(
            arg.to_ascii_lowercase().as_str(),
            "add" | "remove" | "rm" | "rename" | "set-url" | "prune"
        )
    });
    if changes {
        Some("リモート設定を変えるコマンドのため、許可が必要です")
    } else {
        None
    }
}

fn config_reason(args: &[String]) -> Option<&'static str> {
    let mut read_flag = false;
    for arg in args.iter().skip(1) {
        let lower = arg.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "--unset" | "--unset-all" | "--add" | "--replace-all" | "--edit" | "--remove-section" | "--rename-section"
        ) {
            return Some("Git の設定を変えるコマンドのため、許可が必要です");
        }
        if matches!(lower.as_str(), "--get" | "--get-all" | "--get-regexp" | "--list" | "-l") {
            read_flag = true;
        }
    }
    if read_flag {
        None
    } else {
        Some("Git の設定を変えるコマンドのため、許可が必要です")
    }
}

fn cargo_reason(args: &[String]) -> Option<&'static str> {
    match args.first().map(|item| item.to_ascii_lowercase()).as_deref() {
        Some("check" | "test" | "build" | "clippy" | "fmt" | "metadata" | "tree" | "doc") => None,
        Some("publish" | "install" | "login" | "yank") => Some("公開や導入を含むため、許可が必要です"),
        _ => Some("この Cargo の操作は自動では許可しません"),
    }
}

fn go_reason(args: &[String]) -> Option<&'static str> {
    match args.first().map(|item| item.to_ascii_lowercase()).as_deref() {
        Some("test" | "build" | "vet") => None,
        _ => Some("この Go の操作は自動では許可しません"),
    }
}

fn dotnet_reason(args: &[String]) -> Option<&'static str> {
    match args.first().map(|item| item.to_ascii_lowercase()).as_deref() {
        Some("build" | "test" | "--version" | "-v") => None,
        _ => Some("この .NET の操作は自動では許可しません"),
    }
}

fn package_reason(args: &[String]) -> Option<&'static str> {
    match args.first().map(|item| item.to_ascii_lowercase()).as_deref() {
        Some("ls" | "list" | "--version" | "-v") => None,
        Some("publish") => Some("公開や導入を含むため、許可が必要です"),
        Some("test" | "run" | "install" | "exec" | "start") => {
            Some("パッケージのスクリプトを実行するため、許可が必要です")
        }
        _ => Some("このコマンドは自動では許可しません"),
    }
}

fn npx_reason(args: &[String]) -> Option<&'static str> {
    if args.first().map(|item| item.to_ascii_lowercase()).as_deref() == Some("tsc") {
        return tsc_reason(&args[1..]);
    }
    Some("パッケージのスクリプトを実行するため、許可が必要です")
}

fn tsc_reason(args: &[String]) -> Option<&'static str> {
    let version = args.iter().any(|arg| arg == "--version" || arg == "-v")
        && args.iter().all(|arg| arg.starts_with('-'));
    if version || args.iter().any(|arg| arg == "--noEmit") {
        None
    } else {
        Some("このコマンドは自動では許可しません")
    }
}

fn version_only(args: &[String]) -> Option<&'static str> {
    let version = args.iter().any(|arg| arg == "--version" || arg == "-v" || arg == "-V")
        && args.iter().all(|arg| arg.starts_with('-'));
    if version {
        None
    } else {
        Some("このコマンドは自動では許可しません")
    }
}

fn tokenize(segment: &str) -> Result<Vec<String>, ()> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    for ch in segment.chars() {
        if let Some(mark) = quote {
            if ch == mark {
                quote = None;
                continue;
            }
            current.push(ch);
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            ch if ch.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if quote.is_some() {
        return Err(());
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    Ok(tokens)
}

fn loose_tokens(input: &str) -> impl Iterator<Item = &str> {
    input.split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'))
}

fn program_name(token: &str) -> String {
    let base = token.rsplit(['\\', '/']).next().unwrap_or(token);
    let lower = base.to_ascii_lowercase();
    lower
        .strip_suffix(".exe")
        .unwrap_or(&lower)
        .to_string()
}

fn norm_path(path: &str) -> String {
    path.replace('/', "\\").trim_end_matches('\\').to_ascii_lowercase()
}

fn within(path: &str, root: &str) -> bool {
    path == root || path.starts_with(&format!("{root}\\"))
}

fn clip(text: &str) -> String {
    let mut chars = text.chars();
    let clipped: String = chars.by_ref().take(500).collect();
    if chars.next().is_some() {
        format!("{clipped}…")
    } else {
        clipped
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn options() -> Value {
        json!([
            {"optionId": "once", "name": "Allow once", "kind": "allow_once"},
            {"optionId": "always", "name": "Allow always", "kind": "allow_always"},
            {"optionId": "no", "name": "Reject", "kind": "reject_once"}
        ])
    }

    fn tool(kind: &str, name: &str, raw: Value) -> Value {
        json!({
            "kind": kind,
            "name": name,
            "title": "作業",
            "rawInput": raw
        })
    }

    fn allows(kind: &str, name: &str, raw: Value) -> bool {
        review(&tool(kind, name, raw), &options(), None)
            .allow_once
            .as_deref()
            == Some("once")
    }

    fn reason_of(kind: &str, name: &str, raw: Value) -> String {
        review(&tool(kind, name, raw), &options(), None).reason
    }

    #[test]
    fn reads_edits_and_safe_commands_can_use_allow_once() {
        assert!(allows("read", "read", json!({"path": "src/main.ts"})));
        assert!(allows("search", "grep", json!({})));
        assert!(allows("think", "think", json!({})));
        assert!(allows("edit", "edit", json!({"filePath": "src/app.tsx"})));
        assert!(allows("execute", "bash", json!({"command": "git status"})));
        assert!(allows("execute", "bash", json!({"command": "git diff -- src/app.tsx"})));
        assert!(allows("execute", "bash", json!({"command": "cargo test --lib"})));
        assert!(allows(
            "execute",
            "powershell",
            json!({"command": "Get-Content README.md"})
        ));
        assert!(allows(
            "execute",
            "bash",
            json!({"command": "git status && cargo check"})
        ));
        assert!(allows(
            "execute",
            "bash",
            json!({"command": "powershell -NoProfile -Command git status"})
        ));
    }

    #[test]
    fn never_auto_selects_allow_always() {
        let options = json!([
            {"optionId": "always", "name": "Allow always", "kind": "allow_always"}
        ]);
        let verdict = review(&tool("read", "read", json!({})), &options, None);
        assert_eq!(verdict.allow_once, None);
        assert_eq!(verdict.reason, "");
    }

    #[test]
    fn destructive_and_unknown_commands_ask() {
        assert!(reason_of("delete", "delete", json!({"path": "a.ts"})).contains("削除"));
        assert!(reason_of("move", "move", json!({"path": "a.ts"})).contains("移動"));
        assert!(reason_of("fetch", "fetch", json!({})).contains("取得"));
        assert!(reason_of("switch_mode", "switch_mode", json!({})).contains("モード"));
        assert!(reason_of("other", "mystery", json!({})).contains("分からない"));
        assert!(reason_of("execute", "bash", json!({"command": "rm -rf dist"})).contains("削除"));
        assert!(reason_of(
            "execute",
            "bash",
            json!({"command": "git status && Remove-Item tmp"})
        )
        .contains("削除"));
        assert!(reason_of("execute", "bash", json!({"command": "curl https://example.com"}))
            .contains("通信"));
        assert!(reason_of("execute", "bash", json!({"command": "echo hi > out.txt"})).contains("書き出し"));
        assert!(reason_of("execute", "bash", json!({"command": "echo $(rm)"})).contains("確認できない"));
        assert!(reason_of("execute", "bash", json!({})).contains("読めません"));
        assert!(reason_of("execute", "bash", json!({"command": "git push origin main"})).contains("通信"));
        assert!(reason_of("execute", "bash", json!({"command": "git reset --hard"})).contains("作業ツリー"));
        assert!(reason_of("execute", "bash", json!({"command": "cargo publish"})).contains("公開"));
        assert!(reason_of("execute", "bash", json!({"command": "npm test"})).contains("スクリプト"));
        assert!(reason_of("execute", "bash", json!({"command": "python script.py"})).contains("自動では許可"));
        assert!(reason_of(
            "execute",
            "powershell",
            json!({"command": "powershell -EncodedCommand abc"})
        )
        .contains("確認できない"));
    }

    #[test]
    fn edits_outside_the_project_ask() {
        let folder = Path::new(r"C:\work\app");
        let inside = review(
            &tool("edit", "edit", json!({"filePath": r"C:\work\app\src\main.ts"})),
            &options(),
            Some(folder),
        );
        assert_eq!(inside.allow_once.as_deref(), Some("once"));
        let sibling = review(
            &tool("edit", "edit", json!({"filePath": r"C:\work\app-other\file.ts"})),
            &options(),
            Some(folder),
        );
        assert!(sibling.reason.contains("外"));
        assert_eq!(sibling.allow_once, None);
        let reading = review(
            &tool("read", "read", json!({"path": r"C:\Windows\system.ini"})),
            &options(),
            Some(folder),
        );
        assert_eq!(reading.allow_once.as_deref(), Some("once"));
    }
}
