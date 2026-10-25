//! ACP host for one `grok agent --no-leader stdio` child.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    AgentNotification, CancelNotification, ClientCapabilities, ClientSessionCapabilities,
    CloseSessionRequest, CompactionCapabilities, ContentBlock, Implementation, InitializeRequest,
    ListSessionsRequest, LoadSessionRequest, NewSessionRequest, NoticeCapabilities, PromptRequest,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionConfigOption, SessionId, SessionModeState, SessionUpdate,
    SetSessionConfigOptionRequest, SetSessionModeRequest,
};
use agent_client_protocol::{AcpAgent, AcpAgentConfig, Agent, Client, ConnectionTo};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};
use tokio::sync::{oneshot, watch, Notify};

use crate::usage::{
    classify_image_bytes, context_window, current_model, decode_image, encode_image,
    foreign_owner, image_mime_from_name, load_path, parse_active_sessions, prompt_blocks,
    turn_reported_nothing, usage_from_value, used_tokens, LoadedPath, OutgoingImage, TurnUsage,
};

const MODE_WAIT: Duration = Duration::from_millis(800);
const REPLAY_BATCH: usize = 48;

pub struct AgentState {
    tx: tokio::sync::mpsc::UnboundedSender<Cmd>,
    wake: Arc<Notify>,
    shutdown: Arc<AtomicBool>,
    folder: Arc<Mutex<Option<PathBuf>>>,
    our_pid: Arc<Mutex<Option<u32>>>,
    mirror: Arc<Mutex<Mirror>>,
    images: Arc<ImageStore>,
    prompt_image: Arc<AtomicBool>,
    auto_approve: Arc<AtomicBool>,
    join: Mutex<Option<JoinHandle<()>>>,
}

struct StagedImage {
    mime: String,
    bytes: Vec<u8>,
}

struct ImageStore {
    next: AtomicU64,
    items: Mutex<HashMap<String, StagedImage>>,
}

impl ImageStore {
    fn new() -> Self {
        Self {
            next: AtomicU64::new(1),
            items: Mutex::new(HashMap::new()),
        }
    }
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentRecord {
    pub kind: String,
    pub id: Option<String>,
    pub path: Option<String>,
    pub name: String,
    pub mime_type: Option<String>,
    pub preview: Option<String>,
    pub message: Option<String>,
}

enum Cmd {
    SetFolder { path: String, reply: Reply },
    NewSession { reply: Reply },
    LoadSession { id: String, reply: Reply },
    CloseSession { reply: Reply },
    Refresh { reply: Reply },
    Prompt {
        text: String,
        files: Vec<String>,
        image_ids: Vec<String>,
        reply: Reply,
    },
    Compact { reply: Reply },
    Cancel { reply: Reply },
    SetMode { mode_id: String, reply: Reply },
    SetConfig { id: String, value: String, reply: Reply },
    Decide { id: u64, option: Option<String>, reply: Reply },
    Delete { id: String, reply: Reply },
    Shutdown,
}

struct Reply {
    tx: Option<oneshot::Sender<Result<(), String>>>,
}

impl Reply {
    fn new(tx: oneshot::Sender<Result<(), String>>) -> Self {
        Self { tx: Some(tx) }
    }

    fn send(mut self, result: Result<(), String>) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(result);
        }
    }
}

impl Drop for Reply {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Err("エージェントとの接続が切れました".into()));
        }
    }
}

struct Permits {
    next: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<Option<String>>>>,
}

impl Permits {
    fn new() -> Self {
        Self {
            next: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
        }
    }

    fn insert(&self, tx: oneshot::Sender<Option<String>>) -> u64 {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        lock(&self.pending).insert(id, tx);
        id
    }

    fn answer(&self, id: u64, option: Option<String>) -> bool {
        match lock(&self.pending).remove(&id) {
            Some(tx) => {
                let _ = tx.send(option);
                true
            }
            None => false,
        }
    }

    fn cancel_all(&self) -> Vec<u64> {
        let pending = std::mem::take(&mut *lock(&self.pending));
        let ids = pending.keys().copied().collect();
        for (_, tx) in pending {
            let _ = tx.send(None);
        }
        ids
    }
}

#[derive(Default)]
struct Mirror {
    ready: Option<Value>,
    sessions: Option<Value>,
    session: Option<Value>,
    config: Option<Value>,
    mode: Option<Value>,
    usage: Option<Value>,
}

struct Shared {
    app: AppHandle,
    folder: Arc<Mutex<Option<PathBuf>>>,
    our_pid: Arc<Mutex<Option<u32>>>,
    window: AtomicU64,
    busy: AtomicBool,
    replaying: AtomicBool,
    saw_compaction: AtomicBool,
    saw_agent_text: AtomicBool,
    permits: Permits,
    mode_tx: watch::Sender<(u64, Option<String>)>,
    mirror: Arc<Mutex<Mirror>>,
    images: Arc<ImageStore>,
    prompt_image: Arc<AtomicBool>,
    auto_approve: Arc<AtomicBool>,
    replay_buf: Mutex<Vec<Value>>,
}

struct ReplayGuard<'a> {
    shared: &'a Shared,
}

impl<'a> ReplayGuard<'a> {
    fn arm(shared: &'a Shared) -> Self {
        shared.replaying.store(true, Ordering::SeqCst);
        Self { shared }
    }
}

impl Drop for ReplayGuard<'_> {
    fn drop(&mut self) {
        finish_replay(self.shared);
    }
}

enum End {
    Shutdown,
    Lost(String),
}

