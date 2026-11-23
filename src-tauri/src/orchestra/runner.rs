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
        // npm also puts POSIX shims with dotted names next to .cmd files.
        let executable_extension = candidate.extension().is_some_and(|extension| {
            extensions.iter().any(|known| {
                extension
                    .to_string_lossy()
                    .eq_ignore_ascii_case(known.trim_start_matches('.'))
            })
        });
        if (!cfg!(windows) || executable_extension) && candidate.is_file() {
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
    if path.is_absolute() {
        return find(path.to_owned()).ok_or_else(missing);
    }
    // A relative path would depend on the app's current folder, which nobody chose.
    if program.contains(['/', '\\']) || path.has_root() || path.components().count() != 1 {
        return Err(format!("「{program}」は名前か絶対パスで書いてください"));
    }
    let search =
        |paths: &std::ffi::OsStr| search_folders(paths).find_map(|folder| find(folder.join(path)));
    if let Some(found) = search(&std::env::var_os("PATH").unwrap_or_default()) {
        return Ok(found);
    }
    #[cfg(windows)]
    for paths in registry_paths() {
        if let Some(found) = search(std::ffi::OsStr::new(&paths)) {
            return Ok(found);
        }
    }
    Err(missing())
}

/// The PATH entries to search. Relative ones ("" or ".") would search the app's current folder.
fn search_folders(paths: &std::ffi::OsStr) -> impl Iterator<Item = PathBuf> + '_ {
    std::env::split_paths(paths).filter(|folder| folder.is_absolute())
}

#[cfg(any(windows, test))]
fn merge_paths(process: &str, user: &str, machine: &str) -> String {
    let mut entries: Vec<&str> = process
        .split(';')
        .filter(|entry| !entry.is_empty())
        .collect();
    let mut seen: std::collections::HashSet<String> =
        entries.iter().map(|entry| entry.to_lowercase()).collect();
    for entry in user
        .split(';')
        .chain(machine.split(';'))
        .filter(|entry| !entry.is_empty())
    {
        if seen.insert(entry.to_lowercase()) {
            entries.push(entry);
        }
    }
    entries.join(";")
}

#[cfg(windows)]
fn registry_paths() -> [String; 2] {
    use windows_sys::Win32::System::Environment::ExpandEnvironmentStringsW;
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_NOEXPAND, RRF_RT_REG_EXPAND_SZ,
        RRF_RT_REG_SZ, RegGetValueW,
    };

    fn read(key: HKEY, subkey: &str) -> String {
        let subkey: Vec<u16> = subkey.encode_utf16().chain(Some(0)).collect();
        let name: Vec<u16> = "Path".encode_utf16().chain(Some(0)).collect();
        let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND;
        let mut bytes = 0;
        unsafe {
            if RegGetValueW(
                key,
                subkey.as_ptr(),
                name.as_ptr(),
                flags,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut bytes,
            ) != 0
                || bytes == 0
                || bytes > 1024 * 1024
            {
                return String::new();
            }
            let mut value = vec![0u16; (bytes as usize).div_ceil(2) + 1];
            if RegGetValueW(
                key,
                subkey.as_ptr(),
                name.as_ptr(),
                flags,
                std::ptr::null_mut(),
                value.as_mut_ptr().cast(),
                &mut bytes,
            ) != 0
            {
                return String::new();
            }
            let required = ExpandEnvironmentStringsW(value.as_ptr(), std::ptr::null_mut(), 0);
            if required == 0 || required > 1024 * 1024 {
                return String::new();
            }
            let mut expanded = vec![0u16; required as usize];
            let written =
                ExpandEnvironmentStringsW(value.as_ptr(), expanded.as_mut_ptr(), required);
            if written == 0 || written > required {
                return String::new();
            }
            String::from_utf16_lossy(&expanded[..written.saturating_sub(1) as usize])
        }
    }

    [
        read(HKEY_CURRENT_USER, "Environment"),
        read(
            HKEY_LOCAL_MACHINE,
            r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment",
        ),
    ]
}

