use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::{SystemTime, UNIX_EPOCH};

use super::engine::{
    GatewayStatus, HubError, JobSpec, JobView, Orchestra, RunStatus, Source, TaskRecord,
};

const MAX_LINE: usize = 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Submit {
        v: u32,
        job: JobSpec,
    },
    Status {
        v: u32,
        job_id: String,
    },
    Wait {
        v: u32,
        job_id: String,
        timeout_secs: u64,
    },
    Result {
        v: u32,
        job_id: String,
    },
    Cancel {
        v: u32,
        job_id: String,
    },
    Agents {
        v: u32,
    },
    Log {
        v: u32,
        job_id: String,
        tail: usize,
    },
    Ping {
        v: u32,
    },
}

impl Request {
    fn version(&self) -> u32 {
        match self {
            Self::Submit { v, .. }
            | Self::Status { v, .. }
            | Self::Wait { v, .. }
            | Self::Result { v, .. }
            | Self::Cancel { v, .. }
            | Self::Agents { v }
            | Self::Log { v, .. }
            | Self::Ping { v } => *v,
        }
    }
}

pub fn error_value(error: &HubError) -> Value {
    json!({"ok": false, "error": {"code": error.code, "message": error.message}})
}

fn bad_request(message: impl Into<String>) -> HubError {
    HubError::new("bad_request", message)
}

fn task_value(task: &TaskRecord) -> Value {
    json!({
        "status": task.status,
        "jobs": task.dispatches,
        "limit": task.dispatch_limit,
        "pause_reason": task.pause_reason,
    })
}

fn status_name(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Queued => "queued",
        RunStatus::Running => "running",
        RunStatus::Succeeded => "succeeded",
        RunStatus::Failed => "failed",
        RunStatus::TimedOut => "timed_out",
        RunStatus::Killed => "killed",
        RunStatus::Cancelled => "cancelled",
    }
}

fn duration_secs(view: &JobView) -> Option<u64> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    view.run
        .started_at
        .map(|start| view.run.ended_at.unwrap_or(now).saturating_sub(start) / 1000)
}

fn status_value(view: &JobView) -> Value {
    json!({
        "ok": true,
        "job_id": view.run.id,
        "task_id": view.task.id,
        "agent": view.run.agent_id,
        "status": status_name(view.run.status),
        "position": view.position,
        "elapsed_secs": duration_secs(view),
        "task": task_value(&view.task),
    })
}

fn job_value(view: &JobView) -> Value {
    json!({
        "ok": true,
        "job_id": view.run.id,
        "task_id": view.task.id,
        "agent": view.run.agent_id,
        "title": view.run.title,
        "status": status_name(view.run.status),
        "position": view.position,
        "exit_code": view.run.exit_code,
        "duration_secs": duration_secs(view),
        "summary": view.run.summary,
        "denied": view.run.denied,
        "changed_files": view.run.changed_files,
        "diff_stat": view.run.diff_stat,
        "truncated": view.run.truncated,
        "error": view.run.error,
        "recovery_point": view.run.recovery_point,
        "task": task_value(&view.task),
    })
}

/// Grok Bot only sees the jobs it submitted. Jobs started from the screen may hold work the
/// user never meant to send to the cloud, so they look like unknown IDs over the pipe.
fn bot_job(hub: &Orchestra, id: &str) -> Result<JobView, HubError> {
    let view = hub.job(id)?;
    if view.run.source != Source::Bot {
        return Err(HubError::new("not_found", "ジョブが見つかりません"));
    }
    Ok(view)
}