impl AgentState {
    pub fn start(app: AppHandle) -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let wake = Arc::new(Notify::new());
        let shutdown = Arc::new(AtomicBool::new(false));
        let folder = Arc::new(Mutex::new(None));
        let our_pid = Arc::new(Mutex::new(None));
        let mirror = Arc::new(Mutex::new(Mirror::default()));
        let images = Arc::new(ImageStore::new());
        let prompt_image = Arc::new(AtomicBool::new(false));
        let auto_approve = Arc::new(AtomicBool::new(true));
        let thread_wake = wake.clone();
        let thread_shutdown = shutdown.clone();
        let thread_folder = folder.clone();
        let thread_pid = our_pid.clone();
        let thread_mirror = mirror.clone();
        let thread_images = images.clone();
        let thread_prompt_image = prompt_image.clone();
        let thread_auto_approve = auto_approve.clone();
        let join = std::thread::Builder::new()
            .name("grok-acp".into())
            .spawn(move || {
                agent_thread(
                    app,
                    rx,
                    thread_wake,
                    thread_shutdown,
                    thread_folder,
                    thread_pid,
                    thread_mirror,
                    thread_images,
                    thread_prompt_image,
                    thread_auto_approve,
                )
            })
            .expect("acp thread");
        Self {
            tx,
            wake,
            shutdown,
            folder,
            our_pid,
            mirror,
            images,
            prompt_image,
            auto_approve,
            join: Mutex::new(Some(join)),
        }
    }

    pub fn set_auto_approve(&self, enabled: bool) {
        self.auto_approve.store(enabled, Ordering::SeqCst);
    }

    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = self.tx.send(Cmd::Shutdown);
        self.wake.notify_waiters();
        if let Some(handle) = lock(&self.join).take() {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = handle.join();
                let _ = tx.send(());
            });
            let _ = rx.recv_timeout(Duration::from_secs(2));
        }
    }

    pub fn folder(&self) -> Option<PathBuf> {
        lock(&self.folder).clone()
    }

    pub fn our_pid(&self) -> Option<u32> {
        *lock(&self.our_pid)
    }

    pub fn snapshot(&self) -> Value {
        let mirror = lock(&self.mirror);
        json!({
            "ready": mirror.ready,
            "sessions": mirror.sessions,
            "session": mirror.session,
            "config": mirror.config,
            "mode": mirror.mode,
            "usage": mirror.usage,
        })
    }

    pub async fn set_folder(&self, path: String) -> Result<(), String> {
        self.roundtrip(|reply| Cmd::SetFolder { path, reply }).await
    }

    pub async fn new_session(&self) -> Result<(), String> {
        self.roundtrip(|reply| Cmd::NewSession { reply }).await
    }

    pub async fn load_session(&self, id: String) -> Result<(), String> {
        self.roundtrip(|reply| Cmd::LoadSession { id, reply }).await
    }

    pub async fn close_session(&self) -> Result<(), String> {
        self.roundtrip(|reply| Cmd::CloseSession { reply }).await
    }

    pub async fn refresh_sessions(&self) -> Result<(), String> {
        self.roundtrip(|reply| Cmd::Refresh { reply }).await
    }

    pub async fn prompt(
        &self,
        text: String,
        files: Vec<String>,
        image_ids: Vec<String>,
    ) -> Result<(), String> {
        self.roundtrip(|reply| Cmd::Prompt {
            text,
            files,
            image_ids,
            reply,
        })
        .await
    }

    pub fn stage_paths(&self, paths: Vec<String>) -> Vec<AttachmentRecord> {
        paths
            .into_iter()
            .map(|path| self.stage_one_path(path))
            .collect()
    }

    pub fn stage_bytes(&self, name: String, data_base64: String) -> AttachmentRecord {
        if !self.prompt_image.load(Ordering::Relaxed) {
            return rejected(name, "この Grok は画像入力を告知していません");
        }
        let Some(expected) = image_mime_from_name(&name) else {
            return rejected(name, "png、jpeg、gif、webp だけを画像として添付できます");
        };
        let bytes = match decode_image(&data_base64) {
            Ok(bytes) => bytes,
            Err(message) => return rejected(name, message),
        };
        let mime = match classify_image_bytes(&name, expected, &bytes) {
            Ok(mime) => mime.to_string(),
            Err(message) => return rejected(name, message),
        };
        self.store_image(name, mime, bytes, None)
    }

    pub fn unstage(&self, id: String) {
        lock(&self.images.items).remove(&id);
    }

    fn stage_one_path(&self, path: String) -> AttachmentRecord {
        let file_path = Path::new(&path);
        if image_mime_from_name(&path).is_some() && !self.prompt_image.load(Ordering::Relaxed) {
            if file_path.is_file() {
                return path_record(path);
            }
            return rejected(file_name_of(&path), "ファイルを読めません");
        }
        match load_path(file_path) {
            Ok(LoadedPath::Other { path }) => {
                let name = file_name_of(&path);
                AttachmentRecord {
                    kind: "file".into(),
                    id: None,
                    path: Some(path),
                    name,
                    mime_type: None,
                    preview: None,
                    message: None,
                }
            }
            Ok(LoadedPath::Image { name, mime, bytes }) => {
                self.store_image(name, mime, bytes, Some(path))
            }
            Err(message) => {
                if image_mime_from_name(&path).is_some() && file_path.is_file() {
                    return path_record(path);
                }
                rejected(file_name_of(&path), message)
            }
        }
    }

    fn store_image(
        &self,
        name: String,
        mime: String,
        bytes: Vec<u8>,
        path: Option<String>,
    ) -> AttachmentRecord {
        let id = self.images.next.fetch_add(1, Ordering::Relaxed).to_string();
        let preview = format!("data:{mime};base64,{}", encode_image(&bytes));
        lock(&self.images.items).insert(
            id.clone(),
            StagedImage {
                mime: mime.clone(),
                bytes,
            },
        );
        AttachmentRecord {
            kind: "image".into(),
            id: Some(id),
            path,
            name,
            mime_type: Some(mime),
            preview: Some(preview),
            message: None,
        }
    }

    pub async fn compact(&self) -> Result<(), String> {
        self.roundtrip(|reply| Cmd::Compact { reply }).await
    }

    pub async fn cancel(&self) -> Result<(), String> {
        self.roundtrip(|reply| Cmd::Cancel { reply }).await
    }

    pub async fn set_mode(&self, mode_id: String) -> Result<(), String> {
        self.roundtrip(|reply| Cmd::SetMode { mode_id, reply }).await
    }

    pub async fn set_config(&self, id: String, value: String) -> Result<(), String> {
        self.roundtrip(|reply| Cmd::SetConfig { id, value, reply })
            .await
    }

    pub async fn decide_permission(&self, id: u64, option: Option<String>) -> Result<(), String> {
        self.roundtrip(|reply| Cmd::Decide { id, option, reply })
            .await
    }

    pub async fn delete_session(&self, id: String) -> Result<(), String> {
        self.roundtrip(|reply| Cmd::Delete { id, reply }).await
    }

    async fn roundtrip(&self, build: impl FnOnce(Reply) -> Cmd) -> Result<(), String> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(build(Reply::new(tx)))
            .map_err(|_| "アプリが終了しています".to_string())?;
        self.wake.notify_waiters();
        match rx.await {
            Ok(result) => result,
            Err(_) => Err("エージェントとの接続が切れました".into()),
        }
    }
}

