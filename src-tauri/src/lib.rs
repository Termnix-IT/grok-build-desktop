mod agent;
mod orchestra;
mod permit;
mod usage;

use agent::AgentState;
use orchestra::{LogLine, Orchestra, Preflight, ProbeResult, Registry, TaskRecord};
use std::{path::Path, sync::Arc};
use tauri::{Emitter, Manager, State};

#[tauri::command]
async fn set_folder(state: State<'_, AgentState>, path: String) -> Result<(), String> {
    state.set_folder(path).await
}

#[tauri::command]
async fn new_session(state: State<'_, AgentState>) -> Result<(), String> {
    state.new_session().await
}

#[tauri::command]
async fn load_session(state: State<'_, AgentState>, id: String) -> Result<(), String> {
    state.load_session(id).await
}

#[tauri::command]
async fn close_session(state: State<'_, AgentState>) -> Result<(), String> {
    state.close_session().await
}

#[tauri::command]
async fn refresh_sessions(state: State<'_, AgentState>) -> Result<(), String> {
    state.refresh_sessions().await
}

#[tauri::command]
async fn prompt(
    state: State<'_, AgentState>,
    text: String,
    files: Vec<String>,
    image_ids: Vec<String>,
) -> Result<(), String> {
    state.prompt(text, files, image_ids).await
}

#[tauri::command]
fn stage_paths(state: State<'_, AgentState>, paths: Vec<String>) -> Vec<agent::AttachmentRecord> {
    state.stage_paths(paths)
}

#[tauri::command]
fn stage_bytes(
    state: State<'_, AgentState>,
    name: String,
    data_base64: String,
) -> agent::AttachmentRecord {
    state.stage_bytes(name, data_base64)
}

#[tauri::command]
fn unstage(state: State<'_, AgentState>, id: String) {
    state.unstage(id);
}

#[tauri::command]
async fn compact_session(state: State<'_, AgentState>) -> Result<(), String> {
    state.compact().await
}

#[tauri::command]
async fn cancel_turn(state: State<'_, AgentState>) -> Result<(), String> {
    state.cancel().await
}

#[tauri::command]
async fn set_mode(state: State<'_, AgentState>, mode_id: String) -> Result<(), String> {
    state.set_mode(mode_id).await
}

#[tauri::command]
async fn set_config(state: State<'_, AgentState>, id: String, value: String) -> Result<(), String> {
    state.set_config(id, value).await
}

#[tauri::command]
async fn decide_permission(
    state: State<'_, AgentState>,
    id: u64,
    option_id: Option<String>,
) -> Result<(), String> {
    state.decide_permission(id, option_id).await
}

#[tauri::command]
fn set_auto_approve(state: State<'_, AgentState>, enabled: bool) {
    state.set_auto_approve(enabled);
}

#[tauri::command]
async fn delete_session(state: State<'_, AgentState>, id: String) -> Result<(), String> {
    state.delete_session(id).await
}

#[tauri::command]
fn login() -> Result<(), String> {
    agent::spawn_login()
}

#[tauri::command]
fn search_files(
    state: State<'_, AgentState>,
    query: String,
    project: Option<String>,
) -> Result<Vec<String>, String> {
    let folder = project
        .filter(|path| !path.trim().is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| state.folder());
    let Some(folder) = folder else {
        return Err("先にプロジェクトフォルダを選んでください".into());
    };
    Ok(usage::search_files(&folder, query.trim(), 50))
}

#[tauri::command]
fn session_owner(state: State<'_, AgentState>, id: String) -> Option<String> {
    agent::owner_warning(&id, state.our_pid())
}

#[tauri::command]
fn app_snapshot(state: State<'_, AgentState>) -> serde_json::Value {
    state.snapshot()
}

#[tauri::command]
fn agents_config_get(state: State<'_, Orchestra>) -> Registry {
    state.registry()
}

#[tauri::command]
fn agents_config_save(state: State<'_, Orchestra>, registry: Registry) -> Result<Registry, String> {
    state.save_registry(registry)
}

#[tauri::command]
fn agents_config_reset(state: State<'_, Orchestra>) -> Result<Registry, String> {
    state.reset_registry()
}

