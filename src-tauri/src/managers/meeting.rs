use crate::meeting::capture::{self, CaptureHandle};
use crate::meeting::diarize::{self, SpeakerTracker};
use crate::meeting::notes;
use crate::meeting::speaker_id::{self, SpeakerEmbedder};
use crate::meeting::store::MeetingStore;
use crate::meeting::types::{
    AudioSource, MeetingListItem, MeetingRecord, MeetingStartOptions, MeetingStatus,
    MeetingUtterance, MeetingUtteranceEvent, SystemAudioDevice,
};
use crate::managers::audio::AudioRecordingManager;
use crate::managers::transcription::TranscriptionManager;
use crate::settings::get_settings;
use anyhow::Result;
use chrono::Local;
use log::{info, warn};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Manager};
use tauri_specta::Event;

const FRAME_SAMPLES: usize = 480; // 30 ms at 16 kHz
const MIN_SPEECH_FRAMES: usize = 20; // 600 ms
const MAX_SPEECH_FRAMES: usize = 400; // 12 s
const HANGOVER_FRAMES: usize = 10;
const ENERGY_THRESHOLD: f32 = 0.012;

struct SourceVad {
    speaking: bool,
    hangover: usize,
    buffer: Vec<f32>,
    start_ms: i64,
}

impl SourceVad {
    fn new() -> Self {
        Self {
            speaking: false,
            hangover: 0,
            buffer: Vec::new(),
            start_ms: 0,
        }
    }
}

struct TranscribeJob {
    source: AudioSource,
    speaker_id: String,
    speaker_name: String,
    start_ms: i64,
    end_ms: i64,
    audio: Vec<f32>,
}

struct LiveSession {
    meeting_id: i64,
    stop: Arc<AtomicBool>,
    capture: Option<CaptureHandle>,
    worker: Option<thread::JoinHandle<()>>,
}

pub struct MeetingManager {
    app_handle: AppHandle,
    store: Mutex<MeetingStore>,
    session: Mutex<Option<LiveSession>>,
    speaker_embedder: Mutex<Option<Arc<SpeakerEmbedder>>>,
}

impl MeetingManager {
    pub fn new(app_handle: &AppHandle) -> Result<Self> {
        let app_data_dir = crate::portable::app_data_dir(app_handle)?;
        Ok(Self {
            app_handle: app_handle.clone(),
            store: Mutex::new(MeetingStore::new(app_data_dir)?),
            session: Mutex::new(None),
            speaker_embedder: Mutex::new(None),
        })
    }

