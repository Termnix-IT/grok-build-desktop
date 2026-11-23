//! The `gbd` command that Grok Bot runs: one command in, one JSON object out on stdout.

use std::ffi::OsString;
use std::io::{Read, Write};

use serde_json::{Value, json};

use super::engine::{HubError, JobSpec};
use super::ipc::{self, Request};

const JOB_FILE_LIMIT: usize = 1024 * 1024;
const ENCODING_CHECK: &str = "日本語の表示確認";
const DEFAULT_WAIT_SECS: u64 = 300;
const DEFAULT_TAIL: usize = 50;
const MISSING_PIPE: &str = r"\\.\pipe\grok-build-desktop-hub-unknown";

const USAGE: &str =
    "gbd: Grok Build Desktop にジョブを渡すコマンドです。結果は 1 つの JSON で標準出力に出ます。

  gbd submit <job.json | ->          ジョブを登録する（- は標準入力から読む）
  gbd status <job_id>                状態を見る
  gbd wait <job_id> [--timeout 秒]   終わるまで待つ（既定 300、最大 1800）
  gbd result <job_id>                結果を見る
  gbd log <job_id> [--tail 行数]     ログの末尾を見る（既定 50、最大 200）
  gbd cancel <job_id>                取り消す。実行中なら止める
  gbd agents                         使えるエージェントの一覧
  gbd version                        版とアプリへの接続を確かめる
  --ascii                            出力の日本語を \\uXXXX で書く

