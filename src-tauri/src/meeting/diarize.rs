use super::speaker_id::SpeakerEmbedder;
use regex::Regex;
use rustfft::num_complex::Complex;
use rustfft::FftPlanner;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

const EMBED_BINS: usize = 32;
// Cosine-similarity cutoffs for "same speaker as an existing centroid" — tuned
// separately per backend since the FFT heuristic and the ML embedding space
// have different similarity distributions. The ML value matches sherpa-onnx's
// own speaker-embedding example default.
const NEW_SPEAKER_THRESHOLD_HEURISTIC: f32 = 0.62;
const NEW_SPEAKER_THRESHOLD_ML: f32 = 0.6;

/// A segment shorter than this is not allowed to invent a new speaker.
///
/// CAM++ needs a couple of seconds before its embedding describes the *voice* rather than the
/// words being said; below that, two clips of one person can sit further apart than two clips of
/// different people. The VAD emits from 600ms, so without this gate a two-word interjection
/// founds a speaker of its own — measured on a real 9-minute recording of one person, that alone
/// produced four spurious speakers, each holding three or four seconds of audio.
///
/// Short speech is still transcribed and still attributed; it just joins the closest voice
/// already known instead of starting a new one, and does not move that voice's centroid.
const MIN_SAMPLES_FOR_NEW_SPEAKER: usize = 16_000 * 2;

/// How alike two clusters must be, on average, before they are folded back together at the end
/// of a meeting. Deliberately no lower than the live threshold: the gate above removes the
/// over-splitting this was meant to mop up, so there is no reason to spend the margin that keeps
/// two genuinely different people apart.
pub const MERGE_THRESHOLD: f32 = 0.6;

/// One voice: the cluster label used inside this meeting, a stable id for the voice itself,
/// and the vector. The label and the name are disposable; the voiceprint id is not.
#[derive(Clone)]
pub struct Voiceprint {
    pub speaker_id: String,
    pub voiceprint_id: String,
    pub embedding: Vec<f32>,
    pub backend: String,
}

#[derive(Default)]
pub struct SpeakerTracker {
    embedder: Option<Arc<SpeakerEmbedder>>,
    /// Shared between the trackers of one meeting — see `pair`.
    next_index: Arc<AtomicUsize>,
    centroids: Vec<Voiceprint>,
}

impl SpeakerTracker {
    /// The two trackers for one meeting — microphone and system audio — sharing one "spk_N"
    /// counter.
    ///
    /// They have to share it. The label is half the primary key for a speaker within a meeting,
    /// so two independent counters hand "spk_2" to the second voice on the microphone *and* to
    /// the second voice on the call: two different people, one row, one voiceprint overwriting
    /// the other. Constructing them together is the only way to make that impossible to forget.
    pub fn pair(embedder: Option<Arc<SpeakerEmbedder>>) -> (Self, Self) {
        let shared = Arc::new(AtomicUsize::new(0));
        (
            Self {
                embedder: embedder.clone(),
                next_index: shared.clone(),
                centroids: Vec::new(),
            },
            Self {
                embedder,
                next_index: shared,
                centroids: Vec::new(),
            },
        )
    }

