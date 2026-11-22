use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, watch};

use super::adapter::ExitInfo;
use super::engine::LogStream;

#[derive(Clone)]
pub struct Cancel {
    tx: watch::Sender<bool>,
}

impl Default for Cancel {
    fn default() -> Self {
        Self::new()
    }
}

impl Cancel {
    pub fn new() -> Self {
        let (tx, _) = watch::channel(false);
        Self { tx }
    }

    pub fn cancel(&self) {
        self.tx.send_replace(true);
    }

    pub fn is_cancelled(&self) -> bool {
        *self.tx.borrow()
    }

    pub async fn cancelled(&self) {
        let mut rx = self.tx.subscribe();
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                return;
            }
        }
    }
}

pub fn resolve_program(program: &str) -> Result<PathBuf, String> {
    let missing = || format!("「{program}」が見つかりません");
    if program.trim().is_empty() {
        return Err(missing());
    }
    let path = Path::new(program);
    let extensions: Vec<_> = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
        .split(';')
        .filter(|ext| !ext.is_empty())
        .map(str::to_owned)
        .collect();
    let find = |candidate: PathBuf| {
        // npm puts an extensionless shell script next to each .cmd shim; Windows cannot start it.
        if candidate.extension().is_some() && candidate.is_file() {
            return absolute(candidate);
        }
        for ext in &extensions {
            let mut name = candidate.as_os_str().to_os_string();
            name.push(ext);
            let expanded = PathBuf::from(name);
            if expanded.is_file() {
                return absolute(expanded);
            }
        }
        None
    };
    if path.is_absolute() || program.contains(['/', '\\']) {
        return find(path.to_owned()).ok_or_else(missing);
    }
    for folder in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        if let Some(found) = find(folder.join(path)) {
            return Ok(found);
        }
    }
    Err(missing())
}

fn absolute(path: PathBuf) -> Option<PathBuf> {
    if path.is_absolute() {
        Some(path)
    } else {
        std::env::current_dir().ok().map(|cwd| cwd.join(path))
    }
}

pub fn no_window(cmd: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

pub struct RunSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub stdin: Option<String>,
    pub prompt_file: Option<(PathBuf, String)>,
    pub timeout: Duration,
}

struct PromptFile(Option<PathBuf>);

impl Drop for PromptFile {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = std::fs::remove_file(path);
            // Removes the per-task folder only once it is empty.
            if let Some(parent) = path.parent() {
                let _ = std::fs::remove_dir(parent);
            }
        }
    }
}

async fn read_lines(
    input: impl AsyncRead + Unpin,
    stream: LogStream,
    capture: bool,
    tx: mpsc::Sender<(LogStream, String, Option<Vec<u8>>)>,
) {
    let mut reader = BufReader::new(input);
    let mut bytes = Vec::new();
    loop {
        bytes.clear();
        match reader.read_until(b'\n', &mut bytes).await {
            Ok(0) => break,
            Ok(_) => {
                let raw = capture.then(|| bytes.clone());
                if bytes.last() == Some(&b'\n') {
                    bytes.pop();
                }
                if bytes.last() == Some(&b'\r') {
                    bytes.pop();
                }
                let truncated = bytes.len() > 8192;
                bytes.truncate(8192);
                let mut line = String::from_utf8_lossy(&bytes).into_owned();
                if truncated {
                    line.push('…');
                }
                if tx.send((stream, line, raw)).await.is_err() {
                    break;
                }
            }
            Err(err) => {
                let _ = tx
                    .send((
                        LogStream::System,
                        format!("ログを読み取れませんでした: {err}"),
                        None,
                    ))
                    .await;
                break;
            }
        }
    }
}

pub async fn run(
    spec: RunSpec,
    cancel: Cancel,
    on_line: impl FnMut(LogStream, String) + Send,
) -> Result<ExitInfo, String> {
    run_inner(spec, cancel, on_line, false)
        .await
        .map(|(exit, _)| exit)
}

pub async fn run_capture(
    spec: RunSpec,
    cancel: Cancel,
    on_line: impl FnMut(LogStream, String) + Send,
) -> Result<(ExitInfo, String), String> {
    run_inner(spec, cancel, on_line, true).await
}