ジョブの JSON:
  {\"project\": \"C:\\\\path\\\\to\\\\repo\", \"agent\": \"claude\", \"prompt\": \"...\",
   \"title\": \"省略可\", \"task_id\": \"省略可\", \"timeout_secs\": 省略可}
  ファイルは UTF-8 か、BOM 付きの UTF-16 で書きます。日本語は \\uXXXX と書いてもかまいません。

終了コード: 0 成功、1 アプリがエラーを返したか接続できない、2 使い方かジョブファイルの誤り";

enum Command {
    Help,
    Version,
    Agents,
    Submit(String),
    Status(String),
    Result(String),
    Cancel(String),
    Wait(String, u64),
    Log(String, usize),
}

enum Output {
    Text(String),
    Json(Value),
}

pub fn main() -> i32 {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let pipe = ipc::pipe_name().unwrap_or_else(|_| MISSING_PIPE.into());
    let (out, code) = run(args, &mut std::io::stdin(), &pipe);
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(out.as_bytes());
    let _ = stdout.write_all(b"\n");
    let _ = stdout.flush();
    code
}

/// What `gbd version` reports, checked from inside the app.
pub fn selftest() -> Value {
    let pipe = ipc::pipe_name().unwrap_or_else(|_| MISSING_PIPE.into());
    version(&pipe).unwrap_or_else(|error| ipc::error_value(&error))
}

/// Runs one `gbd` invocation and returns the text for stdout and the exit code.
pub fn run(args: Vec<OsString>, stdin: &mut dyn Read, pipe_name: &str) -> (String, i32) {
    let mut ascii = false;
    let mut rest = Vec::new();
    for arg in args {
        let Some(arg) = arg.to_str() else {
            return finish(Err(usage("引数に読み取れない文字があります")), false);
        };
        if arg == "--ascii" {
            ascii = true;
        } else {
            rest.push(arg.to_owned());
        }
    }
    let result = parse(&rest).and_then(|command| execute(command, stdin, pipe_name));
    finish(result, ascii)
}

fn usage(message: impl Into<String>) -> HubError {
    HubError::new(
        "usage",
        format!("{}。gbd help で使い方を出します", message.into()),
    )
}

fn bad_job(message: impl Into<String>) -> HubError {
    HubError::new("bad_job_file", message)
}

fn job_id(value: Option<&String>) -> Result<String, HubError> {
    let id = value.ok_or_else(|| usage("ジョブ ID を指定してください"))?;
    let valid = id.len() == 13
        && id.starts_with('j')
        && id[1..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if !valid {
        return Err(usage(format!(
            "ジョブ ID「{id}」の形が違います。j と 16 進数 12 桁です"
        )));
    }
    Ok(id.clone())
}

/// Reads `--name N` or `--name=N` from the options after the job ID.
fn number_option<T: std::str::FromStr + PartialOrd + Copy + std::fmt::Display>(
    options: &[String],
    name: &str,
    default: T,
    min: T,
    max: T,
) -> Result<T, HubError> {
    let mut value = None;
    let mut index = 0;
    while index < options.len() {
        let option = &options[index];
        let text = if option == name {
            index += 1;
            options
                .get(index)
                .ok_or_else(|| usage(format!("{name} の値がありません")))?
                .as_str()
        } else if let Some(text) = option.strip_prefix(&format!("{name}=")) {
            text
        } else {
            return Err(usage(format!("「{option}」は使えません")));
        };
        let number: T = text
            .parse()
            .map_err(|_| usage(format!("{name} は数で指定してください")))?;
        if number < min || number > max {
            return Err(usage(format!("{name} は {min}〜{max} にしてください")));
        }
        value = Some(number);
        index += 1;
    }
    Ok(value.unwrap_or(default))
}

fn parse(args: &[String]) -> Result<Command, HubError> {
    let Some(name) = args.first() else {
        return Ok(Command::Help);
    };
    let only = |command: Command| {
        if args.len() == 1 {
            Ok(command)
        } else {
            Err(usage(format!("{name} に引数は要りません")))
        }
    };
    let with_id = |make: fn(String) -> Command| {
        if args.len() > 2 {
            return Err(usage(format!("{name} にはジョブ ID だけを渡します")));
        }
        job_id(args.get(1)).map(make)
    };
    match name.as_str() {
        "help" | "--help" | "-h" | "/?" => only(Command::Help),
        "version" | "--version" => only(Command::Version),
        "agents" => only(Command::Agents),
        "submit" => match args.len() {
            2 => Ok(Command::Submit(args[1].clone())),
            _ => Err(usage("submit にはジョブファイルか - を 1 つ渡します")),
        },
        "status" => with_id(Command::Status),
        "result" => with_id(Command::Result),
        "cancel" => with_id(Command::Cancel),
        "wait" => {
            let id = job_id(args.get(1))?;
            let timeout = number_option(&args[2..], "--timeout", DEFAULT_WAIT_SECS, 1, 1800)?;
            Ok(Command::Wait(id, timeout))
        }
        "log" => {
            let id = job_id(args.get(1))?;
            let tail = number_option(&args[2..], "--tail", DEFAULT_TAIL, 1, 200)?;
            Ok(Command::Log(id, tail))
        }
        other => Err(usage(format!("「{other}」というコマンドはありません"))),
    }
}

fn execute(command: Command, stdin: &mut dyn Read, pipe: &str) -> Result<Output, HubError> {
    let request = match command {
        Command::Help => return Ok(Output::Text(USAGE.into())),
        Command::Version => return version(pipe).map(Output::Json),
        Command::Agents => Request::Agents { v: 1 },
        Command::Submit(source) => Request::Submit {
            v: 1,
            job: read_job(&source, stdin)?,
        },
        Command::Status(job_id) => Request::Status { v: 1, job_id },
        Command::Result(job_id) => Request::Result { v: 1, job_id },
        Command::Cancel(job_id) => Request::Cancel { v: 1, job_id },
        Command::Wait(job_id, timeout_secs) => Request::Wait {
            v: 1,
            job_id,
            timeout_secs,
        },
        Command::Log(job_id, tail) => Request::Log { v: 1, job_id, tail },
    };
    ipc::client(pipe, &request).map(Output::Json)
}

fn version(pipe: &str) -> Result<Value, HubError> {
    let mut value = json!({
        "ok": true,
        "cli_version": env!("CARGO_PKG_VERSION"),
        "protocol": 1,
        "app_running": false,
        "app_version": null,
        "accepting": null,
        "encoding_check": ENCODING_CHECK,
    });
    match ipc::client(pipe, &Request::Ping { v: 1 }) {
        Ok(ping) => {
            value["app_running"] = json!(true);
            value["app_version"] = ping["app_version"].clone();
            value["accepting"] = ping["accepting"].clone();
            Ok(value)
        }
        Err(error) if error.code == "app_not_running" => Ok(value),
        Err(error) => Err(error),
    }
}

fn read_job(source: &str, stdin: &mut dyn Read) -> Result<JobSpec, HubError> {
    let mut bytes = Vec::new();
    let read = if source == "-" {
        stdin
            .take(JOB_FILE_LIMIT as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| bad_job(format!("標準入力を読めませんでした: {error}")))
    } else {
        std::fs::File::open(source)
            .and_then(|file| file.take(JOB_FILE_LIMIT as u64 + 1).read_to_end(&mut bytes))
            .map_err(|error| {
                bad_job(format!(
                    "ジョブファイル「{source}」を読めませんでした: {error}"
                ))
            })
    };
    read?;
    if bytes.len() > JOB_FILE_LIMIT {
        return Err(bad_job("ジョブファイルは 1 MiB 以下にしてください"));
    }
    let text = decode(&bytes)?;
    serde_json::from_str(&text)
        .map_err(|error| bad_job(format!("ジョブの JSON が正しくありません: {error}")))
}

/// UTF-8 (with or without BOM) or UTF-16 with a BOM. PowerShell 5.1 writes UTF-16LE with a
/// BOM for `>` and `Out-File`, so that case must work as well as plain UTF-8.
fn decode(bytes: &[u8]) -> Result<String, HubError> {
    let utf16 = |body: &[u8], little: bool| {
        if body.len() % 2 != 0 {
            return Err(bad_job("UTF-16 のバイト数が奇数です"));
        }
        let units: Vec<u16> = body
            .chunks_exact(2)
            .map(|pair| {
                if little {
                    u16::from_le_bytes([pair[0], pair[1]])
                } else {
                    u16::from_be_bytes([pair[0], pair[1]])
                }
            })
            .collect();
        String::from_utf16(&units).map_err(|_| bad_job("UTF-16 として読めない文字があります"))
    };
    match bytes {
        [0xEF, 0xBB, 0xBF, body @ ..] => std::str::from_utf8(body)
            .map(str::to_owned)
            .map_err(|_| bad_job("UTF-8 として読めない文字があります")),
        [0xFF, 0xFE, body @ ..] => utf16(body, true),
        [0xFE, 0xFF, body @ ..] => utf16(body, false),
        body => std::str::from_utf8(body).map(str::to_owned).map_err(|_| {
            bad_job("UTF-8 として読めない文字があります。UTF-8 か、BOM 付きの UTF-16 で保存してください")
        }),
    }
}

fn finish(result: Result<Output, HubError>, ascii: bool) -> (String, i32) {
    let value = match result {
        Ok(Output::Text(text)) => {
            return (if ascii { escape_non_ascii(&text) } else { text }, 0);
        }
        Ok(Output::Json(value)) => value,
        Err(error) => ipc::error_value(&error),
    };
    let code = if value["ok"] == json!(true) {
        0
    } else if matches!(
        value["error"]["code"].as_str(),
        Some("usage" | "bad_job_file")
    ) {
        2
    } else {
        1
    };
    let text = value.to_string();
    (if ascii { escape_non_ascii(&text) } else { text }, code)
}

/// Non-ASCII characters only appear inside JSON strings, so `\uXXXX` keeps the JSON valid.
fn escape_non_ascii(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_ascii() {
            out.push(ch);
        } else {
            let mut units = [0u16; 2];
            for unit in ch.encode_utf16(&mut units) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn missing_pipe() -> String {
        format!(r"\\.\pipe\gbd-test-missing-{}", uuid::Uuid::new_v4())
    }

    fn call(args: &[&str]) -> (Value, i32) {
        call_with(args, b"")
    }

    fn call_with(args: &[&str], input: &[u8]) -> (Value, i32) {
        let args = args.iter().map(OsString::from).collect();
        let (text, code) = run(args, &mut &input[..], &missing_pipe());
        (serde_json::from_str(&text).unwrap(), code)
    }

    fn code_of(value: &Value) -> &str {
        value["error"]["code"].as_str().unwrap()
    }

    #[test]
    fn help_and_usage_errors() {
        for args in [&[][..], &["help"], &["--help"]] {
            let args = args.iter().map(OsString::from).collect();
            let (text, code) = run(args, &mut &b""[..], &missing_pipe());
            assert_eq!(code, 0);
            assert!(text.contains("gbd submit") && text.contains("終了コード"));
        }
        for args in [
            &["nope"][..],
            &["status"],
            &["status", "j123"],
            &["status", "J0123456789AB"],
            &["status", "j0123456789ab", "extra"],
            &["wait", "j0123456789ab", "--timeout", "0"],
            &["wait", "j0123456789ab", "--timeout", "1801"],
            &["wait", "j0123456789ab", "--timeout"],
            &["wait", "j0123456789ab", "--timeout=x"],
            &["log", "j0123456789ab", "--tail", "201"],
            &["log", "j0123456789ab", "--lines", "5"],
            &["submit"],
            &["submit", "a.json", "b.json"],
            &["agents", "x"],
        ] {
            let (value, code) = call(args);
            assert_eq!(code, 2, "{args:?}");
            assert_eq!(code_of(&value), "usage", "{args:?}");
            assert_eq!(value["ok"], false);
        }
    }

    #[test]
    fn valid_commands_reach_the_pipe() {
        for args in [
            &["status", "j0123456789ab"][..],
            &["result", "j0123456789ab"],
            &["cancel", "j0123456789ab"],
            &["wait", "j0123456789ab", "--timeout=5"],
            &["log", "j0123456789ab", "--tail", "200"],
            &["agents"],
        ] {
            let (value, code) = call(args);
            assert_eq!(code, 1, "{args:?}");
            assert_eq!(code_of(&value), "app_not_running", "{args:?}");
        }
    }

    #[test]
    fn version_without_app() {
        let (value, code) = call(&["version"]);
        assert_eq!(code, 0);
        assert_eq!(value["app_running"], false);
        assert_eq!(value["encoding_check"], ENCODING_CHECK);
        assert_eq!(value["protocol"], 1);
    }

    #[test]
    fn decodes_job_files() {
        let job = r#"{"project":"C:\\repo","agent":"claude","prompt":"日本語の依頼"}"#;
        let utf16le: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain(job.encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        let utf16be: Vec<u8> = [0xFE, 0xFF]
            .into_iter()
            .chain(job.encode_utf16().flat_map(u16::to_be_bytes))
            .collect();
        let bom: Vec<u8> = [0xEF, 0xBB, 0xBF].into_iter().chain(job.bytes()).collect();
        for bytes in [job.as_bytes().to_vec(), bom, utf16le, utf16be] {
            let spec = read_job("-", &mut &bytes[..]).unwrap();
            assert_eq!(spec.prompt, "日本語の依頼");
            assert_eq!(spec.agent, "claude");
        }
        let escaped = r#"{"project":"C:\\repo","agent":"claude","prompt":"\u65e5\u672c"}"#;
        assert_eq!(
            read_job("-", &mut escaped.as_bytes()).unwrap().prompt,
            "日本"
        );
        for bytes in [
            vec![0x7B, 0xFF, 0x7D],
            vec![0xFF, 0xFE, 0x41],
            vec![0xFF, 0xFE, 0x00, 0xD8, 0x41, 0x00],
            br#"{"project":"C:\\repo","agent":"claude","prompt":"x","extra":1}"#.to_vec(),
            br#"{"agent":"claude","prompt":"x"}"#.to_vec(),
            vec![b' '; JOB_FILE_LIMIT + 1],
        ] {
            let error = read_job("-", &mut &bytes[..]).err().unwrap();
            assert_eq!(error.code, "bad_job_file");
        }
        let (value, code) = call_with(&["submit", "-"], br#"{"agent":"claude"}"#);
        assert_eq!((code_of(&value), code), ("bad_job_file", 2));
        let (value, code) = call_with(&["submit", "-"], job.as_bytes());
        assert_eq!((code_of(&value), code), ("app_not_running", 1));
        let missing =
            std::env::temp_dir().join(format!("gbd-missing-{}.json", uuid::Uuid::new_v4()));
        let (value, code) = call(&["submit", missing.to_str().unwrap()]);
        assert_eq!((code_of(&value), code), ("bad_job_file", 2));
    }

    #[test]
    fn ascii_output_escapes_everything_outside_ascii() {
        assert_eq!(
            escape_non_ascii(r#"{"a":"日本😀","b":"x"}"#),
            r#"{"a":"\u65e5\u672c\ud83d\ude00","b":"x"}"#
        );
        let args = ["--ascii", "version"].iter().map(OsString::from).collect();
        let (text, code) = run(args, &mut &b""[..], &missing_pipe());
        assert_eq!(code, 0);
        assert!(text.is_ascii());
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["encoding_check"], ENCODING_CHECK);
    }
}