async fn handle(hub: &Orchestra, request: Request) -> Result<Value, HubError> {
    if request.version() != 1 {
        return Err(bad_request(
            "対応していないプロトコルの版です。v は 1 にしてください",
        ));
    }
    match request {
        Request::Submit { job, .. } => {
            let result = hub.submit(job, Source::Bot).await?;
            Ok(json!({
                "ok": true, "job_id": result.job_id, "task_id": result.task_id,
                "status": "queued", "position": result.position,
            }))
        }
        Request::Status { job_id, .. } => Ok(status_value(&bot_job(hub, &job_id)?)),
        Request::Wait {
            job_id,
            timeout_secs,
            ..
        } => {
            if !(1..=1800).contains(&timeout_secs) {
                return Err(bad_request("timeout_secs は 1〜1800 にしてください"));
            }
            bot_job(hub, &job_id)?;
            Ok(job_value(&hub.wait(&job_id, timeout_secs).await?))
        }
        Request::Result { job_id, .. } => Ok(job_value(&bot_job(hub, &job_id)?)),
        Request::Cancel { job_id, .. } => {
            bot_job(hub, &job_id)?;
            Ok(job_value(&hub.cancel(&job_id, Source::Bot).await?))
        }
        Request::Agents { .. } => Ok(json!({"ok": true, "agents": hub.agents().await})),
        Request::Log { job_id, tail, .. } => {
            if !(1..=200).contains(&tail) {
                return Err(bad_request("tail は 1〜200 にしてください"));
            }
            bot_job(hub, &job_id)?;
            let log = hub.log_tail(&job_id, tail)?;
            let lines: Vec<_> = log
                .lines
                .iter()
                .map(|line| json!({"stream": line.stream, "text": line.text}))
                .collect();
            Ok(json!({"ok": true, "job_id": job_id, "lines": lines, "truncated": log.truncated}))
        }
        Request::Ping { .. } => Ok(json!({
            "ok": true,
            "app_version": env!("CARGO_PKG_VERSION"),
            "protocol": 1,
            "accepting": hub.gateway_settings().accepting,
        })),
    }
}

#[cfg(windows)]
pub fn pipe_name() -> Result<String, HubError> {
    windows::sid_string().map(|sid| format!(r"\\.\pipe\grok-build-desktop-hub-{sid}"))
}

#[cfg(not(windows))]
pub fn pipe_name() -> Result<String, HubError> {
    Err(HubError::new(
        "app_not_running",
        "この接続は Windows でのみ使えます",
    ))
}

#[cfg(windows)]
pub async fn serve(hub: Orchestra, pipe_name: &str) -> Result<(), HubError> {
    use tokio::net::windows::named_pipe::NamedPipeServer;

    fn failed(hub: &Orchestra, error: HubError) -> HubError {
        hub.set_gateway_status(GatewayStatus::Failed {
            error: error.message.clone(),
        });
        error
    }

    hub.set_gateway_status(GatewayStatus::Starting);
    let sid = windows::sid_string().map_err(|error| failed(&hub, error))?;
    let mut next: NamedPipeServer =
        windows::create_server(pipe_name, &sid, true).map_err(|error| failed(&hub, error))?;
    hub.set_gateway_status(GatewayStatus::Listening {
        pipe: pipe_name.to_owned(),
    });
    loop {
        if next.connect().await.is_err() {
            // A client that gave up before the connection completed must not stop the gateway.
            // The replacement is created first so the name never has no instance.
            let replacement = windows::create_server(pipe_name, &sid, false)
                .map_err(|error| failed(&hub, error))?;
            next = replacement;
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            continue;
        }
        // Keep a listening instance alive before dispatching the connected instance.
        let connected = next;
        next =
            windows::create_server(pipe_name, &sid, false).map_err(|error| failed(&hub, error))?;
        let hub = hub.clone();
        tokio::spawn(async move {
            connection(hub, connected).await;
        });
    }
}

#[cfg(not(windows))]
pub async fn serve(hub: Orchestra, _pipe_name: &str) -> Result<(), HubError> {
    let error = HubError::new("internal", "名前付きパイプは Windows でのみ使えます");
    hub.set_gateway_status(GatewayStatus::Failed {
        error: error.message.clone(),
    });
    Err(error)
}