    /// Assign (or create) a speaker id for `audio`. `first_speaker_id` names
    /// the very first voice this tracker ever sees (e.g. "you" for the mic
    /// channel, or "spk_1" for a channel with no fixed identity) — every
    /// later *distinct* voice still gets an auto-incrementing "spk_N" label.
    pub fn assign(&mut self, audio: &[f32], first_speaker_id: &str) -> String {
        let (mut embedding, threshold) = match self.embedder.as_ref().and_then(|e| e.embed(audio)) {
            Some(embedding) => (embedding, NEW_SPEAKER_THRESHOLD_ML),
            None => (spectral_embedding(audio), NEW_SPEAKER_THRESHOLD_HEURISTIC),
        };
        // Unit length, so a centroid is the average *direction* of a voice. CAM++ returns vectors
        // whose magnitudes vary by nearly a factor of two between clips; blending those raw lets
        // a loud clip drag the centroid further than a quiet one for no reason to do with who is
        // speaking. Cosine already ignores magnitude, so this changes only the blending.
        normalize(&mut embedding);

        let mut best_id = None;
        let mut best_sim = -1.0_f32;
        for print in &self.centroids {
            // Embeddings from different backends aren't comparable; only
            // match against centroids of the same dimension (i.e. the same
            // backend that's currently active for this tracker/session).
            if print.embedding.len() != embedding.len() {
                continue;
            }
            let sim = cosine(&embedding, &print.embedding);
            if sim > best_sim {
                best_sim = sim;
                best_id = Some(print.speaker_id.clone());
            }
        }
        if let Some(id) = best_id.clone() {
            if best_sim >= threshold {
                if let Some(print) = self
                    .centroids
                    .iter_mut()
                    .find(|p| p.speaker_id == id)
                {
                    blend(&mut print.embedding, &embedding, 0.2);
                    normalize(&mut print.embedding);
                }
                return id;
            }
        }

        // Too little speech to trust as a new voice: attribute it to the nearest one we already
        // know, and leave that centroid alone so an unreliable embedding cannot corrupt it.
        if audio.len() < MIN_SAMPLES_FOR_NEW_SPEAKER && !self.centroids.is_empty() {
            if let Some(id) = best_id {
                return id;
            }
        }

        // Always reserve the next "spk_N" slot, even for the seeded first
        // speaker (whose *returned* id may be a custom label like "you") —
        // otherwise the first genuinely new voice would collide with it.
        let index = self.next_index.fetch_add(1, Ordering::Relaxed) + 1;
        let id = if self.centroids.is_empty() {
            first_speaker_id.to_string()
        } else {
            format!("spk_{index}")
        };
        self.centroids.push(Voiceprint {
            speaker_id: id.clone(),
            // A fresh id for this voice. It never changes again, whatever the speaker is
            // later called, which is what makes naming non-destructive.
            voiceprint_id: new_voiceprint_id(index),
            backend: if self.embedder.is_some() { "camplusplus" } else { "fft" }.to_string(),
            embedding,
        });
        id
    }

    /// The voiceprints gathered so far, for storing against the meeting.
    pub fn voiceprints(&self) -> &[Voiceprint] {
        &self.centroids
    }
}

/// Agglomerative grouping by *average* similarity between groups.
///
/// Average link rather than single link on purpose. Single link merges A and C whenever some B
/// is close to both, so one ambiguous cluster can chain two different people into one speaker —
/// the failure that is hardest to notice and most annoying to undo. Average link makes a whole
/// group vouch for a merge.
///
/// `sim(i, j)` must be symmetric. Returns groups of indices, each sorted, outer order by first
/// member, so the result is stable to compare against.
pub fn average_link_groups<F>(count: usize, threshold: f32, sim: F) -> Vec<Vec<usize>>
where
    F: Fn(usize, usize) -> f32,
{
    let mut groups: Vec<Vec<usize>> = (0..count).map(|i| vec![i]).collect();
    loop {
        let mut best = (threshold, None);
        for i in 0..groups.len() {
            for j in (i + 1)..groups.len() {
                let total: f32 = groups[i]
                    .iter()
                    .flat_map(|a| groups[j].iter().map(move |b| (*a, *b)))
                    .map(|(a, b)| sim(a, b))
                    .sum();
                let average = total / (groups[i].len() * groups[j].len()) as f32;
                if average >= best.0 {
                    best = (average, Some((i, j)));
                }
            }
        }
        match best.1 {
            Some((i, j)) => {
                let merged = groups.remove(j);
                groups[i].extend(merged);
            }
            None => break,
        }
    }
    for group in groups.iter_mut() {
        group.sort_unstable();
    }
    groups.sort_by_key(|g| g[0]);
    groups
}