pub fn spawn_login() -> Result<(), String> {
    let mut cmd = Command::new("grok");
    cmd.arg("login");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0000_0010);
    }
    cmd.spawn()
        .map_err(|error| format!("grok login を起動できませんでした: {error}"))?;
    Ok(())
}

pub fn owner_warning(session_id: &str, our_pid: Option<u32>) -> Option<String> {
    foreign_owner(&read_active(), session_id, our_pid).map(|owner| {
        format!(
            "このセッションは別のプロセス（pid {}）が開いている可能性があります。フォルダは {} です。開くと、そちらの作業と競合することがあります。",
            owner.pid, owner.cwd
        )
    })
}

fn path_record(path: String) -> AttachmentRecord {
    let raw = Path::new(&path);
    let absolute = if raw.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(raw))
            .unwrap_or_else(|_| raw.to_path_buf())
            .to_string_lossy()
            .into_owned()
    };
    let name = file_name_of(&absolute);
    AttachmentRecord {
        kind: "path".into(),
        id: None,
        path: Some(absolute),
        name,
        mime_type: None,
        preview: None,
        message: None,
    }
}

fn rejected(name: String, message: impl Into<String>) -> AttachmentRecord {
    AttachmentRecord {
        kind: "rejected".into(),
        id: None,
        path: None,
        name,
        mime_type: None,
        preview: None,
        message: Some(message.into()),
    }
}

fn file_name_of(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(path)
        .to_string()
}

fn agent_thread(
    app: AppHandle,
    rx: tokio::sync::mpsc::UnboundedReceiver<Cmd>,
    wake: Arc<Notify>,
    shutdown: Arc<AtomicBool>,
    folder: Arc<Mutex<Option<PathBuf>>>,
    our_pid: Arc<Mutex<Option<u32>>>,
    mirror: Arc<Mutex<Mirror>>,
    images: Arc<ImageStore>,
    prompt_image: Arc<AtomicBool>,
    auto_approve: Arc<AtomicBool>,
) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio");
    runtime.block_on(async move {
        let rx = Arc::new(tokio::sync::Mutex::new(rx));
        let (mode_tx, _mode_rx) = watch::channel((0_u64, None));
        let shared = Arc::new(Shared {
            app,
            folder,
            our_pid,
            window: AtomicU64::new(0),
            busy: AtomicBool::new(false),
            replaying: AtomicBool::new(false),
            saw_compaction: AtomicBool::new(false),
            saw_agent_text: AtomicBool::new(false),
            permits: Permits::new(),
            mode_tx,
            mirror,
            images,
            prompt_image,
            auto_approve,
            replay_buf: Mutex::new(Vec::new()),
        });
        loop {
            if shutdown.load(Ordering::SeqCst) {
                break;
            }
            match run_connection(shared.clone(), rx.clone()).await {
                End::Shutdown => break,
                End::Lost(message) => {
                    emit_error(&shared, &message);
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(8)) => {}
                        _ = wake.notified() => {}
                    }
                }
            }
        }
    });
}