#[cfg(windows)]
async fn connection(hub: Orchestra, mut pipe: tokio::net::windows::named_pipe::NamedPipeServer) {
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    let response = async {
        let mut line = Vec::new();
        // take bounds read_until even when a client never sends a newline.
        let result = tokio::time::timeout(Duration::from_secs(10), async {
            let limited = (&mut pipe).take((MAX_LINE + 1) as u64);
            BufReader::new(limited).read_until(b'\n', &mut line).await
        })
        .await;
        match result {
            Err(_) => {
                return Err(bad_request(
                    "要求の読み取りが 10 秒以内に完了しませんでした",
                ));
            }
            Ok(Err(error)) => {
                return Err(bad_request(format!("要求を読み取れませんでした: {error}")));
            }
            Ok(Ok(_)) => {}
        }
        if line.len() > MAX_LINE || line.last() != Some(&b'\n') {
            return Err(bad_request(
                "要求は改行で終わる 1 MiB 以下の JSON にしてください",
            ));
        }
        let request = serde_json::from_slice::<Request>(&line)
            .map_err(|error| bad_request(format!("要求の JSON が正しくありません: {error}")))?;
        handle(&hub, request).await
    }
    .await
    .unwrap_or_else(|error| error_value(&error));
    let mut bytes = serde_json::to_vec(&response).expect("JSON value is serializable");
    bytes.push(b'\n');
    let _ = pipe.write_all(&bytes).await;
    let _ = pipe.flush().await;
    let _ = pipe.shutdown().await;
}

pub fn client(pipe_name: &str, request: &Request) -> Result<Value, HubError> {
    let mut bytes = serde_json::to_vec(request)
        .map_err(|error| bad_request(format!("要求を変換できませんでした: {error}")))?;
    bytes.push(b'\n');
    if bytes.len() > MAX_LINE {
        return Err(bad_request("要求は 1 MiB 以下にしてください"));
    }
    client_bytes(pipe_name, &bytes)
}

#[cfg(windows)]
fn client_bytes(pipe_name: &str, bytes: &[u8]) -> Result<Value, HubError> {
    use std::io::{BufRead, BufReader, Read, Write};
    let mut pipe = windows::open_verified(pipe_name)?;
    // No request data is sent until open_verified has matched the server SID.
    pipe.write_all(bytes)
        .and_then(|()| pipe.flush())
        .map_err(|error| HubError::new("internal", format!("要求を送れませんでした: {error}")))?;
    let mut response = Vec::new();
    BufReader::new(pipe.take((MAX_LINE + 1) as u64))
        .read_until(b'\n', &mut response)
        .map_err(|error| {
            HubError::new("internal", format!("応答を読み取れませんでした: {error}"))
        })?;
    if response.len() > MAX_LINE || response.last() != Some(&b'\n') {
        return Err(HubError::new(
            "internal",
            "応答の長さまたは改行が正しくありません",
        ));
    }
    serde_json::from_slice(&response).map_err(|error| {
        HubError::new(
            "internal",
            format!("応答の JSON が正しくありません: {error}"),
        )
    })
}