async fn run_inner(
    spec: RunSpec,
    cancel: Cancel,
    mut on_line: impl FnMut(LogStream, String) + Send,
    capture_stdout: bool,
) -> Result<(ExitInfo, String), String> {
    if cancel.is_cancelled() {
        return Ok((
            ExitInfo {
                code: None,
                killed: true,
                timed_out: false,
            },
            String::new(),
        ));
    }
    let _prompt_guard = PromptFile(spec.prompt_file.as_ref().map(|(path, _)| path.clone()));
    if let Some((path, prompt)) = &spec.prompt_file {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("プロンプトのフォルダを作れませんでした: {err}"))?;
        }
        std::fs::write(path, prompt)
            .map_err(|err| format!("プロンプトを書き込めませんでした: {err}"))?;
    }
    let mut command = tokio::process::Command::new(&spec.program);
    command
        .args(&spec.args)
        .current_dir(&spec.cwd)
        .kill_on_drop(true)
        .stdin(if spec.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    no_window(command.as_std_mut());
    let mut child = command.spawn().map_err(|err| {
        format!(
            "「{}」を起動できませんでした: {err}",
            spec.program.display()
        )
    })?;
    #[cfg(windows)]
    let job = match windows_job::Job::attach(&child) {
        Ok(job) => Some(job),
        Err(err) => {
            on_line(
                LogStream::System,
                format!("子プロセスをまとめて管理できません。直接終了に切り替えます: {err}"),
            );
            None
        }
    };
    let (tx, mut rx) = mpsc::channel(256);
    let stdout_reader = tokio::spawn(read_lines(
        child.stdout.take().expect("piped stdout"),
        LogStream::Stdout,
        capture_stdout,
        tx.clone(),
    ));
    let stderr_reader = tokio::spawn(read_lines(
        child.stderr.take().expect("piped stderr"),
        LogStream::Stderr,
        false,
        tx.clone(),
    ));
    let writer = spec.stdin.map(|text| {
        let mut stdin = child.stdin.take().expect("piped stdin");
        let tx = tx.clone();
        tokio::spawn(async move {
            if let Err(err) = stdin.write_all(text.as_bytes()).await {
                let _ = tx
                    .send((
                        LogStream::System,
                        format!("標準入力を書き込めませんでした: {err}"),
                        None,
                    ))
                    .await;
            }
            let _ = stdin.shutdown().await;
        })
    });
    drop(tx);
    let deadline = tokio::time::sleep(spec.timeout);
    tokio::pin!(deadline);
    let mut killed = false;
    let mut timed_out = false;
    let mut stream_open = true;
    let mut stdout = Vec::new();
    let status = loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                killed = true;
                #[cfg(windows)]
                if let Some(job) = &job { job.terminate(); }
                let _ = child.start_kill();
                break child.wait().await;
            }
            _ = &mut deadline => {
                timed_out = true;
                #[cfg(windows)]
                if let Some(job) = &job { job.terminate(); }
                let _ = child.start_kill();
                break child.wait().await;
            }
            status = child.wait() => break status,
            line = rx.recv(), if stream_open => {
                match line {
                    Some((stream, line, raw)) => {
                        if let Some(raw) = raw { stdout.extend_from_slice(&raw); }
                        on_line(stream, line);
                    },
                    None => stream_open = false,
                }
            }
        }
    };
    if let Some(writer) = writer {
        writer.abort();
        let _ = writer.await;
    }
    // Closing the job also releases pipes inherited by descendants after the parent exits.
    #[cfg(windows)]
    drop(job);
    while let Some((stream, line, raw)) = rx.recv().await {
        if let Some(raw) = raw {
            stdout.extend_from_slice(&raw);
        }
        on_line(stream, line);
    }
    let _ = stdout_reader.await;
    let _ = stderr_reader.await;
    let status = status.map_err(|err| format!("終了を確認できませんでした: {err}"))?;
    Ok((
        ExitInfo {
            code: status.code(),
            killed,
            timed_out,
        },
        String::from_utf8_lossy(&stdout).into_owned(),
    ))
}