#[tauri::command]
async fn agents_probe(app: tauri::AppHandle, id: String) -> ProbeResult {
    let state = app.state::<Orchestra>().inner().clone();
    state.probe(&id).await
}

#[tauri::command]
async fn orchestrator_preflight(app: tauri::AppHandle, project: String) -> Preflight {
    let state = app.state::<Orchestra>().inner().clone();
    state.preflight(Path::new(&project)).await
}

#[tauri::command]
async fn orchestrator_git_init(
    app: tauri::AppHandle,
    project: String,
) -> Result<Preflight, String> {
    let state = app.state::<Orchestra>().inner().clone();
    state.git_init(Path::new(&project)).await
}

#[derive(serde::Serialize)]
struct RestorePreview {
    files: Vec<orchestra::git::RestoreFile>,
}

#[tauri::command]
async fn orchestrator_restore_preview(
    app: tauri::AppHandle,
    job_id: String,
) -> Result<RestorePreview, orchestra::engine::HubError> {
    let state = app.state::<Orchestra>().inner().clone();
    let files = state.restore_preview(&job_id).await?;
    Ok(RestorePreview { files })
}

#[tauri::command]
async fn orchestrator_restore(
    app: tauri::AppHandle,
    job_id: String,
) -> Result<RestorePreview, orchestra::engine::HubError> {
    let state = app.state::<Orchestra>().inner().clone();
    let files = state.restore(&job_id).await?;
    Ok(RestorePreview { files })
}

#[tauri::command]
async fn orchestrator_dispatch(
    app: tauri::AppHandle,
    project: String,
    agent_id: String,
    prompt: String,
    task_id: Option<String>,
    title: Option<String>,
) -> Result<orchestra::engine::Submitted, String> {
    let state = app.state::<Orchestra>().inner().clone();
    let job = orchestra::engine::JobSpec {
        project,
        agent: agent_id,
        prompt,
        title,
        task_id,
        timeout_secs: None,
    };
    state
        .submit(job, orchestra::engine::Source::Manual)
        .await
        .map_err(|error| error.message)
}

#[tauri::command]
async fn orchestrator_kill(app: tauri::AppHandle, job_id: String) -> Result<(), String> {
    let state = app.state::<Orchestra>().inner().clone();
    state.kill(&job_id).await.map_err(|error| error.message)
}

#[tauri::command]
async fn orchestrator_cancel(app: tauri::AppHandle, job_id: String) -> Result<(), String> {
    let state = app.state::<Orchestra>().inner().clone();
    state
        .cancel(&job_id, orchestra::engine::Source::Manual)
        .await
        .map(|_| ())
        .map_err(|error| error.message)
}

#[tauri::command]
async fn orchestrator_stop_all(app: tauri::AppHandle) -> Result<(), String> {
    let state = app.state::<Orchestra>().inner().clone();
    state.stop_all().await.map_err(|error| error.message)
}

#[tauri::command]
fn orchestrator_resume(state: State<'_, Orchestra>, task_id: String) -> Result<(), String> {
    state.resume(&task_id).map_err(|error| error.message)
}

#[tauri::command]
async fn orchestrator_close(app: tauri::AppHandle, task_id: String) -> Result<(), String> {
    let state = app.state::<Orchestra>().inner().clone();
    state.close(&task_id).await.map_err(|error| error.message)
}

#[tauri::command]
fn orchestrator_resume_project(state: State<'_, Orchestra>, project: String) -> Result<(), String> {
    state
        .resume_project(&project)
        .map_err(|error| error.message)
}

#[tauri::command]
fn orchestrator_tasks(state: State<'_, Orchestra>, project: String) -> Vec<TaskRecord> {
    state.tasks(Path::new(&project))
}

#[tauri::command]
fn orchestrator_projects(state: State<'_, Orchestra>) -> Vec<String> {
    state.projects()
}

#[tauri::command]
fn orchestrator_normalize(state: State<'_, Orchestra>, project: String) -> Result<String, String> {
    state.normalize(&project)
}

#[tauri::command]
fn gateway_add_project(
    state: State<'_, Orchestra>,
    project: String,
) -> Result<serde_json::Value, String> {
    state.gateway_add_project(&project)
}

