//! Optional small ML voice-embedding model for meeting speaker clustering.
//!
//! Off by default (`meeting_speaker_id_enabled` setting). When on, this
//! downloads a ~29MB CAM++ speaker-embedding model (WeSpeaker, Apache-2.0,
//! via k2-fsa/sherpa-onnx) once and reuses the ONNX Runtime already linked
//! into the app for Whisper/Parakeet transcription, so there's no second
//! runtime loaded. Inference is a few ms per utterance on CPU. Falls back to
//! the lightweight FFT heuristic in `diarize.rs` whenever the model isn't
//! downloaded yet (or fails to load), so a meeting never blocks on this.

use anyhow::{anyhow, Result};
use sherpa_onnx::{SpeakerEmbeddingExtractor, SpeakerEmbeddingExtractorConfig};
use std::path::{Path, PathBuf};
use tauri::AppHandle;

const MODEL_FILENAME: &str = "wespeaker_en_voxceleb_CAM++.onnx";
const MODEL_URL: &str = "https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-recongition-models/wespeaker_en_voxceleb_CAM%2B%2B.onnx";

pub fn model_dir(app: &AppHandle) -> Result<PathBuf> {
    let dir = crate::portable::app_data_dir(app)
        .map_err(|e| anyhow!("{e}"))?
        .join("models")
        .join("speaker-embedding");
    Ok(dir)
}

pub fn model_path(app: &AppHandle) -> Result<PathBuf> {
    Ok(model_dir(app)?.join(MODEL_FILENAME))
}

pub fn is_model_ready(app: &AppHandle) -> bool {
    model_path(app)
        .map(|p| p.is_file() && p.metadata().map(|m| m.len() > 1_000_000).unwrap_or(false))
        .unwrap_or(false)
}

/// Download the model into place if it isn't already there. Streams to a
/// `.part` file first and renames atomically on success, so a killed/failed
/// download never leaves a corrupt file that looks "ready".
pub async fn ensure_model_downloaded(app: &AppHandle) -> Result<()> {
    if is_model_ready(app) {
        return Ok(());
    }
    let dir = model_dir(app)?;
    std::fs::create_dir_all(&dir)?;
    let final_path = dir.join(MODEL_FILENAME);
    let tmp_path = dir.join(format!("{MODEL_FILENAME}.part"));

    log::info!("Downloading speaker-ID model from {MODEL_URL}");
    let response = reqwest::get(MODEL_URL).await?.error_for_status()?;
    let bytes = response.bytes().await?;
    std::fs::write(&tmp_path, &bytes)?;
    std::fs::rename(&tmp_path, &final_path)?;
    log::info!(
        "Speaker-ID model ready ({} bytes) at {}",
        bytes.len(),
        final_path.display()
    );
    Ok(())
}

/// Thin wrapper around the sherpa-onnx CAM++ speaker-embedding extractor.
/// `Send + Sync`: the underlying C library is safe for single-object use
/// across threads (the crate itself asserts this on the extractor type).
pub struct SpeakerEmbedder {
    extractor: SpeakerEmbeddingExtractor,
}

impl SpeakerEmbedder {
    pub fn load(path: &Path) -> Result<Self> {
        let config = SpeakerEmbeddingExtractorConfig {
            model: Some(path.to_string_lossy().into_owned()),
            num_threads: 1,
            debug: false,
            provider: Some("cpu".to_string()),
        };
        let extractor = SpeakerEmbeddingExtractor::create(&config)
            .ok_or_else(|| anyhow!("failed to create speaker embedding extractor"))?;
        Ok(Self { extractor })
    }

    /// `audio` must be 16kHz mono f32 samples (matches the meeting pipeline's
    /// existing VAD buffers). Returns `None` if the buffer is too short for
    /// the model to produce an embedding.
    pub fn embed(&self, audio: &[f32]) -> Option<Vec<f32>> {
        let stream = self.extractor.create_stream()?;
        stream.accept_waveform(16_000, audio);
        stream.input_finished();
        if !self.extractor.is_ready(&stream) {
            return None;
        }
        self.extractor.compute(&stream)
    }
}