pub(super) fn child_path(command: &mut std::process::Command) {
    // The child runs in the project folder, where an agent can leave a `node.cmd`. cmd.exe
    // (npm's .cmd shims start `node` by name) and Node look in the current folder before PATH
    // unless this is set, and relative PATH entries would search it too.
    command.env("NoDefaultCurrentDirectoryInExePath", "1");
    #[cfg(windows)]
    {
        let [user, machine] = registry_paths();
        let merged = merge_paths(&std::env::var("PATH").unwrap_or_default(), &user, &machine);
        command.env("PATH", absolute_path_list(&merged));
    }
}

/// The PATH for a child, without the relative entries.
#[cfg(any(windows, test))]
fn absolute_path_list(paths: &str) -> std::ffi::OsString {
    std::env::join_paths(search_folders(std::ffi::OsStr::new(paths))).unwrap_or_default()
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

const FRAME_LIMIT: usize = 1024 * 1024;
const DISPLAY_LIMIT: usize = 8192;

type Frame = (LogStream, String, Option<String>);

async fn send_frame(
    stream: LogStream,
    bytes: &mut Vec<u8>,
    overlong: bool,
    tx: &mpsc::Sender<Frame>,
) -> bool {
    if bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
    let cut = overlong || bytes.len() > DISPLAY_LIMIT;
    let mut display =
        String::from_utf8_lossy(&bytes[..bytes.len().min(DISPLAY_LIMIT)]).into_owned();
    if cut {
        display.push('…');
    }
    let full = (!overlong).then(|| String::from_utf8_lossy(bytes).into_owned());
    tx.send((stream, display, full)).await.is_ok()
}

async fn read_lines(input: impl AsyncRead + Unpin, stream: LogStream, tx: mpsc::Sender<Frame>) {
    let mut reader = BufReader::new(input);
    let mut bytes = Vec::new();
    let mut overlong = false;
    loop {
        let chunk = match reader.fill_buf().await {
            Ok(chunk) => chunk,
            Err(err) => {
                let _ = tx
                    .send((
                        LogStream::System,
                        format!("ログを読み取れませんでした: {err}"),
                        None,
                    ))
                    .await;
                return;
            }
        };
        if chunk.is_empty() {
            if !bytes.is_empty() || overlong {
                send_frame(stream, &mut bytes, overlong, &tx).await;
            }
            return;
        }
        let newline = chunk.iter().position(|byte| *byte == b'\n');
        let length = newline.unwrap_or(chunk.len());
        let retained = length.min(FRAME_LIMIT - bytes.len());
        bytes.extend_from_slice(&chunk[..retained]);
        overlong |= retained < length;
        reader.consume(length + usize::from(newline.is_some()));
        if newline.is_some() {
            if !send_frame(stream, &mut bytes, overlong, &tx).await {
                return;
            }
            bytes.clear();
            overlong = false;
        }
    }
}

pub async fn run(
    spec: RunSpec,
    cancel: Cancel,
    mut on_line: impl FnMut(LogStream, String, Option<&str>) + Send,
) -> Result<ExitInfo, String> {
    if cancel.is_cancelled() {
        return Ok(ExitInfo {
            code: None,
            killed: true,
            timed_out: false,
        });
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
    child_path(command.as_std_mut());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command
            .as_std_mut()
            .creation_flags(0x0800_0000 | 0x0000_0004);
    }
    let mut child = command.spawn().map_err(|err| {
        format!(
            "「{}」を起動できませんでした: {err}",
            spec.program.display()
        )
    })?;
    #[cfg(windows)]
    let job = match windows_job::Job::attach_and_resume(&child) {
        Ok(job) => job,
        Err(err) => {
            let _ = child.kill().await;
            return Err(format!("子プロセスを安全に起動できませんでした: {err}"));
        }
    };
    let (tx, mut rx) = mpsc::channel(32);
    let stdout_reader = tokio::spawn(read_lines(
        child.stdout.take().expect("piped stdout"),
        LogStream::Stdout,
        tx.clone(),
    ));
    let stderr_reader = tokio::spawn(read_lines(
        child.stderr.take().expect("piped stderr"),
        LogStream::Stderr,
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
    let status = loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                killed = true;
                #[cfg(windows)]
                job.terminate();
                let _ = child.start_kill();
                break child.wait().await;
            }
            _ = &mut deadline => {
                timed_out = true;
                #[cfg(windows)]
                job.terminate();
                let _ = child.start_kill();
                break child.wait().await;
            }
            status = child.wait() => break status,
            line = rx.recv(), if stream_open => {
                match line {
                    Some((stream, display, full)) => on_line(stream, display, full.as_deref()),
                    None => stream_open = false,
                }
            }
        }
    };
    if let Some(writer) = writer {
        writer.abort();
        let _ = writer.await;
    }
    // Closing the job terminates any descendants still holding the output pipes.
    #[cfg(windows)]
    drop(job);
    let drain = async {
        while let Some((stream, display, full)) = rx.recv().await {
            on_line(stream, display, full.as_deref());
        }
    };
    if tokio::time::timeout(Duration::from_secs(5), drain)
        .await
        .is_err()
    {
        stdout_reader.abort();
        stderr_reader.abort();
        on_line(
            LogStream::System,
            "出力の読み取りを打ち切りました".into(),
            None,
        );
    }
    let _ = stdout_reader.await;
    let _ = stderr_reader.await;
    let status = status.map_err(|err| format!("終了を確認できませんでした: {err}"))?;
    Ok(ExitInfo {
        code: status.code(),
        killed,
        timed_out,
    })
}