async fn run_connection(
    shared: Arc<Shared>,
    rx: Arc<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<Cmd>>>,
) -> End {
    let agent = AcpAgent::new(
        AcpAgentConfig::new("grok")
            .arg("--permission-mode")
            .arg("default")
            .arg("agent")
            .arg("--no-leader")
            .arg("stdio"),
    );
    let handler_shared = shared.clone();
    let permission_shared = shared.clone();
    let connect = Client
        .builder()
        .on_receive_notification(
            async move |notification: AgentNotification, connection| {
                let refresh = sessions_changed(&notification);
                handle_notification(&handler_shared, notification);
                if refresh {
                    let shared = handler_shared.clone();
                    let list = connection.clone();
                    let _ = connection.spawn(async move {
                        let _ = publish_sessions(&shared, &list).await;
                        Ok(())
                    });
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, connection| {
                let tool = serde_json::to_value(&request.tool_call).unwrap_or(Value::Null);
                let options = serde_json::to_value(&request.options).unwrap_or(json!([]));
                let folder = lock(&permission_shared.folder).clone();
                let verdict = crate::permit::review(&tool, &options, folder.as_deref());
                if permission_shared.auto_approve.load(Ordering::SeqCst) {
                    if let Some(option_id) = verdict.allow_once.clone() {
                        let spawned = connection.spawn(async move {
                            let _ = responder.respond(RequestPermissionResponse::new(
                                RequestPermissionOutcome::Selected(
                                    SelectedPermissionOutcome::new(option_id),
                                ),
                            ));
                            Ok(())
                        });
                        if spawned.is_err() {
                            emit_error(&permission_shared, "自動承認を返せませんでした");
                        }
                        return Ok(());
                    }
                }
                let (tx, rx) = oneshot::channel();
                let id = permission_shared.permits.insert(tx);
                emit(
                    &permission_shared,
                    json!({
                        "type": "permission",
                        "id": id,
                        "sessionId": request.session_id.to_string(),
                        "tool": tool,
                        "options": options,
                        "reason": verdict.reason,
                        "detail": verdict.detail,
                    }),
                );
                let spawned = connection.spawn(async move {
                    let choice = rx.await.ok().flatten();
                    let outcome = match choice {
                        Some(option_id) => RequestPermissionOutcome::Selected(
                            SelectedPermissionOutcome::new(option_id),
                        ),
                        None => RequestPermissionOutcome::Cancelled,
                    };
                    let _ = responder.respond(RequestPermissionResponse::new(outcome));
                    Ok(())
                });
                if spawned.is_err() {
                    permission_shared.permits.answer(id, None);
                    emit(
                        &permission_shared,
                        json!({ "type": "permission-clear", "id": id }),
                    );
                }
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent, |connection| async move {
            if let Err(error) = initialize(&shared, &connection).await {
                return Err(error);
            }
            let mut session_id: Option<String> = None;
            loop {
                let cmd = {
                    let mut inbox = rx.lock().await;
                    inbox.recv().await
                };
                let Some(cmd) = cmd else {
                    return Ok(());
                };
                if matches!(cmd, Cmd::Shutdown) {
                    return Ok(());
                }
                if let Err(error) = dispatch(shared.clone(), &connection, &mut session_id, cmd).await
                {
                    return Err(error);
                }
            }
        });
    match connect.await {
        Ok(()) => End::Shutdown,
        Err(error) => End::Lost(format_acp(&error)),
    }
}

async fn initialize(
    shared: &Shared,
    connection: &ConnectionTo<Agent>,
) -> Result<(), agent_client_protocol::Error> {
    let response = connection
        .send_request(
            InitializeRequest::new(ProtocolVersion::V1)
                .client_info(Implementation::new(
                    "grok-build-desktop",
                    env!("CARGO_PKG_VERSION"),
                ))
                .client_capabilities(
                    ClientCapabilities::new().terminal(false).session(
                        ClientSessionCapabilities::new()
                            .compaction(CompactionCapabilities::new())
                            .notices(NoticeCapabilities::new()),
                    ),
                ),
        )
        .block_task()
        .await?;
    let meta = response
        .meta
        .clone()
        .map(Value::Object)
        .unwrap_or(Value::Null);
    if let Some(size) = context_window(&meta) {
        shared.window.store(size, Ordering::Relaxed);
    }
    let prompt_image = response.agent_capabilities.prompt_capabilities.image;
    shared
        .prompt_image
        .store(prompt_image, Ordering::Relaxed);
    emit(
        &shared,
        json!({
            "type": "ready",
            "name": response.agent_info.as_ref().map(|info| info.name.clone()),
            "version": response.agent_info.as_ref().map(|info| info.version.clone()),
            "model": current_model(&meta),
            "contextTokens": shared.window.load(Ordering::Relaxed),
            "modelState": meta.get("modelState").cloned(),
            "promptImage": prompt_image,
        }),
    );
    Ok(())
}

async fn dispatch(
    shared: Arc<Shared>,
    connection: &ConnectionTo<Agent>,
    session_id: &mut Option<String>,
    cmd: Cmd,
) -> Result<(), agent_client_protocol::Error> {
    match cmd {
        Cmd::Shutdown => Ok(()),
        Cmd::SetFolder { path, reply } => {
            let folder = PathBuf::from(&path);
            if !folder.is_absolute() || !folder.is_dir() {
                reply.send(Err("プロジェクトフォルダが見つかりません".into()));
                return Ok(());
            }
            *lock(&shared.folder) = Some(folder);
            answer_publish(&shared, connection, reply).await
        }
        Cmd::Refresh { reply } => answer_publish(&shared, connection, reply).await,
        Cmd::NewSession { reply } => {
            let Some(folder) = lock(&shared.folder).clone() else {
                reply.send(Err("先にプロジェクトフォルダを選んでください".into()));
                return Ok(());
            };
            if shared.busy.load(Ordering::SeqCst) {
                reply.send(Err("応答の途中です。停止してから切り替えてください".into()));
                return Ok(());
            }
            close_open(connection, session_id).await;
            emit(&shared, json!({ "type": "session-loading", "id": null }));
            match connection
                .send_request(NewSessionRequest::new(folder.clone()))
                .block_task()
                .await
            {
                Ok(response) => {
                    let id = response.session_id.to_string();
                    *session_id = Some(id.clone());
                    remember_pid(&shared, &id);
                    emit_session(
                        &shared,
                        &id,
                        &folder,
                        None,
                        response.config_options.as_ref(),
                        response.modes.as_ref(),
                    );
                    reply.send(Ok(()));
                    publish_soon(&shared, connection).await;
                    Ok(())
                }
                Err(error) => fail_command(&shared, reply, error).await,
            }
        }
        Cmd::LoadSession { id, reply } => {
            let Some(folder) = lock(&shared.folder).clone() else {
                reply.send(Err("先にプロジェクトフォルダを選んでください".into()));
                return Ok(());
            };
            if shared.busy.load(Ordering::SeqCst) {
                reply.send(Err("応答の途中です。停止してから切り替えてください".into()));
                return Ok(());
            }
            let warning = owner_warning(&id, *lock(&shared.our_pid));
            close_open(connection, session_id).await;
            let replay = ReplayGuard::arm(&shared);
            emit(&shared, json!({ "type": "session-loading", "id": id }));
            let loaded = connection
                .send_request(LoadSessionRequest::new(
                    SessionId::new(id.clone()),
                    folder.clone(),
                ))
                .block_task()
                .await;
            drop(replay);
            match loaded {
                Ok(response) => {
                    *session_id = Some(id.clone());
                    remember_pid(&shared, &id);
                    emit_session(
                        &shared,
                        &id,
                        &folder,
                        warning.as_deref(),
                        response.config_options.as_ref(),
                        response.modes.as_ref(),
                    );
                    reply.send(Ok(()));
                    publish_soon(&shared, connection).await;
                    Ok(())
                }
                Err(error) => fail_command(&shared, reply, error).await,
            }
        }
        Cmd::CloseSession { reply } => {
            close_open(connection, session_id).await;
            emit(&shared, json!({ "type": "session-closed" }));
            answer_publish(&shared, connection, reply).await
        }
        Cmd::Prompt {
            text,
            files,
            image_ids,
            reply,
        } => {
            run_prompt(
                shared,
                connection,
                session_id,
                text,
                files,
                image_ids,
                reply,
            )
            .await
        }
        Cmd::Compact { reply } => {
            run_prompt(
                shared,
                connection,
                session_id,
                "/compact".into(),
                Vec::new(),
                Vec::new(),
                reply,
            )
            .await
        }
        Cmd::Cancel { reply } => {
            cancel_turn(&shared, connection, session_id.as_deref());
            reply.send(Ok(()));
            Ok(())
        }
        Cmd::Decide { id, option, reply } => {
            if shared.permits.answer(id, option) {
                emit(&shared, json!({ "type": "permission-clear", "id": id }));
                reply.send(Ok(()));
            } else {
                reply.send(Err("その確認はもう閉じられています".into()));
            }
            Ok(())
        }
        Cmd::SetMode { mode_id, reply } => {
            let Some(id) = session_id.clone() else {
                reply.send(Err("セッションがありません".into()));
                return Ok(());
            };
            let mut mode_rx = shared.mode_tx.subscribe();
            let generation = mode_rx.borrow().0;
            match connection
                .send_request(SetSessionModeRequest::new(
                    SessionId::new(id),
                    mode_id.clone(),
                ))
                .block_task()
                .await
            {
                Ok(_) => {
                    let confirmed = wait_for_mode(&mut mode_rx, generation, &mode_id).await;
                    if confirmed {
                        reply.send(Ok(()));
                    } else {
                        reply.send(Err(
                            "モードの変更を確認できませんでした。表示は届いた通知に合わせます".into(),
                        ));
                    }
                    Ok(())
                }
                Err(error) => fail_command(&shared, reply, error).await,
            }
        }
        Cmd::SetConfig { id, value, reply } => {
            let Some(session) = session_id.clone() else {
                reply.send(Err("セッションがありません".into()));
                return Ok(());
            };
            match connection
                .send_request(SetSessionConfigOptionRequest::new(
                    SessionId::new(session),
                    id,
                    value.as_str(),
                ))
                .block_task()
                .await
            {
                Ok(response) => {
                    emit(
                        &shared,
                        json!({
                            "type": "config",
                            "options": serde_json::to_value(&response.config_options).unwrap_or(json!([])),
                        }),
                    );
                    reply.send(Ok(()));
                    Ok(())
                }
                Err(error) => fail_command(&shared, reply, error).await,
            }
        }
        Cmd::Delete { id, reply } => {
            if shared.busy.load(Ordering::SeqCst) && session_id.as_deref() == Some(id.as_str()) {
                reply.send(Err("応答の途中のセッションは削除できません".into()));
                return Ok(());
            }
            if session_id.as_deref() == Some(id.as_str()) {
                close_open(connection, session_id).await;
                emit(&shared, json!({ "type": "session-closed" }));
            }
            let deleted = delete_session_cli(&id);
            if deleted.is_err() {
                reply.send(deleted);
                return Ok(());
            }
            answer_publish(&shared, connection, reply).await
        }
    }
}

async fn publish_soon(shared: &Arc<Shared>, connection: &ConnectionTo<Agent>) {
    let background = shared.clone();
    let list = connection.clone();
    let spawned = connection.spawn(async move {
        let _ = publish_sessions(&background, &list).await;
        Ok(())
    });
    if spawned.is_err() {
        let _ = publish_sessions(shared, connection).await;
    }
}

async fn answer_publish(
    shared: &Shared,
    connection: &ConnectionTo<Agent>,
    reply: Reply,
) -> Result<(), agent_client_protocol::Error> {
    match publish_sessions(shared, connection).await {
        Ok(()) => {
            reply.send(Ok(()));
            Ok(())
        }
        Err(error) => fail_command(shared, reply, error).await,
    }
}

async fn fail_command(
    shared: &Shared,
    reply: Reply,
    error: agent_client_protocol::Error,
) -> Result<(), agent_client_protocol::Error> {
    let message = format_acp(&error);
    emit_error(&shared, &message);
    reply.send(Err(message));
    if connection_dead(&error) {
        Err(error)
    } else {
        Ok(())
    }
}

async fn run_prompt(
    shared: Arc<Shared>,
    connection: &ConnectionTo<Agent>,
    session_id: &Option<String>,
    text: String,
    files: Vec<String>,
    image_ids: Vec<String>,
    reply: Reply,
) -> Result<(), agent_client_protocol::Error> {
    let Some(session) = session_id.clone() else {
        reply.send(Err("セッションがありません".into()));
        return Ok(());
    };
    if text.trim().is_empty() && image_ids.is_empty() {
        reply.send(Err("空のメッセージは送れません".into()));
        return Ok(());
    }
    let images = match prepared_images(&shared, &image_ids) {
        Ok(images) => images,
        Err(message) => {
            reply.send(Err(message));
            return Ok(());
        }
    };
    if shared.busy.swap(true, Ordering::SeqCst) {
        reply.send(Err("応答の途中です。停止してから送ってください".into()));
        return Ok(());
    }
    shared.saw_compaction.store(false, Ordering::SeqCst);
    shared.saw_agent_text.store(false, Ordering::SeqCst);
    emit(&shared, json!({ "type": "busy", "busy": true }));
    let blocks = prompt_blocks(&text, &files, &images);
    let prompt_connection = connection.clone();
    let task_shared = shared.clone();
    let slash = slash_kind(&text).map(str::to_string);
    let sent_image_ids = image_ids.clone();
    let spawned = connection.spawn(async move {
        let result = prompt_connection
            .send_request(PromptRequest::new(SessionId::new(session), blocks))
            .block_task()
            .await;
        match result {
            Ok(response) => {
                {
                    let mut staged = lock(&task_shared.images.items);
                    for id in &sent_image_ids {
                        staged.remove(id);
                    }
                }
                let usage = response.meta.as_ref().and_then(|meta| {
                    usage_from_value(&Value::Object(meta.clone()))
                });
                if let Some(usage) = usage.as_ref() {
                    emit_usage(&task_shared, usage);
                }
                let nothing = turn_reported_nothing(
                    usage.as_ref(),
                    task_shared.saw_compaction.load(Ordering::SeqCst),
                    task_shared.saw_agent_text.load(Ordering::SeqCst),
                );
                if nothing && slash.as_deref() == Some("compact") {
                    emit(
                        &task_shared,
                        json!({
                            "type": "notice",
                            "title": "圧縮結果は返りませんでした",
                            "description": "ACP はこの圧縮の結果を返しませんでした。Grok 内部の自動圧縮は続いています。",
                        }),
                    );
                } else if nothing && slash.as_deref() == Some("context") {
                    emit(
                        &task_shared,
                        json!({
                            "type": "notice",
                            "title": "コンテキストは使用量から表示しています",
                            "description": "/context は ACP では内訳を返しません。右のパネルは、ターンの入力トークンとモデルのウィンドウで空きを出しています。",
                        }),
                    );
                }
                emit(
                    &task_shared,
                    json!({
                        "type": "turn",
                        "stopReason": stop_reason_name(&response.stop_reason),
                        "compactNoop": nothing && slash.as_deref() == Some("compact"),
                    }),
                );
                let _ = publish_sessions(&task_shared, &prompt_connection).await;
                reply.send(Ok(()));
            }
            Err(error) => {
                let message = format_acp(&error);
                emit_error(&task_shared, &message);
                reply.send(Err(message));
            }
        }
        task_shared.busy.store(false, Ordering::SeqCst);
        emit(&task_shared, json!({ "type": "busy", "busy": false }));
        Ok(())
    });
    if spawned.is_err() {
        shared.busy.store(false, Ordering::SeqCst);
        emit(&shared, json!({ "type": "busy", "busy": false }));
    }
    Ok(())
}

fn sessions_changed(notification: &AgentNotification) -> bool {
    match notification {
        AgentNotification::ExtNotification(ext) => {
            ext.method.contains("sessions/changed") || ext.method.contains("sessions_changed")
        }
        _ => false,
    }
}

fn handle_notification(shared: &Shared, notification: AgentNotification) {
    match notification {
        AgentNotification::SessionNotification(notification) => {
            let session_id = notification.session_id.to_string();
            let raw = serde_json::to_value(&notification.update).unwrap_or(Value::Null);
            match notification.update {
                SessionUpdate::UserMessageChunk(chunk) => emit_chunk(
                    shared,
                    &session_id,
                    "user",
                    &block_text(&chunk.content),
                    chunk.message_id.as_ref().map(ToString::to_string),
                ),
                SessionUpdate::AgentMessageChunk(chunk) => emit_chunk(
                    shared,
                    &session_id,
                    "assistant",
                    &block_text(&chunk.content),
                    chunk.message_id.as_ref().map(ToString::to_string),
                ),
                SessionUpdate::AgentThoughtChunk(chunk) => emit_chunk(
                    shared,
                    &session_id,
                    "thought",
                    &block_text(&chunk.content),
                    chunk.message_id.as_ref().map(ToString::to_string),
                ),
                SessionUpdate::ToolCall(call) => {
                    emit(
                        &shared,
                        json!({
                            "type": "tool",
                            "sessionId": session_id,
                            "tool": serde_json::to_value(&call).unwrap_or(Value::Null),
                        }),
                    );
                }
                SessionUpdate::ToolCallUpdate(call) => {
                    emit(
                        &shared,
                        json!({
                            "type": "tool",
                            "sessionId": session_id,
                            "tool": serde_json::to_value(&call).unwrap_or(Value::Null),
                        }),
                    );
                }
                SessionUpdate::Plan(plan) => {
                    emit(
                        &shared,
                        json!({
                            "type": "plan",
                            "sessionId": session_id,
                            "entries": serde_json::to_value(&plan.entries).unwrap_or(json!([])),
                        }),
                    );
                }
                SessionUpdate::CurrentModeUpdate(update) => {
                    let mode_id = update.current_mode_id.to_string();
                    shared.mode_tx.send_modify(|state| {
                        state.0 = state.0.wrapping_add(1);
                        state.1 = Some(mode_id.clone());
                    });
                    emit(
                        &shared,
                        json!({ "type": "mode", "sessionId": session_id, "modeId": mode_id }),
                    );
                }
                SessionUpdate::ConfigOptionUpdate(update) => {
                    emit(
                        &shared,
                        json!({
                            "type": "config",
                            "options": serde_json::to_value(&update.config_options).unwrap_or(json!([])),
                        }),
                    );
                }
                SessionUpdate::SessionInfoUpdate(_) => {
                    if let Some(title) = raw.get("title").and_then(Value::as_str) {
                        emit(
                            &shared,
                            json!({ "type": "title", "sessionId": session_id, "title": title }),
                        );
                    }
                }
                SessionUpdate::UsageUpdate(update) => {
                    if update.size > 0 {
                        shared.window.store(update.size, Ordering::Relaxed);
                    }
                    emit(
                        &shared,
                        json!({
                            "type": "usage",
                            "used": update.used,
                            "size": update.size,
                        }),
                    );
                }
                SessionUpdate::Notice(notice) => {
                    emit(
                        &shared,
                        json!({
                            "type": "notice",
                            "title": notice.title,
                            "description": notice.description,
                        }),
                    );
                }
                SessionUpdate::CompactionUpdate(_) | SessionUpdate::CompactionSummaryChunk(_) => {
                    shared.saw_compaction.store(true, Ordering::SeqCst);
                    emit(
                        &shared,
                        json!({ "type": "compaction", "sessionId": session_id, "update": raw }),
                    );
                }
                SessionUpdate::AvailableCommandsUpdate(update) => {
                    emit(
                        &shared,
                        json!({
                            "type": "commands",
                            "commands": serde_json::to_value(&update.available_commands).unwrap_or(json!([])),
                        }),
                    );
                }
                _ => {
                    emit(
                        &shared,
                        json!({ "type": "unknown", "sessionId": session_id, "update": raw }),
                    );
                }
            }
        }
        AgentNotification::ExtNotification(ext) => {
            if !ext.method.contains("session_notification") {
                return;
            }
            let Ok(value) = serde_json::from_str::<Value>(ext.params.get()) else {
                return;
            };
            if let Some(usage) = usage_from_value(&value) {
                emit_usage(shared, &usage);
            }
        }
        _ => {}
    }
}

fn emit_chunk(shared: &Shared, session_id: &str, role: &str, text: &str, message_id: Option<String>) {
    if text.is_empty() {
        return;
    }
    if role != "user" {
        shared.saw_agent_text.store(true, Ordering::SeqCst);
    }
    emit(
        &shared,
        json!({
            "type": "chunk",
            "sessionId": session_id,
            "role": role,
            "text": text,
            "messageId": message_id,
            "replay": shared.replaying.load(Ordering::SeqCst),
        }),
    );
}

fn block_text(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text(text) => text.text.clone(),
        ContentBlock::ResourceLink(link) => link.name.clone(),
        ContentBlock::Image(_) => "（画像）".to_string(),
        _ => String::new(),
    }
}

fn prepared_images(shared: &Shared, ids: &[String]) -> Result<Vec<OutgoingImage>, String> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    if !shared.prompt_image.load(Ordering::Relaxed) {
        return Err("この Grok は画像入力を告知していません".into());
    }
    let staged = lock(&shared.images.items);
    let mut images = Vec::with_capacity(ids.len());
    for id in ids {
        let Some(image) = staged.get(id) else {
            return Err("添付した画像が見つかりません。もう一度ドロップしてください".into());
        };
        images.push(OutgoingImage {
            mime_type: image.mime.clone(),
            data: encode_image(&image.bytes),
        });
    }
    Ok(images)
}

async fn publish_sessions(
    shared: &Shared,
    connection: &ConnectionTo<Agent>,
) -> Result<(), agent_client_protocol::Error> {
    let Some(folder) = lock(&shared.folder).clone() else {
        emit(&shared, json!({ "type": "sessions", "sessions": [] }));
        return Ok(());
    };
    let mut cursor: Option<String> = None;
    let mut sessions = Vec::new();
    for _ in 0..20 {
        let mut request = ListSessionsRequest::new().cwd(folder.clone());
        if let Some(token) = cursor.clone() {
            request = request.cursor(token);
        }
        let response = connection.send_request(request).block_task().await?;
        for session in response.sessions {
            sessions.push(serde_json::to_value(&session).unwrap_or(Value::Null));
        }
        match response.next_cursor.filter(|token| !token.is_empty()) {
            Some(token) => cursor = Some(token),
            None => break,
        }
    }
    emit(
        &shared,
        json!({ "type": "sessions", "sessions": sessions }),
    );
    Ok(())
}

async fn close_open(connection: &ConnectionTo<Agent>, session_id: &mut Option<String>) {
    let Some(id) = session_id.take() else {
        return;
    };
    let _ = connection
        .send_request(CloseSessionRequest::new(SessionId::new(id)))
        .block_task()
        .await;
}

fn cancel_turn(shared: &Shared, connection: &ConnectionTo<Agent>, session_id: Option<&str>) {
    if let Some(id) = session_id {
        let _ = connection.send_notification(CancelNotification::new(SessionId::new(id)));
    }
    for id in shared.permits.cancel_all() {
        emit(&shared, json!({ "type": "permission-clear", "id": id }));
    }
}

fn emit_session(
    shared: &Shared,
    id: &str,
    folder: &PathBuf,
    warning: Option<&str>,
    options: Option<&Vec<SessionConfigOption>>,
    modes: Option<&SessionModeState>,
) {
    let (mode_id, available) = match modes {
        Some(modes) => (
            Some(modes.current_mode_id.to_string()),
            modes
                .available_modes
                .iter()
                .map(|mode| json!({ "id": mode.id.to_string(), "name": mode.name }))
                .collect::<Vec<_>>(),
        ),
        None => (None, Vec::new()),
    };
    if let Some(mode_id) = &mode_id {
        shared.mode_tx.send_modify(|state| {
            state.0 = state.0.wrapping_add(1);
            state.1 = Some(mode_id.clone());
        });
    }
    emit(
        &shared,
        json!({
            "type": "session",
            "id": id,
            "cwd": folder,
            "warning": warning,
            "modeId": mode_id,
            "modes": available,
            "configOptions": options.and_then(|opts| serde_json::to_value(opts).ok()).unwrap_or(json!([])),
        }),
    );
}

fn emit_usage(shared: &Shared, usage: &TurnUsage) {
    emit(
        &shared,
        json!({
            "type": "usage",
            "used": used_tokens(usage).unwrap_or(0),
            "size": shared.window.load(Ordering::Relaxed),
            "inputTokens": usage.input_tokens,
            "outputTokens": usage.output_tokens,
            "totalTokens": usage.total_tokens,
            "cachedReadTokens": usage.cached_read_tokens,
            "reasoningTokens": usage.reasoning_tokens,
        }),
    );
}

async fn wait_for_mode(
    rx: &mut watch::Receiver<(u64, Option<String>)>,
    generation: u64,
    expected: &str,
) -> bool {
    let wait = async {
        loop {
            let current = rx.borrow().clone();
            if current.0 > generation {
                return current.1.as_deref() == Some(expected);
            }
            drop(current);
            if rx.changed().await.is_err() {
                return false;
            }
        }
    };
    matches!(tokio::time::timeout(MODE_WAIT, wait).await, Ok(true))
}

fn remember_pid(shared: &Shared, session_id: &str) {
    if let Some(entry) = read_active()
        .into_iter()
        .find(|session| session.session_id == session_id)
    {
        *lock(&shared.our_pid) = Some(entry.pid);
    }
}

fn read_active() -> Vec<crate::usage::ActiveSession> {
    let path = grok_dir().join("active_sessions.json");
    let bytes = std::fs::read(path).unwrap_or_default();
    parse_active_sessions(&bytes)
}

fn grok_dir() -> PathBuf {
    if let Ok(profile) = std::env::var("USERPROFILE") {
        return PathBuf::from(profile).join(".grok");
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".grok");
    }
    PathBuf::from(".grok")
}