#[cfg(windows)]
mod windows_job {
    use std::io;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject,
    };

    pub(super) struct Job(HANDLE);

    // Windows handles are process-wide and may be closed from any runtime thread.
    unsafe impl Send for Job {}

    impl Job {
        pub(super) fn attach(child: &tokio::process::Child) -> io::Result<Self> {
            unsafe {
                let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if handle.is_null() {
                    return Err(io::Error::last_os_error());
                }
                let job = Self(handle);
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                if SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    std::mem::size_of_val(&info) as u32,
                ) == 0
                {
                    return Err(io::Error::last_os_error());
                }
                let process = child
                    .raw_handle()
                    .ok_or_else(|| io::Error::other("子プロセスのハンドルがありません"))?;
                if AssignProcessToJobObject(handle, process as HANDLE) == 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(job)
            }
        }

        pub(super) fn terminate(&self) {
            unsafe {
                TerminateJobObject(self.0, 1);
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("gbd-runner-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn spec(&self, script: &str) -> RunSpec {
            let path = self.0.join("fake.ps1");
            std::fs::write(&path, script).unwrap();
            RunSpec {
                program: resolve_program("powershell").unwrap(),
                args: vec![
                    "-NoProfile".into(),
                    "-NonInteractive".into(),
                    "-ExecutionPolicy".into(),
                    "Bypass".into(),
                    "-File".into(),
                    path.to_string_lossy().into_owned(),
                ],
                cwd: self.0.clone(),
                stdin: None,
                prompt_file: None,
                timeout: Duration::from_secs(10),
            }
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn resolves_powershell_and_extensions() {
        let path = resolve_program("powershell").unwrap();
        assert!(path.is_absolute());
        assert_eq!(
            path.extension()
                .unwrap()
                .to_string_lossy()
                .to_ascii_lowercase(),
            "exe"
        );
        let dir = TempDir::new();
        let cmd = dir.0.join("fake.CMD");
        std::fs::write(&cmd, "").unwrap();
        std::fs::write(dir.0.join("fake"), "#!/bin/sh").unwrap();
        assert_eq!(
            resolve_program(dir.0.join("fake").to_str().unwrap()).unwrap(),
            cmd
        );
        assert_eq!(resolve_program(cmd.to_str().unwrap()).unwrap(), cmd);
        std::fs::remove_file(&cmd).unwrap();
        assert!(resolve_program(dir.0.join("fake").to_str().unwrap()).is_err());
        assert!(resolve_program(dir.0.join("missing").to_str().unwrap()).is_err());
    }

    #[tokio::test]
    async fn stdin_streams_exit_and_cleanup() {
        let dir = TempDir::new();
        let mut spec = dir.spec("[Console]::Out.WriteLine([Console]::In.ReadToEnd()); [Console]::Error.WriteLine('stderr-line'); exit 3");
        spec.stdin = Some("stdin-value".into());
        let prompt_path = dir.0.join("nested/prompt.txt");
        spec.prompt_file = Some((prompt_path.clone(), "prompt".into()));
        let mut lines = Vec::new();
        let exit = run(spec, Cancel::new(), |stream, line| {
            lines.push((stream, line))
        })
        .await
        .unwrap();
        assert_eq!(exit.code, Some(3));
        assert!(!exit.killed && !exit.timed_out);
        assert!(
            lines
                .iter()
                .any(|(stream, line)| *stream == LogStream::Stdout && line == "stdin-value")
        );
        assert!(
            lines
                .iter()
                .any(|(stream, line)| *stream == LogStream::Stderr && line == "stderr-line")
        );
        assert!(!prompt_path.exists());
        assert!(!prompt_path.parent().unwrap().exists());
    }

    #[tokio::test]
    async fn captures_complete_json_while_truncating_logs() {
        let dir = TempDir::new();
        let spec = dir.spec("[Console]::Out.WriteLine(('{\"result\":\"' + ('x' * 10000) + '\"}'))");
        let mut lines = Vec::new();
        let (exit, stdout) = run_capture(spec, Cancel::new(), |stream, line| {
            if stream == LogStream::Stdout {
                lines.push(line);
            }
        })
        .await
        .unwrap();
        assert_eq!(exit.code, Some(0));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&stdout).unwrap()["result"]
                .as_str()
                .unwrap()
                .len(),
            10000
        );
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].chars().count(), 8193);
        assert!(lines[0].ends_with('…'));
    }

    #[tokio::test]
    async fn truncates_long_lines_and_times_out() {
        let dir = TempDir::new();
        let mut spec = dir.spec("[Console]::Out.WriteLine(('x' * 9000)); Start-Sleep 60");
        spec.timeout = Duration::from_secs(1);
        let mut lines = Vec::new();
        let exit = run(spec, Cancel::new(), |stream, line| {
            lines.push((stream, line))
        })
        .await
        .unwrap();
        assert!(exit.timed_out);
        assert!(!exit.killed);
        assert!(
            lines
                .iter()
                .any(|(_, line)| line.chars().count() == 8193 && line.ends_with('…'))
        );
    }

    #[tokio::test]
    async fn cancellation_kills_grandchild() {
        let dir = TempDir::new();
        let pid_file = dir.0.join("pid.txt");
        let script = format!(
            "$p = Start-Process powershell.exe -WindowStyle Hidden -ArgumentList '-NoProfile -NonInteractive -Command Start-Sleep 60' -PassThru; [IO.File]::WriteAllText('{}', [string]$p.Id); Start-Sleep 60",
            pid_file.to_string_lossy().replace('\'', "''")
        );
        let spec = dir.spec(&script);
        let cancel = Cancel::new();
        let runner_cancel = cancel.clone();
        let handle = tokio::spawn(async move { run(spec, runner_cancel, |_, _| {}).await });
        tokio::time::timeout(Duration::from_secs(10), async {
            while !pid_file.exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let pid: u32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        cancel.cancel();
        let exit = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(exit.killed);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let alive = unsafe {
                    let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
                    if process.is_null() {
                        false
                    } else {
                        let mut code = 0;
                        let alive = GetExitCodeProcess(process, &mut code) != 0 && code == 259;
                        CloseHandle(process);
                        alive
                    }
                };
                if !alive {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn cancel_before_spawn_and_cleanup_on_spawn_error() {
        let dir = TempDir::new();
        let cancel = Cancel::new();
        cancel.cancel();
        cancel.cancelled().await;
        assert!(
            run(dir.spec("exit 0"), cancel, |_, _| {})
                .await
                .unwrap()
                .killed
        );
        let mut spec = dir.spec("exit 0");
        spec.program = dir.0.join("missing.exe");
        let prompt = dir.0.join("prompt.txt");
        spec.prompt_file = Some((prompt.clone(), "body".into()));
        assert!(run(spec, Cancel::new(), |_, _| {}).await.is_err());
        assert!(!prompt.exists());
    }
}
