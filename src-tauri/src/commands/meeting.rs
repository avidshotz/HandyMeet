use crate::managers::meeting::MeetingManager;
use crate::meeting::types::{
    MeetingListItem, MeetingRecord, MeetingStartOptions, SystemAudioDevice,
};
use std::sync::Arc;
use tauri::{AppHandle, Manager};
use tauri_plugin_dialog::DialogExt;

#[tauri::command]
#[specta::specta]
pub fn start_meeting(
    app: AppHandle,
    options: MeetingStartOptions,
) -> Result<MeetingRecord, String> {
    meeting_manager(&app)?.start_meeting(options)
}

#[tauri::command]
#[specta::specta]
pub fn stop_meeting(app: AppHandle) -> Result<MeetingRecord, String> {
    meeting_manager(&app)?.stop_meeting()
}

#[tauri::command]
#[specta::specta]
pub fn list_meetings(app: AppHandle) -> Result<Vec<MeetingListItem>, String> {
    meeting_manager(&app)?.list_meetings()
}

#[tauri::command]
#[specta::specta]
pub fn get_meeting(app: AppHandle, id: i64) -> Result<MeetingRecord, String> {
    meeting_manager(&app)?.get_meeting(id)
}

#[tauri::command]
#[specta::specta]
pub fn rename_meeting_speaker(
    app: AppHandle,
    meeting_id: i64,
    speaker_id: String,
    display_name: String,
) -> Result<MeetingRecord, String> {
    meeting_manager(&app)?.rename_speaker(meeting_id, speaker_id, display_name)
}

#[tauri::command]
#[specta::specta]
pub fn delete_meeting(app: AppHandle, id: i64) -> Result<(), String> {
    meeting_manager(&app)?.delete_meeting(id)
}

#[tauri::command]
#[specta::specta]
pub fn export_meeting_markdown(app: AppHandle, id: i64) -> Result<String, String> {
    meeting_manager(&app)?.export_markdown(id)
}

#[tauri::command]
#[specta::specta]
pub fn save_meeting_markdown(app: AppHandle, id: i64) -> Result<Option<String>, String> {
    let markdown = meeting_manager(&app)?.export_markdown(id)?;
    let meeting = meeting_manager(&app)?.get_meeting(id)?;
    let suggested = format!("{}.md", sanitize_filename(&meeting.title));
    let path = app
        .dialog()
        .file()
        .set_file_name(&suggested)
        .add_filter("Markdown", &["md"])
        .blocking_save_file();
    let Some(file) = path else {
        return Ok(None);
    };
    let path = file.into_path().map_err(|e| e.to_string())?;
    std::fs::write(&path, markdown).map_err(|e| format!("Failed to write file: {e}"))?;
    Ok(Some(path.to_string_lossy().to_string()))
}

#[tauri::command]
#[specta::specta]
pub fn list_system_audio_devices(app: AppHandle) -> Result<Vec<SystemAudioDevice>, String> {
    meeting_manager(&app)?.list_system_audio_devices()
}

#[tauri::command]
#[specta::specta]
pub fn get_meeting_your_name(app: AppHandle) -> Result<String, String> {
    Ok(meeting_manager(&app)?.default_your_name())
}

#[tauri::command]
#[specta::specta]
pub fn set_meeting_your_name(app: AppHandle, name: String) -> Result<(), String> {
    meeting_manager(&app)?.set_default_your_name(&name)
}

#[tauri::command]
#[specta::specta]
pub fn is_meeting_recording(app: AppHandle) -> Result<bool, String> {
    Ok(meeting_manager(&app)?.is_recording())
}

#[tauri::command]
#[specta::specta]
pub fn get_active_meeting(app: AppHandle) -> Result<Option<MeetingRecord>, String> {
    let manager = meeting_manager(&app)?;
    match manager.active_meeting_id() {
        Some(id) => manager.get_meeting(id).map(Some),
        None => Ok(None),
    }
}

fn meeting_manager(app: &AppHandle) -> Result<Arc<MeetingManager>, String> {
    app.try_state::<Arc<MeetingManager>>()
        .map(|s| s.inner().clone())
        .ok_or_else(|| "Meeting manager is not initialized".to_string())
}

fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '-',
            c if c.is_control() => '-',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.');
    if trimmed.is_empty() {
        "meeting".to_string()
    } else {
        trimmed.chars().take(80).collect()
    }
}
