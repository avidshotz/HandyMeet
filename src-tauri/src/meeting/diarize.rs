use super::speaker_id::SpeakerEmbedder;
use regex::Regex;
use rustfft::num_complex::Complex;
use rustfft::FftPlanner;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

const EMBED_BINS: usize = 32;
// Cosine-similarity cutoffs for "same speaker as an existing centroid" — tuned
// separately per backend since the FFT heuristic and the ML embedding space
// have different similarity distributions. The ML value matches sherpa-onnx's
// own speaker-embedding example default.
const NEW_SPEAKER_THRESHOLD_HEURISTIC: f32 = 0.62;
const NEW_SPEAKER_THRESHOLD_ML: f32 = 0.6;

#[derive(Default)]
pub struct SpeakerTracker {
    embedder: Option<Arc<SpeakerEmbedder>>,
    next_index: usize,
    centroids: Vec<(String, Vec<f32>)>,
}

impl SpeakerTracker {
    pub fn new(embedder: Option<Arc<SpeakerEmbedder>>) -> Self {
        Self {
            embedder,
            next_index: 0,
            centroids: Vec::new(),
        }
    }

    /// Assign (or create) a speaker id for `audio`. `first_speaker_id` names
    /// the very first voice this tracker ever sees (e.g. "you" for the mic
    /// channel, or "spk_1" for a channel with no fixed identity) — every
    /// later *distinct* voice still gets an auto-incrementing "spk_N" label.
    pub fn assign(&mut self, audio: &[f32], first_speaker_id: &str) -> String {
        let (embedding, threshold) = match self.embedder.as_ref().and_then(|e| e.embed(audio)) {
            Some(embedding) => (embedding, NEW_SPEAKER_THRESHOLD_ML),
            None => (spectral_embedding(audio), NEW_SPEAKER_THRESHOLD_HEURISTIC),
        };

        let mut best_id = None;
        let mut best_sim = -1.0_f32;
        for (id, centroid) in &self.centroids {
            // Embeddings from different backends aren't comparable; only
            // match against centroids of the same dimension (i.e. the same
            // backend that's currently active for this tracker/session).
            if centroid.len() != embedding.len() {
                continue;
            }
            let sim = cosine(&embedding, centroid);
            if sim > best_sim {
                best_sim = sim;
                best_id = Some(id.clone());
            }
        }
        if let Some(id) = best_id {
            if best_sim >= threshold {
                if let Some((_, centroid)) = self.centroids.iter_mut().find(|(cid, _)| cid == &id) {
                    blend(centroid, &embedding, 0.2);
                }
                return id;
            }
        }

        // Always reserve the next "spk_N" slot, even for the seeded first
        // speaker (whose *returned* id may be a custom label like "you") —
        // otherwise the first genuinely new voice would collide with it.
        self.next_index += 1;
        let id = if self.centroids.is_empty() {
            first_speaker_id.to_string()
        } else {
            format!("spk_{}", self.next_index)
        };
        self.centroids.push((id.clone(), embedding));
        id
    }
}

pub fn guess_names_from_text(text: &str) -> Vec<String> {
    static INTRO: OnceLock<Regex> = OnceLock::new();
    let intro = INTRO.get_or_init(|| {
        Regex::new(
            r"(?i)\b(?:i(?:['’]?m| am)|this is|my name is|it's)\s+([A-Z][a-z]+(?:\s+[A-Z][a-z]+)?)",
        )
        .expect("intro regex")
    });
    intro
        .captures_iter(text)
        .filter_map(|caps| caps.get(1).map(|m| m.as_str().trim().to_string()))
        .filter(|name| !is_common_false_positive(name))
        .collect()
}

pub fn apply_context_names(
    speakers: &mut HashMap<String, String>,
    speaker_id: &str,
    text: &str,
) {
    if speaker_id == "you" {
        return;
    }
    if let Some(name) = guess_names_from_text(text).into_iter().next() {
        let current = speakers.get(speaker_id).cloned().unwrap_or_default();
        if current.is_empty() || current.starts_with("Speaker ") {
            speakers.insert(speaker_id.to_string(), name);
        }
    }
}

pub fn default_remote_name(speaker_id: &str) -> String {
    match speaker_id.strip_prefix("spk_") {
        Some(n) => format!("Speaker {n}"),
        None => speaker_id.to_string(),
    }
}

fn is_common_false_positive(name: &str) -> bool {
    matches!(
        name.to_lowercase().as_str(),
        "good" | "just" | "going" | "here" | "there" | "fine" | "okay" | "ok"
    )
}

fn spectral_embedding(audio: &[f32]) -> Vec<f32> {
    let n = 512.min(audio.len().next_power_of_two().max(64));
    let mut buffer = vec![Complex { re: 0.0, im: 0.0 }; n];
    let take = audio.len().min(n);
    let start = audio.len().saturating_sub(take);
    for (i, sample) in audio[start..].iter().enumerate() {
        buffer[i].re = *sample;
    }
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(n);
    fft.process(&mut buffer);

    let mut bins = vec![0.0_f32; EMBED_BINS];
    let usable = n / 2;
    for i in 0..usable {
        let mag = buffer[i].norm();
        let bin = i * EMBED_BINS / usable;
        bins[bin] += mag;
    }
    let mut max = bins.iter().copied().fold(0.0_f32, f32::max);
    if max < 1e-6 {
        max = 1.0;
    }
    for value in &mut bins {
        *value = (*value / max).ln_1p();
    }
    bins
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let mut dot = 0.0;
    let mut na = 0.0;
    let mut nb = 0.0;
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na < 1e-8 || nb < 1e-8 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

fn blend(centroid: &mut [f32], sample: &[f32], rate: f32) {
    for (c, s) in centroid.iter_mut().zip(sample) {
        *c = *c * (1.0 - rate) + *s * rate;
    }
}