fn delete_session_cli(id: &str) -> Result<(), String> {
    let output = Command::new("grok")
        .args(["sessions", "delete", id])
        .output()
        .map_err(|error| format!("削除コマンドを起動できませんでした: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        Err(format!("セッションを削除できませんでした。{stderr}{stdout}"))
    }
}

fn slash_kind(text: &str) -> Option<&'static str> {
    match text.trim() {
        "/compact" => Some("compact"),
        "/context" => Some("context"),
        _ => None,
    }
}

fn stop_reason_name(reason: &impl serde::Serialize) -> String {
    serde_json::to_value(reason)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "end_turn".into())
}

fn record(mirror: &Mutex<Mirror>, value: &Value) {
    let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
    let mut slot = lock(mirror);
    match kind {
        "ready" => slot.ready = Some(value.clone()),
        "sessions" => slot.sessions = Some(value.clone()),
        "session" => slot.session = Some(value.clone()),
        "session-closed" => slot.session = None,
        "config" => slot.config = Some(value.clone()),
        "mode" => slot.mode = Some(value.clone()),
        "usage" => slot.usage = Some(value.clone()),
        _ => {}
    }
}

fn publish(app: &AppHandle, value: Value) {
    let _ = app.emit("agent-event", value);
}

fn emit(shared: &Shared, value: Value) {
    if shared.replaying.load(Ordering::SeqCst) {
        let mut buf = lock(&shared.replay_buf);
        if shared.replaying.load(Ordering::SeqCst) {
            record(&shared.mirror, &value);
            buf.push(value);
            if buf.len() >= REPLAY_BATCH {
                let batch = std::mem::take(&mut *buf);
                publish_replay(shared, batch);
            }
            return;
        }
    }
    record(&shared.mirror, &value);
    publish(&shared.app, value);
}

