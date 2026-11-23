use std::fs;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::time::{sleep, timeout};

use super::engine::{LogStream, OrchestratorEvent, RunStatus};
use super::testkit::{Kit, Record};
use super::{cli, config, ipc};

const QUICK: &str = "[Console]::Out.WriteLine('quick')";
const STDIN: &str = "$prompt = [Console]::In.ReadToEnd()";

struct AgentCase {
    id: &'static str,
    args: &'static [&'static str],
    read_prompt: &'static str,
    output: &'static str,
    denied: &'static [&'static str],
}

const AGENTS: &[AgentCase] = &[
    AgentCase {
        id: "claude",
        args: &[],
        read_prompt: STDIN,
        output: r#"
[Console]::Out.WriteLine('marker claude')
[Console]::Out.WriteLine('{"type":"assistant","message":{"content":[{"type":"text","text":"作業中"}]}}')
[Console]::Out.WriteLine('{"type":"result","subtype":"success","result":"claude 完了","permission_denials":[{"tool_name":"Write","tool_input":{"file_path":"blocked.txt"}}]}')
"#,
        denied: &["Write: blocked.txt"],
    },
    AgentCase {
        id: "codex",
        args: &["-C", "{cwd}"],
        read_prompt: r#"
if ($argv[-2] -cne '-C' -or $argv[-1] -cne (Get-Location).Path) {
    throw '作業フォルダの引数 -C が現在のフォルダと一致しません'
}
$prompt = [Console]::In.ReadToEnd()
"#,
        output: r#"
[Console]::Out.WriteLine('marker codex')
[Console]::Out.WriteLine('{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"作業中"}}')
[Console]::Out.WriteLine('{"type":"item.completed","item":{"id":"item_1","type":"agent_message","text":"codex 完了"}}')
[Console]::Out.WriteLine('{"type":"turn.completed"}')
"#,
        denied: &[],
    },
    AgentCase {
        id: "grok",
        args: &["--prompt-file", "{promptFile}", "--cwd", "{cwd}"],
        read_prompt: r#"
if ($argv[-2] -cne '--cwd' -or $argv[-1] -cne (Get-Location).Path) {
    throw '作業フォルダの引数 --cwd が現在のフォルダと一致しません'
}
if ($argv[-4] -cne '--prompt-file' -or [string]::IsNullOrEmpty($argv[-3])) {
    throw 'プロンプトファイルの引数がありません'
}
$prompt = [IO.File]::ReadAllText($argv[-3], $utf8)
"#,
        output: r#"
[Console]::Out.WriteLine('marker grok')
[Console]::Out.WriteLine('{"type":"text","data":"作業中"}')
[Console]::Out.WriteLine('{"type":"tool_call","toolCallId":"c1","title":"write_file","status":"pending"}')
[Console]::Error.WriteLine('permission required: Write blocked.txt')
[Console]::Out.WriteLine('{"type":"text","data":"grok "}')
[Console]::Out.WriteLine('{"type":"text","data":"完了"}')
[Console]::Out.WriteLine('{"type":"end","stopReason":"end_turn","sessionId":"s"}')
"#,
        denied: &["Write blocked.txt"],
    },
    AgentCase {
        id: "antigravity",
        args: &["-p={prompt}"],
        read_prompt: r#"
if (-not $argv[-1].StartsWith('-p=', [StringComparison]::Ordinal)) {
    throw 'プロンプトの引数 -p= がありません'
}
$prompt = $argv[-1].Substring(3)
"#,
        output: r#"
[Console]::Out.WriteLine('marker antigravity')
[Console]::Out.WriteLine(@'
{
  "conversation_id": "c",
  "status": "SUCCESS",
  "response": "antigravity 完了",
  "denied_actions": [{"display_name":"Bash","action":"command"}]
}
'@)
"#,
        denied: &["Bash (command)"],
    },
    AgentCase {
        id: "custom",
        args: &[],
        read_prompt: STDIN,
        // Text uses the stdout tail verbatim as the summary.
        output: r#"
[Console]::Error.WriteLine('marker custom')
[Console]::Out.WriteLine('custom 完了')
"#,
        denied: &[],
    },
];

fn prompt_for(id: &str) -> String {
    format!(r#"「{id}」さんへの依頼です。日本語と記号 "引用" & 100%"#)
}

fn prefix_for(id: &str) -> String {
    match id {
        "claude" | "antigravity" => "実行時の注意事項".into(),
        "grok" => {
            config::defaults()
                .agents
                .into_iter()
                .find(|agent| agent.id == id)
                .unwrap()
                .prompt_prefix
        }
        _ => String::new(),
    }
}

fn delivered_prompt(id: &str) -> String {
    let prefix = prefix_for(id);
    let prompt = prompt_for(id);
    if prefix.is_empty() {
        prompt
    } else {
        format!("{prefix}\n\n{prompt}")
    }
}

fn agent_script(case: &AgentCase) -> String {
    let id = case.id;
    let expected = delivered_prompt(id);
    let read_prompt = case.read_prompt;
    let output = case.output;
    // PowerShell's script parameter parsing can reinterpret switches after -File.
    format!(
        r#"
$ErrorActionPreference = 'Stop'
try {{
    $argv = [Environment]::GetCommandLineArgs()
    {read_prompt}
    if ($prompt -cne '{expected}') {{
        throw "{id} に届いたプロンプトが期待値と一致しません: <$prompt>"
    }}
    [IO.File]::WriteAllText((Join-Path (Get-Location).Path '{id}.txt'), $prompt, $utf8)
}} catch {{
    [Console]::Error.WriteLine($_.Exception.Message)
    exit 9
}}
{output}
"#
    )
}

async fn serving(kit: &Kit) -> String {
    let pipe = format!(r"\\.\pipe\gbd-test-{}", uuid::Uuid::new_v4());
    let hub = kit.hub.clone();
    let server_pipe = pipe.clone();
    tokio::spawn(async move { ipc::serve(hub, &server_pipe).await });
    timeout(Duration::from_secs(5), async {
        loop {
            let gateway = kit.hub.gateway_view();
            match gateway["status"]["state"].as_str() {
                Some("listening") => break,
                Some("failed") => panic!("パイプを起動できませんでした: {gateway}"),
                _ => sleep(Duration::from_millis(10)).await,
            }
        }
    })
    .await
    .expect("パイプが待ち受けを開始しませんでした");
    pipe
}

async fn call(pipe: &str, args: &[&str], input: Vec<u8>, expected_exit: i32) -> Value {
    let cli_args = args.iter().map(std::ffi::OsString::from).collect();
    let pipe = pipe.to_owned();
    let (text, exit) =
        tokio::task::spawn_blocking(move || cli::run(cli_args, &mut input.as_slice(), &pipe))
            .await
            .unwrap();
    assert_eq!(exit, expected_exit, "gbd {args:?}: {text}");
    let value: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(value["ok"], expected_exit == 0, "gbd {args:?}: {value}");
    value
}

async fn gbd(pipe: &str, args: &[&str]) -> Value {
    call(pipe, args, Vec::new(), 0).await
}

fn job(kit: &Kit, agent: &str, prompt: &str, task_id: Option<&str>) -> Value {
    let mut job = json!({"project": kit.project(), "agent": agent, "prompt": prompt});
    if let Some(id) = task_id {
        job["task_id"] = json!(id);
    }
    job
}

async fn submit(pipe: &str, job: &Value, expected_exit: i32) -> Value {
    call(
        pipe,
        &["submit", "-"],
        serde_json::to_vec(job).unwrap(),
        expected_exit,
    )
    .await
}

fn has_log(log: &Value, stream: &str, text: &str) -> bool {
    log["lines"]
        .as_array()
        .unwrap()
        .iter()
        .any(|line| line["stream"] == stream && line["text"] == text)
}

async fn until_response(pipe: &str, args: &[&str], matches: impl Fn(&Value) -> bool) -> Value {
    let mut last = Value::Null;
    timeout(Duration::from_secs(20), async {
        loop {
            last = gbd(pipe, args).await;
            if matches(&last) {
                break;
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("gbd {args:?} が期待する応答を返しませんでした: {last}"));
    last
}

#[tokio::test]
async fn each_agent_kind_runs_through_gbd() {
    let scripts: Vec<_> = AGENTS.iter().map(agent_script).collect();
    let agents: Vec<_> = AGENTS
        .iter()
        .zip(&scripts)
        .map(|(case, script)| (case.id, script.as_str()))
        .collect();
    let kit = Kit::new(&agents, |registry| {
        let defaults = config::defaults();
        for (agent, case) in registry.agents.iter_mut().zip(AGENTS) {
            if case.id != "custom" {
                let spec = defaults
                    .agents
                    .iter()
                    .find(|spec| spec.id == case.id)
                    .unwrap();
                agent.kind = spec.kind;
                agent.output = spec.output;
                agent.prompt_via = spec.prompt_via;
            }
            agent.prompt_prefix = prefix_for(case.id);
            agent.args.extend(case.args.iter().map(|arg| (*arg).into()));
        }
    })
    .await;
    let pipe = serving(&kit).await;
    let version = gbd(&pipe, &["version"]).await;
    assert_eq!(version["app_running"], true);
    assert_eq!(version["accepting"], true);
    let available = gbd(&pipe, &["agents"]).await;
    let available = available["agents"].as_array().unwrap();
    assert_eq!(available.len(), AGENTS.len());
    for case in AGENTS {
        assert!(
            available
                .iter()
                .any(|agent| agent["id"] == case.id && agent["available"] == true),
            "{}: {available:?}",
            case.id
        );
    }

    let mut task_id: Option<String> = None;
    for (index, case) in AGENTS.iter().enumerate() {
        let id = case.id;
        let prompt = prompt_for(id);
        let request = job(&kit, id, &prompt, task_id.as_deref());
        let submitted = if index == 0 {
            let file = kit.root.join("job.json");
            fs::write(&file, format!("\u{feff}{request}")).unwrap();
            gbd(&pipe, &["submit", file.to_str().unwrap()]).await
        } else {
            submit(&pipe, &request, 0).await
        };
        let job_id = submitted["job_id"].as_str().unwrap();
        let task = task_id.get_or_insert_with(|| submitted["task_id"].as_str().unwrap().to_owned());
        assert_eq!(submitted["task_id"], task.as_str());
        let done = gbd(&pipe, &["wait", job_id, "--timeout", "60"]).await;
        assert_eq!(done["status"], "succeeded", "{done}");
        assert_eq!(done["exit_code"], 0, "{done}");
        assert_eq!(done["summary"], format!("{id} 完了"), "{done}");
        assert_eq!(done["denied"], json!(case.denied), "{done}");
        assert_eq!(done["task_id"], task.as_str());
        assert_eq!(done["task"]["jobs"], index + 1);
        let tree = done["recovery_point"].as_str().unwrap();
        assert_eq!(
            done["recovery_command"],
            format!("git restore --source={tree} --worktree -- .")
        );

        let file = format!("{id}.txt");
        let changes = done["changed_files"].as_array().unwrap();
        assert_eq!(changes.len(), 1, "{done}");
        assert_eq!(changes[0]["path"], file);
        assert_eq!(changes[0]["change"], "untracked");
        assert_eq!(
            fs::read_to_string(kit.repo.join(file)).unwrap(),
            delivered_prompt(id)
        );
        assert_eq!(kit.hub.job(job_id).unwrap().run.prompt, prompt);

        let brief = gbd(&pipe, &["log", job_id, "--tail", "200"]).await;
        let raw = gbd(&pipe, &["log", job_id, "--raw", "--tail=200"]).await;
        let stream = if id == "custom" { "stderr" } else { "stdout" };
        assert!(has_log(&raw, stream, &format!("marker {id}")), "{raw}");
        let line = match id {
            "codex" | "grok" => format!("[文章] {id} 完了"),
            "claude" | "antigravity" => format!("[結果] {id} 完了"),
            _ => format!("{id} 完了"),
        };
        assert!(has_log(&brief, "stdout", &line), "{brief}");
        if id == "custom" {
            assert!(
                has_log(&brief, "stderr", "[stderr] marker custom"),
                "{brief}"
            );
        } else {
            let contains_json = |log: &Value| {
                log["lines"].as_array().unwrap().iter().any(|line| {
                    line["stream"] == "stdout" && line["text"].as_str().unwrap().starts_with('{')
                })
            };
            assert!(contains_json(&raw), "{raw}");
            assert!(!contains_json(&brief), "{brief}");
        }
    }
}

#[tokio::test]
async fn a_running_job_can_be_watched_through_gbd() {
    const SLOW: &str = r#"
[Console]::Out.WriteLine('started')
$clock = [Diagnostics.Stopwatch]::StartNew()
while (-not (Test-Path -LiteralPath 'go' -PathType Leaf)) {
    if ($clock.Elapsed.TotalSeconds -ge 30) {
        [Console]::Error.WriteLine('go が作られるまでの待ち時間を超えました')
        exit 1
    }
    Start-Sleep -Milliseconds 50
}
[Console]::Out.WriteLine('finished')
"#;
    let kit = Kit::new(&[("slow", SLOW), ("quick", QUICK)], |_| {}).await;
    let pipe = serving(&kit).await;
    let slow = submit(&pipe, &job(&kit, "slow", "待機", None), 0).await;
    let slow_id = slow["job_id"].as_str().unwrap();
    let running = until_response(&pipe, &["status", slow_id], |value| {
        value["status"] == "running"
    })
    .await;
    assert!(running["elapsed_secs"].is_number(), "{running}");

    let quick = submit(&pipe, &job(&kit, "quick", "すぐに終了", None), 0).await;
    assert_ne!(quick["task_id"], slow["task_id"]);
    let quick_id = quick["job_id"].as_str().unwrap();
    let queued = gbd(&pipe, &["status", quick_id]).await;
    assert_eq!(queued["status"], "queued", "{queued}");
    let queued_result = gbd(&pipe, &["result", quick_id]).await;
    assert!(
        queued_result.get("recovery_command").unwrap().is_null(),
        "{queued_result}"
    );
    assert!(
        queued["position"].as_u64().is_some_and(|n| n >= 1),
        "{queued}"
    );

    let log = until_response(&pipe, &["log", slow_id, "--tail", "5"], |value| {
        has_log(value, "stdout", "started")
    })
    .await;
    assert!(!has_log(&log, "stdout", "finished"), "{log}");
    let waiting = gbd(&pipe, &["wait", slow_id, "--timeout", "1"]).await;
    assert_eq!(waiting["status"], "running", "{waiting}");

    fs::write(kit.repo.join("go"), b"").unwrap();
    for id in [slow_id, quick_id] {
        let done = gbd(&pipe, &["wait", id, "--timeout", "60"]).await;
        assert_eq!(done["status"], "succeeded", "{done}");
    }

    let records = kit.records();
    let mut states: Vec<_> = records
        .iter()
        .filter_map(|record| match record {
            Record::Event(OrchestratorEvent::Run { run, .. }) if run.id == slow_id => {
                Some(run.status)
            }
            _ => None,
        })
        .collect();
    states.dedup();
    assert_eq!(
        states,
        [RunStatus::Queued, RunStatus::Running, RunStatus::Succeeded]
    );
    let lines: Vec<_> = records
        .iter()
        .filter_map(|record| match record {
            Record::Logs(batch) if batch.node_id == slow_id => Some(&batch.lines),
            _ => None,
        })
        .flatten()
        .filter(|line| line.stream == LogStream::Stdout)
        .map(|line| line.text.as_str())
        .collect();
    assert_eq!(lines, ["started", "finished"]);
}

fn process_is_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return false;
        }
        let mut exit_code = 0;
        let alive = GetExitCodeProcess(process, &mut exit_code) != 0 && exit_code == 259;
        CloseHandle(process);
        alive
    }
}

#[tokio::test]
async fn cancel_through_gbd_stops_the_running_job() {
    const SPAWN_CHILD: &str = r#"
$ErrorActionPreference = 'Stop'
$child = Start-Process powershell.exe -WindowStyle Hidden -ArgumentList '-NoProfile -NonInteractive -Command Start-Sleep 30' -PassThru
[IO.File]::WriteAllText((Join-Path (Get-Location).Path 'pid.txt'), [string]$child.Id)
[Console]::Out.WriteLine('ready')
Start-Sleep -Seconds 30
"#;
    let kit = Kit::new(&[("spawn", SPAWN_CHILD), ("quick", QUICK)], |_| {}).await;
    let pipe = serving(&kit).await;
    let active = submit(&pipe, &job(&kit, "spawn", "待機", None), 0).await;
    let id = active["job_id"].as_str().unwrap();
    let task = active["task_id"].as_str().unwrap();
    until_response(&pipe, &["log", id, "--tail", "20"], |value| {
        has_log(value, "stdout", "ready")
    })
    .await;
    let pid = fs::read_to_string(kit.repo.join("pid.txt"))
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        process_is_alive(pid),
        "停止前に孫プロセス {pid} が見つかりません"
    );

    // The engine reports a running cancellation as killed and reserves cancelled for queued jobs.
    let stopped = gbd(&pipe, &["cancel", id]).await;
    assert_eq!(stopped["status"], "killed", "{stopped}");
    assert_eq!(stopped["task"]["status"], "open");
    let result = gbd(&pipe, &["result", id]).await;
    assert_eq!(result["status"], "killed", "{result}");
    timeout(Duration::from_secs(10), async {
        while process_is_alive(pid) {
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("孫プロセス {pid} が終了しませんでした"));

    let next = submit(
        &pipe,
        &job(&kit, "quick", "同じタスクで続行", Some(task)),
        0,
    )
    .await;
    assert_eq!(next["task_id"], task);
    let next_id = next["job_id"].as_str().unwrap();
    let done = gbd(&pipe, &["wait", next_id, "--timeout", "60"]).await;
    assert_eq!(done["status"], "succeeded", "{done}");
}

#[tokio::test]
async fn failures_are_visible_and_repeat_failures_pause_the_task() {
    const FAIL: &str = r#"
[Console]::Error.WriteLine('失敗しました 42 C:\work\x')
exit 3
"#;
    let kit = Kit::new(&[("fail", FAIL)], |_| {}).await;
    let pipe = serving(&kit).await;
    let first = submit(&pipe, &job(&kit, "fail", "失敗を確認", None), 0).await;
    let task = first["task_id"].as_str().unwrap();
    let again = job(&kit, "fail", "同じ失敗を繰り返す", Some(task));
    let first_id = first["job_id"].as_str().unwrap();
    let first_result = gbd(&pipe, &["wait", first_id, "--timeout", "60"]).await;
    assert_eq!(first_result["status"], "failed", "{first_result}");
    assert_eq!(first_result["exit_code"], 3);
    assert_eq!(
        first_result["error"]["message"],
        r"失敗しました 42 C:\work\x"
    );
    assert_eq!(first_result["task"]["status"], "open");

    let second = submit(&pipe, &again, 0).await;
    assert_eq!(second["task_id"], task);
    let second_id = second["job_id"].as_str().unwrap();
    let second_result = gbd(&pipe, &["wait", second_id, "--timeout", "60"]).await;
    assert_eq!(second_result["status"], "failed", "{second_result}");
    assert_eq!(second_result["exit_code"], 3);
    assert_eq!(second_result["task"]["status"], "paused");
    let reason = second_result["task"]["pause_reason"].as_str().unwrap();
    assert!(reason.contains("同じ失敗が 2 回"), "{second_result}");

    let rejected = submit(&pipe, &again, 1).await;
    assert_eq!(rejected["error"]["code"], "task_paused", "{rejected}");
}
