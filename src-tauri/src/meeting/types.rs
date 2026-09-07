use serde::{Deserialize, Serialize};
use specta::Type;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum AudioSource {
    Microphone,
    System,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum MeetingStatus {
    Recording,
    Processing,
    Done,
}

#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct MeetingUtterance {
    pub id: i64,
    pub meeting_id: i64,
    pub speaker_id: String,
    pub speaker_name: String,
    pub source: AudioSource,
    pub start_ms: i64,
    pub end_ms: i64,
    pub text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct MeetingSpeaker {
    pub speaker_id: String,
    pub display_name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, Type, Default)]
pub struct MeetingNotes {
    pub summary: String,
    pub action_items: Vec<String>,
    pub decisions: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct MeetingRecord {
    pub id: i64,
    pub title: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub your_name: String,
    pub status: MeetingStatus,
    pub notes: MeetingNotes,
    pub speakers: Vec<MeetingSpeaker>,
    pub utterances: Vec<MeetingUtterance>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct MeetingListItem {
    pub id: i64,
    pub title: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub status: MeetingStatus,
    pub utterance_count: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, Type, tauri_specta::Event)]
pub struct MeetingUtteranceEvent {
    pub utterance: MeetingUtterance,
}

#[derive(Clone, Debug, Serialize, Deserialize, Type, tauri_specta::Event)]
pub struct MeetingStateEvent {
    pub meeting: MeetingRecord,
}

#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct MeetingStartOptions {
    pub title: Option<String>,
    pub your_name: Option<String>,
    pub system_audio_device: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct SystemAudioDevice {
    pub name: String,
    pub is_default: bool,
}