fn publish_replay(shared: &Shared, batch: Vec<Value>) {
    if batch.is_empty() {
        return;
    }
    publish(
        &shared.app,
        json!({ "type": "replay", "events": batch }),
    );
}

fn finish_replay(shared: &Shared) {
    let mut buf = lock(&shared.replay_buf);
    shared.replaying.store(false, Ordering::SeqCst);
    let batch = std::mem::take(&mut *buf);
    publish_replay(shared, batch);
}

fn emit_error(shared: &Shared, message: &str) {
    publish(
        &shared.app,
        json!({
            "type": "error",
            "message": message,
            "auth": is_auth(message),
        }),
    );
}

fn format_acp(error: &impl std::fmt::Display) -> String {
    let text = error.to_string();
    if is_auth(&text) {
        format!("{text} 画面の「ログイン」から grok login を開いてください。")
    } else {
        text
    }
}

fn is_auth(text: &str) -> bool {
    let text = text.to_lowercase();
    text.contains("unauthorized")
        || text.contains("not logged")
        || text.contains("logged in")
        || text.contains("authentication")
        || text.contains("login required")
        || text.contains("please login")
        || text.contains("grok login")
        || text.contains("credentials")
        || text.contains("sign in")
}

fn connection_dead(error: &impl std::fmt::Display) -> bool {
    let text = error.to_string().to_lowercase();
    text.contains("closed")
        || text.contains("eof")
        || text.contains("broken pipe")
        || text.contains("connection reset")
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}