impl SpeakerTracker {
    /// Fold clusters that turned out to be the same voice back into one.
    ///
    /// The live pass has to decide who is speaking before it has heard the rest of the meeting;
    /// this runs once at the end, when every centroid is as good as it is going to get, and can
    /// undo a split the live pass could not have avoided. Returns the relabelling to apply to
    /// the stored utterances, as (old id, id it is now part of) — empty when nothing merged.
    ///
    /// The surviving id is always the earliest of the group, so a meeting keeps "you" and the
    /// lowest-numbered speakers rather than renaming everyone.
    pub fn merge_similar(&mut self, threshold: f32) -> Vec<(String, String)> {
        if self.centroids.len() < 2 {
            return Vec::new();
        }
        let groups = average_link_groups(self.centroids.len(), threshold, |a, b| {
            let (x, y) = (&self.centroids[a], &self.centroids[b]);
            if x.embedding.len() == y.embedding.len() {
                cosine(&x.embedding, &y.embedding)
            } else {
                // Different backends are not comparable, so never merge across them.
                -1.0
            }
        });

        let mut remap = Vec::new();
        let mut keep = Vec::new();
        for group in groups {
            let survivor = group[0];
            for &other in &group[1..] {
                remap.push((
                    self.centroids[other].speaker_id.clone(),
                    self.centroids[survivor].speaker_id.clone(),
                ));
            }
            keep.push(survivor);
        }
        if remap.is_empty() {
            return remap;
        }
        let mut kept: Vec<Voiceprint> = keep
            .into_iter()
            .map(|i| self.centroids[i].clone())
            .collect();
        kept.sort_by(|a, b| a.speaker_id.cmp(&b.speaker_id));
        self.centroids = kept;
        remap
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

/// A stable id for a voice. Time plus the slot it was found in, which is unique enough for
/// voices in meetings and avoids pulling in a uuid crate for three fields.
fn new_voiceprint_id(index: usize) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("vp_{nanos:x}_{index}")
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

fn normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 1e-8 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

fn blend(centroid: &mut [f32], sample: &[f32], rate: f32) {
    for (c, s) in centroid.iter_mut().zip(sample) {
        *c = *c * (1.0 - rate) + *s * rate;
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    /// Distinct tones, far enough apart that the heuristic embedding treats them as
    /// different voices. Long enough by default to clear `MIN_SAMPLES_FOR_NEW_SPEAKER`.
    fn tone(hz: f32) -> Vec<f32> {
        tone_of(hz, 16_000 * 3)
    }

    fn tone_of(hz: f32, samples: usize) -> Vec<f32> {
        (0..samples)
            .map(|i| (i as f32 * hz * std::f32::consts::TAU / 16_000.0).sin() * 0.5)
            .collect()
    }

    #[test]
    fn paired_trackers_never_hand_out_the_same_label() {
        let (mut mic, mut sys) = SpeakerTracker::pair(None);

        let mut labels = vec![mic.assign(&tone(110.0), "you"), sys.assign(&tone(180.0), "spk_1")];
        labels.push(mic.assign(&tone(900.0), "you"));
        labels.push(sys.assign(&tone(2600.0), "spk_1"));

        // Guard the guard: if the tones were not heard as four separate voices the labels
        // could not collide anyway, and this test would pass without testing anything.
        assert_eq!(mic.voiceprints().len(), 2, "mic heard {labels:?}");
        assert_eq!(sys.voiceprints().len(), 2, "system audio heard {labels:?}");

        let unique: std::collections::HashSet<_> = labels.iter().collect();
        assert_eq!(
            unique.len(),
            labels.len(),
            "two voices shared a label, so they would share one row: {labels:?}"
        );
    }

    #[test]
    fn a_voiceprint_id_outlives_the_label_it_was_found_under() {
        let (mut mic, _sys) = SpeakerTracker::pair(None);
        let voice = tone(110.0);

        let id = mic.assign(&voice, "you");
        let first = mic.voiceprints()[0].voiceprint_id.clone();

        // The same voice again: recognised, centroid nudged, identity unchanged.
        assert_eq!(mic.assign(&voice, "you"), id);
        assert_eq!(mic.voiceprints().len(), 1);
        assert_eq!(mic.voiceprints()[0].voiceprint_id, first);
    }


    /// The similarity matrix measured from a real 9-minute recording of ONE person that the
    /// live pass split into seven speakers. Only the numbers are kept here, not the voiceprints
    /// they came from. Order: you, spk_2, spk_3, spk_4, spk_5, spk_6, spk_7 — where you, spk_2
    /// and spk_3 hold 53s, 109s and 346s, and spk_4..spk_7 hold three or four seconds each.
    const REAL_SPLIT: [[f32; 7]; 7] = [
        [1.000, 0.756, 0.574, 0.447, 0.301, 0.391, 0.549],
        [0.756, 1.000, 0.739, 0.587, 0.433, 0.562, 0.446],
        [0.574, 0.739, 1.000, 0.462, 0.323, 0.586, 0.478],
        [0.447, 0.587, 0.462, 1.000, 0.525, 0.546, 0.265],
        [0.301, 0.433, 0.323, 0.525, 1.000, 0.611, 0.261],
        [0.391, 0.562, 0.586, 0.546, 0.611, 1.000, 0.336],
        [0.549, 0.446, 0.478, 0.265, 0.261, 0.336, 1.000],
    ];

    /// What these numbers do NOT say, recorded here because it was briefly got wrong: the three
    /// well-sampled clusters are *not* three copies of one speaker. Reading the transcript showed
    /// cluster 2 holding both the interviewer and the interviewee — it contains both "I want to
    /// start a podcast called The Diary of a CEO" and, twelve seconds later, someone asking
    /// "Stephen, that's fascinating. Would you mind giving us a case study?".
    ///
    /// So these similarities describe a clustering that is wrong in both directions at once, and
    /// nothing here should be used to justify a merge threshold.
    #[test]
    fn the_measured_similarities_do_not_separate_anyone() {
        let mut same_speaker_lows = 0;
        let mut cross_cluster_highs = 0;
        for a in 0..7 {
            for b in (a + 1)..7 {
                if REAL_SPLIT[a][b] < 0.45 {
                    same_speaker_lows += 1;
                }
                if REAL_SPLIT[a][b] > 0.7 {
                    cross_cluster_highs += 1;
                }
            }
        }
        assert!(
            same_speaker_lows > 0 && cross_cluster_highs > 0,
            "the measured spread should straddle any single threshold; if this ever stops being \
             true the recording or the embedder changed and the whole approach can be revisited"
        );
    }

    #[test]
    fn average_link_does_not_chain_two_people_through_a_middle_cluster() {
        // A is close to B, B is close to C, A and C are not alike at all. Single link would
        // merge all three; average link must not, because that is how two people become one.
        let sim = |a: usize, b: usize| -> f32 {
            match (a.min(b), a.max(b)) {
                (i, j) if i == j => 1.0,
                (0, 1) => 0.66,
                (1, 2) => 0.66,
                (0, 2) => 0.10,
                _ => 0.0,
            }
        };
        let groups = average_link_groups(3, 0.6, sim);
        assert!(
            groups.len() >= 2,
            "average link chained a merge it should have refused: {groups:?}"
        );
    }

    #[test]
    fn a_brief_clip_does_not_invent_a_speaker() {
        let (mut mic, _sys) = SpeakerTracker::pair(None);
        mic.assign(&tone(110.0), "you");
        let centroid_before = mic.voiceprints()[0].embedding.clone();

        // A clearly different voice, but only half a second of it.
        let brief = tone_of(2600.0, 8_000);
        let id = mic.assign(&brief, "you");

        assert_eq!(id, "you", "a brief clip should join the nearest known voice");
        assert_eq!(mic.voiceprints().len(), 1, "and must not found a new one");
        assert_eq!(
            mic.voiceprints()[0].embedding, centroid_before,
            "and must not drag the centroid it joined"
        );
    }

    #[test]
    fn a_long_clip_of_a_different_voice_still_gets_its_own_speaker() {
        // The gate must not cost us the thing the feature is for.
        let (mut mic, _sys) = SpeakerTracker::pair(None);
        assert_eq!(mic.assign(&tone(110.0), "you"), "you");
        let second = mic.assign(&tone(2600.0), "you");
        assert_ne!(second, "you", "a well-sampled second voice must still separate");
        assert_eq!(mic.voiceprints().len(), 2);
    }

    #[test]
    fn centroids_stay_unit_length() {
        let (mut mic, _sys) = SpeakerTracker::pair(None);
        mic.assign(&tone(110.0), "you");
        mic.assign(&tone(112.0), "you"); // near enough to blend into the same voice
        for print in mic.voiceprints() {
            let norm = print.embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
            assert!((norm - 1.0).abs() < 1e-4, "centroid norm drifted to {norm}");
        }
    }
}
