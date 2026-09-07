use super::notes;
use super::types::MeetingRecord;
use anyhow::Result;
use log::{info, warn};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

const AVIDDESK_STASH: &str = "http://127.0.0.1:3970/api/settings/stash/public";
const HEADSECRETARY_SCAN: &str = "http://127.0.0.1:8787/stash/scan";

/// Write a NoteCatcher-shaped markdown file into `unprocessed/` and ask
/// Head Secretary to rescan. Summary / action items are left empty on purpose.
pub fn dump_to_inbox(meeting: &MeetingRecord) -> Result<PathBuf> {
    let markdown = notes::to_markdown(
        &meeting.title,
        &meeting.your_name,
        meeting.started_at,
        &meeting.utterances,
        &meeting
            .speakers
            .iter()
            .map(|s| s.display_name.clone())
            .collect::<Vec<_>>(),
    );
    let inbox = resolve_unprocessed_dir()?;
    fs::create_dir_all(&inbox)?;
    let filename = notes::stash_filename(meeting.started_at, &meeting.title);
    let path = unique_path(&inbox.join(filename));
    fs::write(&path, markdown)?;
    write_sidecar(&path, meeting)?;
    info!("Wrote meeting transcript to {}", path.display());
    ping_headsecretary_scan();
    Ok(path)
}

fn unique_path(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("Meeting");
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    for i in 2..100 {
        let candidate = parent.join(format!("{stem}-{i}.md"));
        if !candidate.exists() {
            return candidate;
        }
    }
    parent.join(format!("{stem}-{}.md", chrono::Utc::now().timestamp()))
}

fn write_sidecar(md_path: &Path, meeting: &MeetingRecord) -> Result<()> {
    let sidecar = PathBuf::from(format!("{}.meta.json", md_path.display()));
    let payload = json!({
        "tags": ["source:handymeet"],
        "topics": [],
        "clients": [],
        "tagged_by": "HandyMeet",
        "source": "",
        "speakers": meeting.speakers.iter().map(|s| json!({
            "id": s.speaker_id,
            "name": s.display_name,
        })).collect::<Vec<_>>(),
    });
    fs::write(sidecar, serde_json::to_string_pretty(&payload)?)?;
    Ok(())
}

fn resolve_unprocessed_dir() -> Result<PathBuf> {
    if let Some(root) = fetch_json_path(AVIDDESK_STASH, &["effective_path", "output_dir"]) {
        return Ok(root.join("unprocessed"));
    }
    let home = dirs_path();
    let candidates = [
        home.join("Documents/programming/AvidDesk/data/settings.json"),
        home.join("Documents/programming/AvidDesk/config/stash.path.txt"),
        home.join(".transcript_watcher/config.json"),
    ];
    for candidate in candidates {
        if let Some(root) = read_stash_root(&candidate) {
            return Ok(root.join("unprocessed"));
        }
    }
    let fallback = home
        .join("Documents/programming/AvidDesk/apps/AVSZHSNoteCatcher/Transcripts/unprocessed");
    Ok(fallback)
}

fn dirs_path() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn read_stash_root(path: &Path) -> Option<PathBuf> {
    let raw = fs::read_to_string(path).ok()?;
    if path.extension().and_then(|e| e.to_str()) == Some("txt") {
        let line = raw.lines().next()?.trim();
        if line.is_empty() {
            return None;
        }
        return Some(PathBuf::from(line));
    }
    let value: Value = serde_json::from_str(&raw).ok()?;
    if let Some(dir) = value
        .pointer("/stash/output_dir")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        return Some(PathBuf::from(dir));
    }
    value
        .get("output_dir")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

fn fetch_json_path(url: &str, keys: &[&str]) -> Option<PathBuf> {
    let body = tauri::async_runtime::block_on(async {
        reqwest::Client::new()
            .get(url)
            .timeout(Duration::from_millis(1500))
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?
            .json::<Value>()
            .await
            .ok()
    })?;
    for key in keys {
        if let Some(path) = body.get(*key).and_then(|v| v.as_str()) {
            if !path.trim().is_empty() {
                return Some(PathBuf::from(path));
            }
        }
    }
    None
}

fn ping_headsecretary_scan() {
    let result = tauri::async_runtime::block_on(async {
        reqwest::Client::new()
            .post(HEADSECRETARY_SCAN)
            .timeout(Duration::from_millis(2000))
            .send()
            .await
    });
    match result {
        Ok(response) if response.status().is_success() => {
            info!("Asked Head Secretary to rescan stash");
        }
        Ok(response) => warn!(
            "Head Secretary stash scan returned {}",
            response.status()
        ),
        Err(err) => warn!("Head Secretary not reachable for stash scan: {err}"),
    }
}

pub fn markdown_for(meeting: &MeetingRecord) -> String {
    notes::to_markdown(
        &meeting.title,
        &meeting.your_name,
        meeting.started_at,
        &meeting.utterances,
        &meeting
            .speakers
            .iter()
            .map(|s| s.display_name.clone())
            .collect::<Vec<_>>(),
    )
}