#[cfg(not(windows))]
fn client_bytes(_pipe_name: &str, _bytes: &[u8]) -> Result<Value, HubError> {
    Err(HubError::new(
        "app_not_running",
        "Grok Build Desktop に接続できませんでした",
    ))
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::{
        ffi::c_void,
        fs::{File, OpenOptions},
        io,
        os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
        ptr,
    };
    use tokio::net::windows::named_pipe::{NamedPipeServer, PipeMode, ServerOptions};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, HANDLE, LocalFree},
        Security::{
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            },
            EqualSid, GetTokenInformation, PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
            TokenUser,
        },
        System::{
            Pipes::{GetNamedPipeServerProcessId, WaitNamedPipeW},
            Threading::{
                GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
            },
        },
    };

    struct Handle(HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    // usize storage provides alignment for TOKEN_USER, including its SID pointer.
    struct UserSid(Vec<usize>);
    impl UserSid {
        fn ptr(&self) -> PSID {
            unsafe { (*(self.0.as_ptr().cast::<TOKEN_USER>())).User.Sid }
        }
    }

    fn user_sid(process: HANDLE) -> io::Result<UserSid> {
        let mut token = ptr::null_mut();
        if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = Handle(token);
        let mut needed = 0;
        unsafe {
            GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut needed);
        }
        if needed == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut data = vec![0usize; (needed as usize).div_ceil(size_of::<usize>())];
        if unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                data.as_mut_ptr().cast(),
                needed,
                &mut needed,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(UserSid(data))
    }

    pub(super) fn sid_string() -> Result<String, HubError> {
        let sid = user_sid(unsafe { GetCurrentProcess() }).map_err(|error| {
            HubError::new(
                "internal",
                format!("ユーザー SID を確認できませんでした: {error}"),
            )
        })?;
        let mut string_sid = ptr::null_mut();
        if unsafe { ConvertSidToStringSidW(sid.ptr(), &mut string_sid) } == 0 {
            return Err(HubError::new(
                "internal",
                format!(
                    "ユーザー SID を変換できませんでした: {}",
                    io::Error::last_os_error()
                ),
            ));
        }
        let text = unsafe {
            let mut len = 0;
            while *string_sid.add(len) != 0 {
                len += 1;
            }
            let text = String::from_utf16_lossy(std::slice::from_raw_parts(string_sid, len));
            LocalFree(string_sid.cast());
            text
        };
        Ok(text)
    }

    pub(super) fn create_server(
        name: &str,
        sid: &str,
        first: bool,
    ) -> Result<NamedPipeServer, HubError> {
        let sddl: Vec<u16> = format!("D:P(A;;GA;;;{sid})")
            .encode_utf16()
            .chain([0])
            .collect();
        let mut descriptor = ptr::null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut descriptor,
                ptr::null_mut(),
            )
        } == 0
        {
            return Err(HubError::new(
                "internal",
                format!(
                    "パイプのアクセス権を設定できませんでした: {}",
                    io::Error::last_os_error()
                ),
            ));
        }
        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        let result = unsafe {
            ServerOptions::new()
                .first_pipe_instance(first)
                .reject_remote_clients(true)
                .pipe_mode(PipeMode::Byte)
                .create_with_security_attributes_raw(
                    name,
                    (&mut attributes as *mut SECURITY_ATTRIBUTES).cast::<c_void>(),
                )
        };
        unsafe {
            LocalFree(descriptor);
        }
        result.map_err(|error| {
            let message = if first && error.kind() == io::ErrorKind::PermissionDenied {
                "別のプロセスが同じ名前のパイプを使っています。Grok Build Desktop が二重に起動していないか確認してください".to_owned()
            } else {
                format!("パイプを作れませんでした: {error}")
            };
            HubError::new("internal", message)
        })
    }

    pub(super) fn open_verified(name: &str) -> Result<File, HubError> {
        let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
        let mut attempts = 0;
        // The server may identify this client but not act as it (SecurityIdentification).
        const SECURITY_IDENTIFICATION: u32 = 0x0001_0000;
        let pipe = loop {
            match OpenOptions::new()
                .read(true)
                .write(true)
                .security_qos_flags(SECURITY_IDENTIFICATION)
                .open(name)
            {
                Ok(pipe) => break pipe,
                Err(error) if error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) => {
                    return Err(HubError::new(
                        "app_not_running",
                        "Grok Build Desktop が起動していません",
                    ));
                }
                Err(error)
                    if error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) && attempts < 5 =>
                {
                    attempts += 1;
                    unsafe {
                        WaitNamedPipeW(wide.as_ptr(), 2000);
                    }
                }
                Err(error) => {
                    return Err(HubError::new(
                        "internal",
                        format!("Grok Build Desktop に接続できませんでした: {error}"),
                    ));
                }
            }
        };
        verify_server(&pipe).map_err(|error| {
            HubError::new(
                "untrusted_server",
                format!("接続先のユーザーを確認できませんでした: {error}"),
            )
        })?;
        Ok(pipe)
    }

    fn verify_server(pipe: &File) -> io::Result<()> {
        let mut pid = 0;
        if unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut pid) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if process.is_null() {
            return Err(io::Error::last_os_error());
        }
        let process = Handle(process);
        let server = user_sid(process.0)?;
        let current = user_sid(unsafe { GetCurrentProcess() })?;
        if unsafe { EqualSid(server.ptr(), current.ptr()) } == 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "接続先は現在のユーザーではありません",
            ));
        }
        Ok(())
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::super::testkit::Kit;
    use super::*;
    use std::time::Duration;

    const QUICK: &str = "[Console]::Out.WriteLine('quick')";

    fn unique_pipe() -> String {
        format!(r"\\.\pipe\gbd-test-{}", uuid::Uuid::new_v4())
    }

    async fn call(pipe: &str, request: Request) -> Value {
        let pipe = pipe.to_owned();
        tokio::task::spawn_blocking(move || {
            client(&pipe, &request).unwrap_or_else(|error| error_value(&error))
        })
        .await
        .unwrap()
    }

    async fn raw(pipe: &str, bytes: Vec<u8>) -> Value {
        let pipe = pipe.to_owned();
        tokio::task::spawn_blocking(move || {
            client_bytes(&pipe, &bytes).unwrap_or_else(|error| error_value(&error))
        })
        .await
        .unwrap()
    }

    async fn serving(kit: &Kit) -> String {
        let name = unique_pipe();
        let hub = kit.hub.clone();
        let server_name = name.clone();
        tokio::spawn(async move {
            let _ = serve(hub, &server_name).await;
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            while kit.hub.gateway_view()["status"]["state"] != "listening" {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        name
    }

    fn code(value: &Value) -> &str {
        value["error"]["code"].as_str().unwrap_or_default()
    }

    #[tokio::test]
    async fn round_trip_over_the_pipe() {
        let kit = Kit::new(&[("quick", QUICK)], |_| {}).await;
        let pipe = serving(&kit).await;
        let ping = call(&pipe, Request::Ping { v: 1 }).await;
        assert_eq!(
            (
                ping["ok"].clone(),
                ping["protocol"].clone(),
                ping["accepting"].clone()
            ),
            (json!(true), json!(1), json!(true))
        );
        let agents = call(&pipe, Request::Agents { v: 1 }).await;
        assert_eq!(agents["agents"][0]["id"], "quick");
        assert_eq!(agents["agents"][0]["available"], true);
        let submitted = call(
            &pipe,
            Request::Submit {
                v: 1,
                job: kit.job("quick", "日本語の依頼"),
            },
        )
        .await;
        assert_eq!(submitted["ok"], true, "{submitted}");
        assert_eq!(submitted["status"], "queued");
        let job_id = submitted["job_id"].as_str().unwrap().to_owned();
        let done = call(
            &pipe,
            Request::Wait {
                v: 1,
                job_id: job_id.clone(),
                timeout_secs: 60,
            },
        )
        .await;
        assert_eq!(done["status"], "succeeded", "{done}");
        assert_eq!(done["summary"], "quick");
        assert_eq!(done["title"], "日本語の依頼");
        assert_eq!(done["task"]["jobs"], 1);
        assert_eq!(done["task"]["limit"], 10);
        assert_eq!(done["task"]["status"], "open");
        assert!(done["recovery_point"].is_string());
        assert_eq!(done["changed_files"], json!([]));
        let status = call(
            &pipe,
            Request::Status {
                v: 1,
                job_id: job_id.clone(),
            },
        )
        .await;
        assert_eq!(status["status"], "succeeded");
        let result = call(
            &pipe,
            Request::Result {
                v: 1,
                job_id: job_id.clone(),
            },
        )
        .await;
        assert_eq!(result["job_id"], job_id.as_str());
        let log = call(
            &pipe,
            Request::Log {
                v: 1,
                job_id: job_id.clone(),
                tail: 200,
            },
        )
        .await;
        assert!(
            log["lines"]
                .as_array()
                .unwrap()
                .iter()
                .any(|line| line["text"] == "quick" && line["stream"] == "stdout")
        );
        let cancel = call(
            &pipe,
            Request::Cancel {
                v: 1,
                job_id: job_id.clone(),
            },
        )
        .await;
        assert_eq!(cancel["status"], "succeeded");
        // The gbd command goes through the same client.
        let cli_pipe = pipe.clone();
        let cli_job = job_id.clone();
        let (text, exit) = tokio::task::spawn_blocking(move || {
            super::super::cli::run(
                vec!["--ascii".into(), "result".into(), cli_job.into()],
                &mut &b""[..],
                &cli_pipe,
            )
        })
        .await
        .unwrap();
        assert_eq!(exit, 0);
        assert!(text.is_ascii());
        assert_eq!(
            serde_json::from_str::<Value>(&text).unwrap()["title"],
            "日本語の依頼"
        );
    }

    #[tokio::test]
    async fn bot_cannot_see_jobs_started_on_screen() {
        let kit = Kit::new(&[("quick", QUICK)], |_| {}).await;
        let pipe = serving(&kit).await;
        let manual = kit
            .hub
            .submit(kit.job("quick", "by hand"), Source::Manual)
            .await
            .unwrap();
        kit.finish(&manual.job_id).await;
        // Nor can it add a job to a task made on the screen.
        let mut joined = kit.job("quick", "into the screen's task");
        joined.task_id = Some(manual.task_id.clone());
        for request in [
            Request::Submit { v: 1, job: joined },
            Request::Status {
                v: 1,
                job_id: manual.job_id.clone(),
            },
            Request::Result {
                v: 1,
                job_id: manual.job_id.clone(),
            },
            Request::Wait {
                v: 1,
                job_id: manual.job_id.clone(),
                timeout_secs: 1,
            },
            Request::Log {
                v: 1,
                job_id: manual.job_id.clone(),
                tail: 10,
            },
            Request::Cancel {
                v: 1,
                job_id: manual.job_id.clone(),
            },
        ] {
            assert_eq!(code(&call(&pipe, request).await), "not_found");
        }
        assert_eq!(kit.hub.task(&manual.task_id).unwrap().runs.len(), 1);
    }

    #[tokio::test]
    async fn malformed_requests_are_rejected_and_the_pipe_keeps_serving() {
        let kit = Kit::new(&[("quick", QUICK)], |_| {}).await;
        let pipe = serving(&kit).await;
        for bytes in [
            vec![b'x'; MAX_LINE + 10],
            b"not json\n".to_vec(),
            b"{\"v\":1,\"cmd\":\"explode\"}\n".to_vec(),
            b"{\"v\":1,\"cmd\":\"ping\",\"extra\":true}\n".to_vec(),
            b"{\"v\":2,\"cmd\":\"ping\"}\n".to_vec(),
            b"{\"v\":1,\"cmd\":\"wait\",\"job_id\":\"j000000000000\",\"timeout_secs\":0}\n"
                .to_vec(),
            b"{\"v\":1,\"cmd\":\"log\",\"job_id\":\"j000000000000\",\"tail\":201}\n".to_vec(),
        ] {
            assert_eq!(code(&raw(&pipe, bytes).await), "bad_request");
        }
        assert_eq!(
            code(
                &call(
                    &pipe,
                    Request::Status {
                        v: 1,
                        job_id: "j000000000000".into()
                    }
                )
                .await
            ),
            "not_found"
        );
        assert_eq!(call(&pipe, Request::Ping { v: 1 }).await["ok"], true);
    }

    #[tokio::test]
    async fn second_server_and_missing_app() {
        let kit = Kit::new(&[], |_| {}).await;
        let pipe = serving(&kit).await;
        let sid = windows::sid_string().unwrap();
        assert!(windows::create_server(&pipe, &sid, true).is_err());
        assert_eq!(
            code(&call(&unique_pipe(), Request::Ping { v: 1 }).await),
            "app_not_running"
        );
        assert!(
            pipe_name()
                .unwrap()
                .starts_with(r"\\.\pipe\grok-build-desktop-hub-S-1-5-")
        );
    }
}