#[tauri::command]
fn orchestrator_task(state: State<'_, Orchestra>, task_id: String) -> Option<TaskRecord> {
    state.task(&task_id)
}

#[tauri::command]
fn orchestrator_delete(state: State<'_, Orchestra>, task_id: String) -> Result<(), String> {
    state.delete(&task_id)
}

#[tauri::command]
fn orchestrator_snapshot(state: State<'_, Orchestra>) -> serde_json::Value {
    state.snapshot()
}

#[tauri::command]
fn agent_log(state: State<'_, Orchestra>, node_id: String) -> Vec<LogLine> {
    state.logs(&node_id)
}

#[tauri::command]
fn agent_brief(state: State<'_, Orchestra>, node_id: String) -> Vec<LogLine> {
    state.brief(&node_id)
}

#[tauri::command]
fn gateway_get(state: State<'_, Orchestra>) -> serde_json::Value {
    state.gateway_view()
}

#[tauri::command]
fn gateway_save(
    state: State<'_, Orchestra>,
    accepting: bool,
    allowed_projects: Vec<String>,
) -> Result<serde_json::Value, String> {
    state.gateway_save(accepting, allowed_projects)
}

/// Runs `gbd version` against the live pipe, the same way Grok Bot will.
#[tauri::command]
async fn gateway_selftest() -> Result<serde_json::Value, String> {
    tauri::async_runtime::spawn_blocking(orchestra::cli::selftest)
        .await
        .map_err(|error| error.to_string())
}

/// Entry point of the `gbd` console binary.
pub fn gbd_main() -> i32 {
    orchestra::cli::main()
}

/// The Grok chat screen was removed in favour of 連携. Turning this on starts the ACP host again.
const CHAT_ENABLED: bool = false;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            // The screen only shows 連携 now. The ACP host stays in the code but does not start
            // a Grok chat process.
            let state = if CHAT_ENABLED {
                AgentState::start(app.handle().clone())
            } else {
                AgentState::disabled()
            };
            app.manage(state);
            let config_dir = app.path().app_config_dir()?;
            let orchestra = Orchestra::new(
                config_dir.join("agents.json"),
                config_dir.join("gateway.json"),
                app.path().app_data_dir()?.join("tasks"),
                Arc::new(orchestra::TauriSink(app.handle().clone())),
            );
            let hub = orchestra.clone();
            app.manage(orchestra);
            tauri::async_runtime::spawn(async move {
                match orchestra::ipc::pipe_name() {
                    Ok(pipe) => {
                        let _ = orchestra::ipc::serve(hub, &pipe).await;
                    }
                    Err(error) => {
                        hub.set_gateway_status(orchestra::engine::GatewayStatus::Failed {
                            error: error.message,
                        })
                    }
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            set_folder,
            new_session,
            load_session,
            close_session,
            refresh_sessions,
            prompt,
            stage_paths,
            stage_bytes,
            unstage,
            compact_session,
            cancel_turn,
            set_mode,
            set_config,
            decide_permission,
            set_auto_approve,
            delete_session,
            login,
            search_files,
            session_owner,
            app_snapshot,
            agents_config_get,
            agents_config_save,
            agents_config_reset,
            agents_probe,
            orchestrator_preflight,
            orchestrator_git_init,
            orchestrator_restore_preview,
            orchestrator_restore,
            orchestrator_dispatch,
            orchestrator_kill,
            orchestrator_stop_all,
            orchestrator_cancel,
            orchestrator_resume,
            orchestrator_close,
            orchestrator_resume_project,
            orchestrator_tasks,
            orchestrator_task,
            orchestrator_projects,
            orchestrator_normalize,
            gateway_add_project,
            orchestrator_delete,
            orchestrator_snapshot,
            agent_log,
            agent_brief,
            gateway_get,
            gateway_save,
            gateway_selftest
        ])
        .build(tauri::generate_context!())
        .expect("error while running tauri application")
        .run(|app, event| {
            if matches!(event, tauri::RunEvent::Exit) {
                if let Some(state) = app.try_state::<AgentState>() {
                    state.shutdown();
                }
                if let Some(state) = app.try_state::<Orchestra>() {
                    state.shutdown();
                }
                let _ = app.emit("agent-event", serde_json::json!({ "type": "shutdown" }));
            }
        });
}