    /// Returns the cached ML speaker-embedding backend if the setting is on
    /// and the model has already been downloaded; otherwise `None`, in which
    /// case callers fall back to the lightweight heuristic. Never blocks on
    /// a download — if the model isn't there yet, kicks one off in the
    /// background (fire-and-forget) so it's ready for a future meeting.
    fn speaker_embedder_if_ready(&self) -> Option<Arc<SpeakerEmbedder>> {
        if !get_settings(&self.app_handle).meeting_speaker_id_enabled {
            return None;
        }

        if let Some(embedder) = self.speaker_embedder.lock().unwrap().as_ref() {
            return Some(embedder.clone());
        }

        if !speaker_id::is_model_ready(&self.app_handle) {
            let app = self.app_handle.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(err) = speaker_id::ensure_model_downloaded(&app).await {
                    warn!("Speaker-ID model download failed: {err}");
                }
            });
            return None;
        }

        let path = speaker_id::model_path(&self.app_handle).ok()?;
        match SpeakerEmbedder::load(&path) {
            Ok(embedder) => {
                let embedder = Arc::new(embedder);
                *self.speaker_embedder.lock().unwrap() = Some(embedder.clone());
                Some(embedder)
            }
            Err(err) => {
                warn!("Failed to load speaker-ID model: {err}");
                None
            }
        }
    }

    pub fn is_recording(&self) -> bool {
        self.session.lock().unwrap().is_some()
    }

    pub fn meeting_is_recording(app: &AppHandle) -> bool {
        app.try_state::<Arc<MeetingManager>>()
            .is_some_and(|manager| manager.is_recording())
    }

    pub fn default_your_name(&self) -> String {
        self.store
            .lock()
            .unwrap()
            .get_kv("your_name")
            .ok()
            .flatten()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "You".to_string())
    }

    pub fn set_default_your_name(&self, name: &str) -> Result<(), String> {
        self.store
            .lock()
            .unwrap()
            .set_kv("your_name", name)
            .map_err(|e| e.to_string())
    }

    pub fn start_meeting(&self, options: MeetingStartOptions) -> Result<MeetingRecord, String> {
        if self
            .app_handle
            .try_state::<Arc<AudioRecordingManager>>()
            .is_some_and(|audio| audio.is_recording())
            || self
                .app_handle
                .try_state::<Arc<TranscriptionManager>>()
                .is_some_and(|tm| tm.is_streaming())
        {
            return Err("Stop dictation before starting a meeting".to_string());
        }

        let mut session_guard = self.session.lock().unwrap();
        if session_guard.is_some() {
            return Err("A meeting is already being recorded".to_string());
        }

        let your_name = options
            .your_name
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| self.default_your_name());
        let _ = self.store.lock().unwrap().set_kv("your_name", &your_name);

        let title = options.title.filter(|s| !s.trim().is_empty()).unwrap_or_else(|| {
            format!("Meeting {}", Local::now().format("%b %-d, %-I:%M %p"))
        });
        let started_at = now_ms();

        let settings = get_settings(&self.app_handle);
        if let Some(tm) = self.app_handle.try_state::<Arc<TranscriptionManager>>() {
            tm.initiate_model_load();
        }
        let (frame_tx, frame_rx) = mpsc::channel();
        // Microphones first, the row second. Capture is the step that actually fails — no
        // microphone permission, a device that has gone away, no loopback for system audio — and
        // when the row was written first, every one of those failures left a meeting stranded in
        // "recording" with nothing in it, for ever. Nothing is written down until there is
        // something to record.
        let capture = capture::start_dual_capture(
            settings.selected_microphone.clone(),
            settings.selected_channel,
            options.system_audio_device,
            frame_tx,
        )
        .map_err(|e| e.to_string())?;

        let meeting_id = match self
            .store
            .lock()
            .unwrap()
            .create_meeting(&title, &your_name, started_at)
        {
            Ok(id) => id,
            Err(err) => {
                // Do not leave the microphone live for a meeting that does not exist.
                capture.stop();
                return Err(err.to_string());
            }
        };

        let stop = Arc::new(AtomicBool::new(false));
        let app = self.app_handle.clone();
        let your_name_clone = your_name.clone();
        let worker_stop = Arc::clone(&stop);
        let speaker_id_enabled = settings.meeting_speaker_id_enabled;
        let embedder = self.speaker_embedder_if_ready();
        let worker = thread::Builder::new()
            .name("meeting-session".into())
            .spawn(move || {
                run_session(
                    app,
                    meeting_id,
                    your_name_clone,
                    started_at,
                    frame_rx,
                    worker_stop,
                    speaker_id_enabled,
                    embedder,
                );
            })
            .map_err(|e| e.to_string())?;

        *session_guard = Some(LiveSession {
            meeting_id,
            stop,
            capture: Some(capture),
            worker: Some(worker),
        });
        drop(session_guard);

        info!("Started meeting {meeting_id}");
        self.get_meeting(meeting_id)
    }

    pub fn stop_meeting(&self) -> Result<MeetingRecord, String> {
        let mut session_guard = self.session.lock().unwrap();
        let mut session = session_guard
            .take()
            .ok_or_else(|| "No meeting is being recorded".to_string())?;
        let meeting_id = session.meeting_id;
        session.stop.store(true, Ordering::Relaxed);
        if let Some(capture) = session.capture.take() {
            capture.stop();
        }
        drop(session_guard);
        if let Some(worker) = session.worker.take() {
            let _ = worker.join();
        }
        info!("Stopped meeting {meeting_id}");
        self.get_meeting(meeting_id)
    }

    pub fn list_meetings(&self) -> Result<Vec<MeetingListItem>, String> {
        self.store
            .lock()
            .unwrap()
            .list_meetings()
            .map_err(|e| e.to_string())
    }

    pub fn get_meeting(&self, id: i64) -> Result<MeetingRecord, String> {
        self.store
            .lock()
            .unwrap()
            .get_meeting(id)
            .map_err(|e| e.to_string())
    }

    pub fn rename_speaker(
        &self,
        meeting_id: i64,
        speaker_id: String,
        display_name: String,
    ) -> Result<MeetingRecord, String> {
        if display_name.trim().is_empty() {
            return Err("Speaker name cannot be empty".to_string());
        }
        {
            let store = self.store.lock().unwrap();
            store
                .rename_speaker(meeting_id, &speaker_id, display_name.trim())
                .map_err(|e| e.to_string())?;
            if speaker_id == "you" {
                let _ = store.set_kv("your_name", display_name.trim());
            }
        }
        self.get_meeting(meeting_id)
    }

    pub fn delete_meeting(&self, id: i64) -> Result<(), String> {
        if let Some(session) = self.session.lock().unwrap().as_ref() {
            if session.meeting_id == id {
                return Err("Stop the meeting before deleting it".to_string());
            }
        }
        self.store
            .lock()
            .unwrap()
            .delete_meeting(id)
            .map_err(|e| e.to_string())
    }

    pub fn export_markdown(&self, id: i64) -> Result<String, String> {
        let meeting = self.get_meeting(id)?;
        Ok(crate::meeting::stash::markdown_for(&meeting))
    }

    pub fn list_system_audio_devices(&self) -> Result<Vec<SystemAudioDevice>, String> {
        let mut devices = vec![SystemAudioDevice {
            name: "auto".to_string(),
            is_default: true,
        }];
        match capture::list_system_audio_candidates() {
            Ok(found) => {
                for device in found {
                    devices.push(SystemAudioDevice {
                        name: device.name,
                        is_default: false,
                    });
                }
            }
            Err(err) => warn!("Could not list loopback candidates: {err}"),
        }
        Ok(devices)
    }

    pub fn active_meeting_id(&self) -> Option<i64> {
        self.session.lock().unwrap().as_ref().map(|s| s.meeting_id)
    }
}

