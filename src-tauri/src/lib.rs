mod agent;
mod orchestra;
mod permit;
mod usage;

use agent::AgentState;
use orchestra::{LogLine, Orchestra, Preflight, ProbeResult, Registry, TaskRecord};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
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
fn stage_paths(
    state: State<'_, AgentState>,
    paths: Vec<String>,
) -> Vec<agent::AttachmentRecord> {
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
async fn set_config(
    state: State<'_, AgentState>,
    id: String,
    value: String,
) -> Result<(), String> {
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
fn search_files(state: State<'_, AgentState>, query: String) -> Result<Vec<String>, String> {
    let folder = state.folder();
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
async fn orchestrator_start(state: State<'_, Orchestra>, project: String, goal: String) -> Result<String, String> {
    state.start(PathBuf::from(project), goal).await
}

#[tauri::command]
fn orchestrator_answer(state: State<'_, Orchestra>, task_id: String, text: String) -> Result<(), String> {
    state.answer(&task_id, text)
}

#[tauri::command]
fn orchestrator_abort(state: State<'_, Orchestra>, task_id: String) -> Result<(), String> {
    state.abort(&task_id)
}

#[tauri::command]
fn orchestrator_tasks(state: State<'_, Orchestra>, project: String) -> Vec<TaskRecord> {
    state.tasks(Path::new(&project))
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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            let state = AgentState::start(app.handle().clone());
            app.manage(state);
            let orchestra = Orchestra::new(
                app.path().app_config_dir()?.join("agents.json"),
                app.path().app_data_dir()?.join("tasks"),
                Arc::new(orchestra::TauriSink(app.handle().clone())),
                Arc::new(orchestra::GrokPlanner),
            );
            app.manage(orchestra);
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
            orchestrator_start,
            orchestrator_answer,
            orchestrator_abort,
            orchestrator_tasks,
            orchestrator_task,
            orchestrator_delete,
            orchestrator_snapshot,
            agent_log
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