#[cfg(windows)]
mod windows_job {
    use std::io;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject,
    };
    use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};

    struct Owned(HANDLE);

    impl Drop for Owned {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    pub(super) struct Job(Owned);

    // Windows handles are process-wide and may be closed from any runtime thread.
    unsafe impl Send for Job {}

    impl Job {
        /// The child must have been created with CREATE_SUSPENDED: it is put into the job
        /// before any of its code runs, so every descendant is in the job too.
        pub(super) fn attach_and_resume(child: &tokio::process::Child) -> io::Result<Self> {
            let job = Self::create()?;
            let process = child
                .raw_handle()
                .ok_or_else(|| io::Error::other("子プロセスのハンドルがありません"))?;
            let pid = child
                .id()
                .ok_or_else(|| io::Error::other("子プロセスの ID がありません"))?;
            unsafe {
                if AssignProcessToJobObject((job.0).0, process as HANDLE) == 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            resume_threads(pid)?;
            Ok(job)
        }

        fn create() -> io::Result<Self> {
            unsafe {
                let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if handle.is_null() {
                    return Err(io::Error::last_os_error());
                }
                let job = Self(Owned(handle));
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
                Ok(job)
            }
        }

        pub(super) fn terminate(&self) {
            unsafe {
                TerminateJobObject((self.0).0, 1);
            }
        }
    }

    fn resume_threads(pid: u32) -> io::Result<()> {
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
            if snapshot == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            let snapshot = Owned(snapshot);
            let mut entry: THREADENTRY32 = std::mem::zeroed();
            entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
            let mut resumed = 0;
            let mut more = Thread32First(snapshot.0, &mut entry) != 0;
            while more {
                if entry.th32OwnerProcessID == pid {
                    let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                    if thread.is_null() {
                        return Err(io::Error::last_os_error());
                    }
                    let thread = Owned(thread);
                    if ResumeThread(thread.0) == u32::MAX {
                        return Err(io::Error::last_os_error());
                    }
                    resumed += 1;
                }
                entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
                more = Thread32Next(snapshot.0, &mut entry) != 0;
            }
            if resumed == 0 {
                return Err(io::Error::other("子プロセスのスレッドが見つかりません"));
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod path_tests {
    use super::merge_paths;

    #[test]
    fn merges_registry_paths_without_duplicates() {
        assert_eq!(
            merge_paths(
                r"C:\Windows;C:\Tools;",
                r"c:\tools;C:\Users\u\AppData\Local\agy\bin",
                r";C:\Windows;D:\Bin"
            ),
            r"C:\Windows;C:\Tools;C:\Users\u\AppData\Local\agy\bin;D:\Bin"
        );
        assert_eq!(merge_paths("", "", ""), "");
        assert_eq!(merge_paths("", r"C:\A", ""), r"C:\A");
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
                timeout: Duration::from_secs(20),
            }
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn alive(pid: u32) -> bool {
        unsafe {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if process.is_null() {
                return false;
            }
            let mut code = 0;
            let alive = GetExitCodeProcess(process, &mut code) != 0 && code == 259;
            CloseHandle(process);
            alive
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
        // npm writes a POSIX shim with the dotted name next to the .cmd one.
        let dotted_cmd = dir.0.join("my-agent.cli.CMD");
        std::fs::write(&dotted_cmd, "").unwrap();
        std::fs::write(dir.0.join("my-agent.cli"), "#!/bin/sh").unwrap();
        assert_eq!(
            resolve_program(dir.0.join("my-agent.cli").to_str().unwrap()).unwrap(),
            dotted_cmd
        );
        let exe = dir.0.join("tool.exe");
        std::fs::write(&exe, "").unwrap();
        assert_eq!(resolve_program(exe.to_str().unwrap()).unwrap(), exe);
        std::fs::remove_file(&cmd).unwrap();
        assert!(resolve_program(dir.0.join("fake").to_str().unwrap()).is_err());
        assert!(resolve_program(dir.0.join("missing").to_str().unwrap()).is_err());
    }

    #[test]
    fn child_path_keeps_only_absolute_entries() {
        assert_eq!(
            absolute_path_list(r".;C:\Tools;;relative\bin;D:\Other\;.."),
            std::ffi::OsString::from(r"C:\Tools;D:\Other\")
        );
        let mut command = std::process::Command::new("unused");
        child_path(&mut command);
        let env: Vec<_> = command.get_envs().collect();
        assert!(env.contains(&(
            std::ffi::OsStr::new("NoDefaultCurrentDirectoryInExePath"),
            Some(std::ffi::OsStr::new("1"))
        )));
    }

    #[test]
    fn relative_programs_and_path_entries_are_not_searched() {
        for program in [r"bin\tool", "./tool", "sub/tool.exe", r"\tool", "C:tool"] {
            let error = resolve_program(program).unwrap_err();
            assert!(error.contains("名前か絶対パス"), "{program}: {error}");
        }
        let folders: Vec<_> = search_folders(std::ffi::OsStr::new(
            r";.;C:\Tools;relative\bin;D:\Other\;..",
        ))
        .collect();
        assert_eq!(
            folders,
            [PathBuf::from(r"C:\Tools"), PathBuf::from(r"D:\Other\")]
        );
    }

    #[tokio::test]
    async fn stdin_streams_exit_and_cleanup() {
        let dir = TempDir::new();
        let mut spec = dir.spec("[Console]::Out.WriteLine([Console]::In.ReadToEnd()); [Console]::Error.WriteLine('stderr-line'); exit 3");
        spec.stdin = Some("stdin-value".into());
        let prompt_path = dir.0.join("nested/prompt.txt");
        spec.prompt_file = Some((prompt_path.clone(), "prompt".into()));
        let mut lines = Vec::new();
        let exit = run(spec, Cancel::new(), |stream, line, _| {
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
    async fn full_line_reaches_parser_while_log_is_short() {
        let dir = TempDir::new();
        let spec = dir.spec("[Console]::Out.WriteLine(('{\"result\":\"' + ('x' * 10000) + '\"}'))");
        let mut lines = Vec::new();
        let mut frames = Vec::new();
        let exit = run(spec, Cancel::new(), |stream, line, full| {
            if stream == LogStream::Stdout {
                lines.push(line);
                frames.push(full.map(str::to_owned));
            }
        })
        .await
        .unwrap();
        assert_eq!(exit.code, Some(0));
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].chars().count(), 8193);
        assert!(lines[0].ends_with('…'));
        let frame = frames[0].as_deref().unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(frame).unwrap()["result"]
                .as_str()
                .unwrap()
                .len(),
            10000
        );
    }

    #[tokio::test]
    async fn output_without_newline_is_bounded() {
        let dir = TempDir::new();
        // 3 MiB without a newline, then one normal line.
        let spec = dir.spec("$s = 'y' * 65536; for ($i = 0; $i -lt 48; $i++) { [Console]::Out.Write($s) }; [Console]::Out.WriteLine(''); [Console]::Out.WriteLine('after')");
        let mut frames = Vec::new();
        let exit = run(spec, Cancel::new(), |stream, line, full| {
            if stream == LogStream::Stdout {
                frames.push((line, full.map(str::len)));
            }
        })
        .await
        .unwrap();
        assert_eq!(exit.code, Some(0));
        assert_eq!(frames.len(), 2);
        assert!(frames[0].0.ends_with('…'));
        assert_eq!(
            frames[0].1, None,
            "an overlong frame is not passed to the parser"
        );
        assert_eq!(frames[1], ("after".into(), Some(5)));
    }

    #[tokio::test]
    async fn truncates_long_lines_and_times_out() {
        let dir = TempDir::new();
        let mut spec = dir.spec("[Console]::Out.WriteLine(('x' * 9000)); Start-Sleep 60");
        spec.timeout = Duration::from_secs(2);
        let mut lines = Vec::new();
        let exit = run(spec, Cancel::new(), |stream, line, _| {
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
    async fn cancellation_kills_child_and_grandchild() {
        let dir = TempDir::new();
        let pid_file = dir.0.join("pid.txt");
        let script = format!(
            "$p = Start-Process powershell.exe -WindowStyle Hidden -ArgumentList '-NoProfile -NonInteractive -Command Start-Sleep 60' -PassThru; [IO.File]::WriteAllText('{}', \"$PID $($p.Id)\"); [Console]::Out.WriteLine('ready'); Start-Sleep 60",
            pid_file.to_string_lossy().replace('\'', "''")
        );
        let spec = dir.spec(&script);
        let cancel = Cancel::new();
        let runner_cancel = cancel.clone();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(async move {
            let mut ready = Some(ready_tx);
            run(spec, runner_cancel, move |stream, line, _| {
                if stream == LogStream::Stdout && line == "ready" {
                    if let Some(tx) = ready.take() {
                        let _ = tx.send(());
                    }
                }
            })
            .await
        });
        tokio::time::timeout(Duration::from_secs(20), ready_rx)
            .await
            .unwrap()
            .unwrap();
        let pids: Vec<u32> = std::fs::read_to_string(&pid_file)
            .unwrap()
            .split_whitespace()
            .map(|pid| pid.parse().unwrap())
            .collect();
        assert_eq!(pids.len(), 2);
        assert!(pids.iter().all(|pid| alive(*pid)));
        cancel.cancel();
        let exit = tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(exit.killed);
        tokio::time::timeout(Duration::from_secs(5), async {
            while pids.iter().any(|pid| alive(*pid)) {
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
            run(dir.spec("exit 0"), cancel, |_, _, _| {})
                .await
                .unwrap()
                .killed
        );
        let mut spec = dir.spec("exit 0");
        spec.program = dir.0.join("missing.exe");
        let prompt = dir.0.join("prompt.txt");
        spec.prompt_file = Some((prompt.clone(), "body".into()));
        assert!(run(spec, Cancel::new(), |_, _, _| {}).await.is_err());
        assert!(!prompt.exists());
    }
}