fn run_session(
    app: AppHandle,
    meeting_id: i64,
    your_name: String,
    started_at: i64,
    frame_rx: mpsc::Receiver<(AudioSource, Vec<f32>)>,
    stop: Arc<AtomicBool>,
    speaker_id_enabled: bool,
    embedder: Option<Arc<SpeakerEmbedder>>,
) {
    let Some(manager) = app.try_state::<Arc<MeetingManager>>() else {
        warn!("Meeting manager missing in session thread");
        return;
    };

    let _ = manager
        .store
        .lock()
        .unwrap()
        .set_status(meeting_id, MeetingStatus::Recording);

    let (job_tx, job_rx) = mpsc::channel::<TranscribeJob>();
    let transcribe_stop = Arc::clone(&stop);
    let transcribe_app = app.clone();
    let transcribe_your_name = your_name.clone();
    let transcribe_thread = thread::Builder::new()
        .name("meeting-transcribe".into())
        .spawn(move || {
            transcribe_loop(
                transcribe_app,
                meeting_id,
                transcribe_your_name,
                job_rx,
                transcribe_stop,
            )
        })
        .ok();

    let mut mic_vad = SourceVad::new();
    let mut sys_vad = SourceVad::new();
    // System audio has always been clustered into distinct speakers; that
    // stays on unconditionally (now optionally ML-backed). The mic only gets
    // its own tracker (mic could have multiple in-person voices) when the
    // setting is on — off by default reproduces the old "mic is always you"
    // behavior exactly.
    let (mut mic_tracker, mut sys_tracker) = SpeakerTracker::pair(embedder);
    let origin = Instant::now();

    while !stop.load(Ordering::Relaxed) {
        match frame_rx.recv_timeout(Duration::from_millis(50)) {
            Ok((source, frame)) => {
                let elapsed = origin.elapsed().as_millis() as i64;
                match source {
                    AudioSource::Microphone => push_vad(
                        &mut mic_vad,
                        &frame,
                        elapsed,
                        AudioSource::Microphone,
                        "you",
                        &your_name,
                        speaker_id_enabled.then_some(&mut mic_tracker),
                        &job_tx,
                    ),
                    AudioSource::System => push_vad(
                        &mut sys_vad,
                        &frame,
                        elapsed,
                        AudioSource::System,
                        "spk_1",
                        "Speaker",
                        Some(&mut sys_tracker),
                        &job_tx,
                    ),
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    flush_vad(
        &mut mic_vad,
        origin.elapsed().as_millis() as i64,
        AudioSource::Microphone,
        "you",
        &your_name,
        speaker_id_enabled.then_some(&mut mic_tracker),
        &job_tx,
    );
    flush_vad(
        &mut sys_vad,
        origin.elapsed().as_millis() as i64,
        AudioSource::System,
        "spk_1",
        "Speaker",
        Some(&mut sys_tracker),
        &job_tx,
    );
    drop(job_tx);
    if let Some(handle) = transcribe_thread {
        let _ = handle.join();
    }

    // Keep each voice's print alongside the meeting. Without this the clustering is thrown away
    // when the meeting ends and a voice could never be recognised in a later one. It is stored
    // separately from display_name, so renaming a speaker never disturbs the identity underneath.
    // NOT merging clusters here, deliberately. An automatic end-of-meeting merge was written and
    // then backed out: on a real recording of an interview, one cluster already held both people
    // (the interviewer's "Stephen, that's fascinating..." sat in the same cluster as Stephen's
    // own account of starting his podcast). Merging on top of that fuses two speakers into one,
    // which is worse than the over-splitting it was meant to fix and much harder to notice.
    // `SpeakerTracker::merge_similar` is kept and tested, for use once clustering runs over
    // per-utterance embeddings instead of these drifting centroids.

    {
        let store = manager.store.lock().unwrap();
        for tracker in [&mic_tracker, &sys_tracker] {
            for print in tracker.voiceprints() {
                if let Err(err) = store.save_voiceprint(
                    meeting_id,
                    &print.speaker_id,
                    &print.voiceprint_id,
                    &print.embedding,
                    &print.backend,
                ) {
                    log::warn!("could not store the voiceprint for {}: {err}", print.speaker_id);
                }
            }
        }
    }

    let _ = manager
        .store
        .lock()
        .unwrap()
        .set_status(meeting_id, MeetingStatus::Processing);

    finalize_meeting(&app, &manager, meeting_id, started_at);
}

fn transcribe_loop(
    app: AppHandle,
    meeting_id: i64,
    your_name: String,
    job_rx: mpsc::Receiver<TranscribeJob>,
    stop: Arc<AtomicBool>,
) {
    let Some(manager) = app.try_state::<Arc<MeetingManager>>() else {
        return;
    };
    let Some(transcription) = app.try_state::<Arc<TranscriptionManager>>() else {
        return;
    };
    let speakers = Arc::new(Mutex::new(HashMap::from([(
        "you".to_string(),
        your_name.clone(),
    )])));

    while let Ok(job) = job_rx.recv() {
        if job.audio.len() < FRAME_SAMPLES * 8 {
            continue;
        }
        match transcription.transcribe(job.audio) {
            Ok(text) => {
                let text = text.trim().to_string();
                if text.is_empty() {
                    continue;
                }
                let mut speaker_name = job.speaker_name.clone();
                {
                    let mut map = speakers.lock().unwrap();
                    if job.source == AudioSource::System {
                        map.entry(job.speaker_id.clone())
                            .or_insert_with(|| job.speaker_name.clone());
                        diarize::apply_context_names(&mut map, &job.speaker_id, &text);
                    }
                    if let Some(name) = map.get(&job.speaker_id) {
                        speaker_name = name.clone();
                    }
                }
                let utterance_id = match manager.store.lock().unwrap().insert_utterance(
                    meeting_id,
                    &job.speaker_id,
                    &speaker_name,
                    job.source,
                    job.start_ms,
                    job.end_ms,
                    &text,
                ) {
                    Ok(id) => id,
                    Err(err) => {
                        warn!("Failed to store utterance: {err}");
                        continue;
                    }
                };
                let _ = manager.store.lock().unwrap().upsert_speaker(
                    meeting_id,
                    &job.speaker_id,
                    &speaker_name,
                );
                let utterance = MeetingUtterance {
                    id: utterance_id,
                    meeting_id,
                    speaker_id: job.speaker_id,
                    speaker_name,
                    source: job.source,
                    start_ms: job.start_ms,
                    end_ms: job.end_ms,
                    text,
                };
                let _ = MeetingUtteranceEvent { utterance }.emit(&app);
            }
            Err(err) => {
                if stop.load(Ordering::Relaxed) {
                    warn!("Meeting transcription failed during shutdown: {err}");
                } else {
                    warn!("Meeting transcription failed: {err}");
                }
            }
        }
    }
}

fn finalize_meeting(app: &AppHandle, manager: &MeetingManager, meeting_id: i64, started_at: i64) {
    let meeting = match manager.store.lock().unwrap().get_meeting(meeting_id) {
        Ok(meeting) => meeting,
        Err(err) => {
            warn!("Could not load meeting {meeting_id} to finalize: {err}");
            return;
        }
    };
    let notes = match crate::meeting::stash::dump_to_inbox(&meeting) {
        Ok(path) => notes::inbox_placeholder(&path.to_string_lossy()),
        Err(err) => {
            warn!("Could not write transcript to Head Secretary inbox: {err}");
            notes::empty_notes()
        }
    };

    let ended_at = started_at
        + meeting
            .utterances
            .iter()
            .map(|u| u.end_ms)
            .max()
            .unwrap_or(now_ms() - started_at);
    let _ = manager
        .store
        .lock()
        .unwrap()
        .finish_meeting(meeting_id, ended_at.max(now_ms()), &notes);

    if let Ok(record) = manager.get_meeting(meeting_id) {
        let _ = crate::meeting::types::MeetingStateEvent { meeting: record }.emit(app);
    }
}

fn push_vad(
    vad: &mut SourceVad,
    frame: &[f32],
    elapsed_ms: i64,
    source: AudioSource,
    speaker_id: &str,
    speaker_name: &str,
    tracker: Option<&mut SpeakerTracker>,
    job_tx: &mpsc::Sender<TranscribeJob>,
) {
    let voiced = energy(frame) > ENERGY_THRESHOLD;
    if voiced {
        if !vad.speaking {
            vad.speaking = true;
            vad.start_ms = elapsed_ms.saturating_sub(30);
            vad.buffer.clear();
        }
        vad.hangover = HANGOVER_FRAMES;
        vad.buffer.extend_from_slice(frame);
        if vad.buffer.len() >= MAX_SPEECH_FRAMES * FRAME_SAMPLES {
            emit_job(
                vad,
                elapsed_ms,
                source,
                speaker_id,
                speaker_name,
                tracker,
                job_tx,
            );
            vad.speaking = true;
            vad.start_ms = elapsed_ms;
        }
        return;
    }

    if vad.speaking {
        if vad.hangover > 0 {
            vad.hangover -= 1;
            vad.buffer.extend_from_slice(frame);
            return;
        }
        emit_job(
            vad,
            elapsed_ms,
            source,
            speaker_id,
            speaker_name,
            tracker,
            job_tx,
        );
    }
}

fn flush_vad(
    vad: &mut SourceVad,
    elapsed_ms: i64,
    source: AudioSource,
    speaker_id: &str,
    speaker_name: &str,
    tracker: Option<&mut SpeakerTracker>,
    job_tx: &mpsc::Sender<TranscribeJob>,
) {
    if vad.speaking || !vad.buffer.is_empty() {
        emit_job(
            vad,
            elapsed_ms,
            source,
            speaker_id,
            speaker_name,
            tracker,
            job_tx,
        );
    }
}

fn emit_job(
    vad: &mut SourceVad,
    elapsed_ms: i64,
    source: AudioSource,
    speaker_id: &str,
    speaker_name: &str,
    tracker: Option<&mut SpeakerTracker>,
    job_tx: &mpsc::Sender<TranscribeJob>,
) {
    if vad.buffer.len() < MIN_SPEECH_FRAMES * FRAME_SAMPLES {
        vad.speaking = false;
        vad.buffer.clear();
        return;
    }
    let audio = std::mem::take(&mut vad.buffer);
    // `speaker_id` doubles as the seed label for this tracker's very first
    // voice ("you" for the mic, "spk_1" for system audio) when a tracker is
    // supplied; with no tracker it's just used verbatim (mic when speaker-ID
    // clustering is off).
    let (speaker_id, speaker_name) = if let Some(tracker) = tracker {
        let id = tracker.assign(&audio, speaker_id);
        let name = if id == "you" {
            speaker_name.to_string()
        } else {
            diarize::default_remote_name(&id)
        };
        (id, name)
    } else {
        (speaker_id.to_string(), speaker_name.to_string())
    };
    let job = TranscribeJob {
        source,
        speaker_id,
        speaker_name,
        start_ms: vad.start_ms,
        end_ms: elapsed_ms,
        audio,
    };
    vad.speaking = false;
    vad.hangover = 0;
    let _ = job_tx.send(job);
}

fn energy(frame: &[f32]) -> f32 {
    if frame.is_empty() {
        return 0.0;
    }
    (frame.iter().map(|s| s * s).sum::<f32>() / frame.len() as f32).sqrt()
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
