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

// `async` matters here: without it Tauri runs this on the main thread, and `blocking_save_file`
// then waits for a dialog that only the main thread can show — the app beach-balls forever with
// the Save panel stuck. Marking the command async moves it to a worker thread, so the main thread
// stays free to run the panel.
#[tauri::command(async)]
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

/// Whether meetings cluster distinct voices (Speaker 1/2/3...) on both the
/// mic and system audio using a small on-device ML model, instead of just
/// the lightweight always-on heuristic for system audio.
#[tauri::command]
#[specta::specta]
pub fn get_meeting_speaker_id_enabled(app: AppHandle) -> Result<bool, String> {
    Ok(crate::settings::get_settings(&app).meeting_speaker_id_enabled)
}

#[tauri::command]
#[specta::specta]
pub fn set_meeting_speaker_id_enabled(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = crate::settings::get_settings(&app);
    settings.meeting_speaker_id_enabled = enabled;
    crate::settings::write_settings(&app, settings);

    if enabled {
        // Fire-and-forget: get the ~29MB model in place now so it's ready by
        // the time the user actually starts a meeting. Never blocks this
        // call, and a meeting started before it finishes just uses the
        // heuristic for that session.
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            if let Err(err) = crate::meeting::speaker_id::ensure_model_downloaded(&app).await {
                log::warn!("Speaker-ID model download failed: {err}");
            }
        });
    }
    Ok(())
}

/// Whether the speaker-ID model has finished downloading (so the frontend
/// can show "downloading..." vs "ready" without polling the filesystem).
#[tauri::command]
#[specta::specta]
pub fn is_meeting_speaker_id_model_ready(app: AppHandle) -> Result<bool, String> {
    Ok(crate::meeting::speaker_id::is_model_ready(&app))
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
