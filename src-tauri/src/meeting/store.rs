use super::types::{
    AudioSource, MeetingListItem, MeetingNotes, MeetingRecord, MeetingSpeaker, MeetingStatus,
    MeetingUtterance,
};
use anyhow::{anyhow, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use std::fs;
use std::path::PathBuf;

pub struct MeetingStore {
    db_path: PathBuf,
}

impl MeetingStore {
    pub fn new(app_data_dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&app_data_dir)?;
        let store = Self {
            db_path: app_data_dir.join("meetings.db"),
        };
        store.init()?;
        Ok(store)
    }

    fn connect(&self) -> Result<Connection> {
        Ok(Connection::open(&self.db_path)?)
    }

    fn init(&self) -> Result<()> {
        let conn = self.connect()?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meetings (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                title TEXT NOT NULL,
                started_at INTEGER NOT NULL,
                ended_at INTEGER,
                your_name TEXT NOT NULL,
                status TEXT NOT NULL,
                notes_json TEXT NOT NULL DEFAULT '{}'
            );
            CREATE TABLE IF NOT EXISTS utterances (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                meeting_id INTEGER NOT NULL,
                speaker_id TEXT NOT NULL,
                speaker_name TEXT NOT NULL,
                source TEXT NOT NULL,
                start_ms INTEGER NOT NULL,
                end_ms INTEGER NOT NULL,
                text TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS speakers (
                meeting_id INTEGER NOT NULL,
                speaker_id TEXT NOT NULL,
                display_name TEXT NOT NULL,
                PRIMARY KEY (meeting_id, speaker_id)
            );
            CREATE TABLE IF NOT EXISTS kv (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );",
        )?;

        // Voiceprints, added to existing databases in place. Three separate things on purpose:
        //   speaker_id    the cluster label inside one meeting ("spk_2"), what utterances point at
        //   voiceprint_id a stable id for the voice itself, which outlives any label
        //   embedding     the vector, so the same voice can be recognised in a later meeting
        // Renaming a speaker touches only display_name, so a name you type can never damage the
        // identity underneath it.
        for (column, decl) in [
            ("voiceprint_id", "TEXT"),
            ("embedding", "TEXT"),
            ("embedding_backend", "TEXT"),
        ] {
            let sql = format!("ALTER TABLE speakers ADD COLUMN {column} {decl}");
            if let Err(err) = conn.execute(&sql, []) {
                // "duplicate column name" just means an older run already added it.
                if !err.to_string().contains("duplicate column") {
                    return Err(err.into());
                }
            }
        }
        Ok(())
    }

    /// Fold one speaker into another: its utterances move across and the empty row goes.
    ///
    /// The surviving speaker keeps the name it already has, including one you typed. A merge
    /// undoes a mistake the live pass made about *who was speaking*; it is not an opinion about
    /// what they should be called.
    pub fn merge_speakers(&self, meeting_id: i64, from: &str, into: &str) -> Result<()> {
        if from == into {
            return Ok(());
        }
        let conn = self.connect()?;
        let name: Option<String> = conn
            .query_row(
                "SELECT display_name FROM speakers WHERE meeting_id = ?1 AND speaker_id = ?2",
                params![meeting_id, into],
                |row| row.get(0),
            )
            .optional()?;
        let name = name.unwrap_or_else(|| super::diarize::default_remote_name(into));
        conn.execute(
            "UPDATE utterances SET speaker_id = ?1, speaker_name = ?2
             WHERE meeting_id = ?3 AND speaker_id = ?4",
            params![into, name, meeting_id, from],
        )?;
        conn.execute(
            "DELETE FROM speakers WHERE meeting_id = ?1 AND speaker_id = ?2",
            params![meeting_id, from],
        )?;
        Ok(())
    }

    /// Store the voiceprint for a speaker. Never touches an existing display_name.
    ///
    /// Inserts rather than updates, because a voice can be heard without ever being transcribed —
    /// someone says something the recogniser returns as empty, and no speaker row is written. A
    /// plain UPDATE would change nothing and say nothing, losing the print in silence. On conflict
    /// only the voiceprint columns are set, so a name you typed survives untouched.
    pub fn save_voiceprint(
        &self,
        meeting_id: i64,
        speaker_id: &str,
        voiceprint_id: &str,
        embedding: &[f32],
        backend: &str,
    ) -> Result<()> {
        let conn = self.connect()?;
        let vector = serde_json::to_string(embedding)?;
        let fallback_name = super::diarize::default_remote_name(speaker_id);
        conn.execute(
            "INSERT INTO speakers (meeting_id, speaker_id, display_name,
                                   voiceprint_id, embedding, embedding_backend)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(meeting_id, speaker_id) DO UPDATE SET
                 voiceprint_id = excluded.voiceprint_id,
                 embedding = excluded.embedding,
                 embedding_backend = excluded.embedding_backend",
            params![
                meeting_id,
                speaker_id,
                fallback_name,
                voiceprint_id,
                vector,
                backend
            ],
        )?;
        Ok(())
    }

    /// Every stored voiceprint, for matching a voice against earlier meetings.
    pub fn voiceprints(&self) -> Result<Vec<(String, String, Vec<f32>, String)>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT voiceprint_id, display_name, embedding, embedding_backend
             FROM speakers
             WHERE voiceprint_id IS NOT NULL AND embedding IS NOT NULL",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let vector: String = row.get(2)?;
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    serde_json::from_str::<Vec<f32>>(&vector).unwrap_or_default(),
                    row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                ))
            })?
            .filter_map(|r| r.ok())
            .filter(|(_, _, v, _): &(String, String, Vec<f32>, String)| !v.is_empty())
            .collect();
        Ok(rows)
    }

    pub fn get_kv(&self, key: &str) -> Result<Option<String>> {
        let conn = self.connect()?;
        Ok(conn
            .query_row("SELECT value FROM kv WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()?)
    }

    pub fn set_kv(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "INSERT INTO kv(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn create_meeting(&self, title: &str, your_name: &str, started_at: i64) -> Result<i64> {
        let conn = self.connect()?;
        conn.execute(
            "INSERT INTO meetings (title, started_at, your_name, status, notes_json)
             VALUES (?1, ?2, ?3, 'recording', '{}')",
            params![title, started_at, your_name],
        )?;
        // Read it here, before anything else is inserted. `last_insert_rowid` is per connection,
        // not per table, so the INSERT into `speakers` below overwrites it — returning that
        // instead handed the caller a speakers rowid and called it a meeting id. The two happened
        // to agree for as long as every meeting had exactly one speaker, and stopped agreeing the
        // moment speaker detection produced a second one: meetings were then recorded against ids
        // that were never in `meetings` at all, the real rows stayed stuck on "recording", and
        // opening one reported "Meeting 21 not found".
        let meeting_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT OR REPLACE INTO speakers (meeting_id, speaker_id, display_name)
             VALUES (?1, 'you', ?2)",
            params![meeting_id, your_name],
        )?;
        Ok(meeting_id)
    }

    pub fn set_status(&self, id: i64, status: MeetingStatus) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE meetings SET status = ?1 WHERE id = ?2",
            params![status_str(status), id],
        )?;
        Ok(())
    }

    pub fn finish_meeting(&self, id: i64, ended_at: i64, notes: &MeetingNotes) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE meetings SET ended_at = ?1, status = 'done', notes_json = ?2 WHERE id = ?3",
            params![ended_at, serde_json::to_string(notes)?, id],
        )?;
        Ok(())
    }

    pub fn insert_utterance(
        &self,
        meeting_id: i64,
        speaker_id: &str,
        speaker_name: &str,
        source: AudioSource,
        start_ms: i64,
        end_ms: i64,
        text: &str,
    ) -> Result<i64> {
        let conn = self.connect()?;
        conn.execute(
            "INSERT INTO utterances (meeting_id, speaker_id, speaker_name, source, start_ms, end_ms, text)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                meeting_id,
                speaker_id,
                speaker_name,
                source_str(source),
                start_ms,
                end_ms,
                text
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn upsert_speaker(&self, meeting_id: i64, speaker_id: &str, display_name: &str) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "INSERT INTO speakers (meeting_id, speaker_id, display_name)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(meeting_id, speaker_id) DO UPDATE SET display_name = excluded.display_name",
            params![meeting_id, speaker_id, display_name],
        )?;
        Ok(())
    }

    pub fn rename_speaker(&self, meeting_id: i64, speaker_id: &str, display_name: &str) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE speakers SET display_name = ?1 WHERE meeting_id = ?2 AND speaker_id = ?3",
            params![display_name, meeting_id, speaker_id],
        )?;
        conn.execute(
            "UPDATE utterances SET speaker_name = ?1 WHERE meeting_id = ?2 AND speaker_id = ?3",
            params![display_name, meeting_id, speaker_id],
        )?;
        if speaker_id == "you" {
            conn.execute(
                "UPDATE meetings SET your_name = ?1 WHERE id = ?2",
                params![display_name, meeting_id],
            )?;
        }
        Ok(())
    }

    pub fn update_notes(&self, id: i64, notes: &MeetingNotes) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE meetings SET notes_json = ?1 WHERE id = ?2",
            params![serde_json::to_string(notes)?, id],
        )?;
        Ok(())
    }

    pub fn list_meetings(&self) -> Result<Vec<MeetingListItem>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT m.id, m.title, m.started_at, m.ended_at, m.status,
                    (SELECT COUNT(*) FROM utterances u WHERE u.meeting_id = m.id)
             FROM meetings m
             ORDER BY m.started_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(MeetingListItem {
                id: row.get(0)?,
                title: row.get(1)?,
                started_at: row.get(2)?,
                ended_at: row.get(3)?,
                status: parse_status(&row.get::<_, String>(4)?),
                utterance_count: row.get(5)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn get_meeting(&self, id: i64) -> Result<MeetingRecord> {
        let conn = self.connect()?;
        let (title, started_at, ended_at, your_name, status, notes_json): (
            String,
            i64,
            Option<i64>,
            String,
            String,
            String,
        ) = conn
            .query_row(
                "SELECT title, started_at, ended_at, your_name, status, notes_json FROM meetings WHERE id = ?1",
                [id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .map_err(|_| anyhow!("Meeting {id} not found"))?;

        let mut speaker_stmt = conn.prepare(
            "SELECT speaker_id, display_name FROM speakers WHERE meeting_id = ?1",
        )?;
        let speakers = speaker_stmt
            .query_map([id], |row| {
                Ok(MeetingSpeaker {
                    speaker_id: row.get(0)?,
                    display_name: row.get(1)?,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();

        let mut utt_stmt = conn.prepare(
            "SELECT id, speaker_id, speaker_name, source, start_ms, end_ms, text
             FROM utterances WHERE meeting_id = ?1 ORDER BY start_ms, id",
        )?;
        let utterances = utt_stmt
            .query_map([id], |row| {
                Ok(MeetingUtterance {
                    id: row.get(0)?,
                    meeting_id: id,
                    speaker_id: row.get(1)?,
                    speaker_name: row.get(2)?,
                    source: parse_source(&row.get::<_, String>(3)?),
                    start_ms: row.get(4)?,
                    end_ms: row.get(5)?,
                    text: row.get(6)?,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();

        Ok(MeetingRecord {
            id,
            title,
            started_at,
            ended_at,
            your_name,
            status: parse_status(&status),
            notes: parse_notes(&notes_json),
            speakers,
            utterances,
        })
    }

    pub fn delete_meeting(&self, id: i64) -> Result<()> {
        let conn = self.connect()?;
        conn.execute("DELETE FROM utterances WHERE meeting_id = ?1", [id])?;
        conn.execute("DELETE FROM speakers WHERE meeting_id = ?1", [id])?;
        conn.execute("DELETE FROM meetings WHERE id = ?1", [id])?;
        Ok(())
    }
}

fn status_str(status: MeetingStatus) -> &'static str {
    match status {
        MeetingStatus::Recording => "recording",
        MeetingStatus::Processing => "processing",
        MeetingStatus::Done => "done",
    }
}

fn parse_status(value: &str) -> MeetingStatus {
    match value {
        "recording" => MeetingStatus::Recording,
        "processing" => MeetingStatus::Processing,
        _ => MeetingStatus::Done,
    }
}

fn source_str(source: AudioSource) -> &'static str {
    match source {
        AudioSource::Microphone => "microphone",
        AudioSource::System => "system",
    }
}

fn parse_source(value: &str) -> AudioSource {
    match value {
        "system" => AudioSource::System,
        _ => AudioSource::Microphone,
    }
}

fn parse_notes(json: &str) -> MeetingNotes {
    serde_json::from_str::<Value>(json)
        .ok()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meeting::types::AudioSource;

    fn store() -> (MeetingStore, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "handy-store-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let store = MeetingStore::new(dir.clone()).expect("store");
        store.init().expect("schema");
        (store, dir)
    }

    /// The id handed back must name a row in `meetings`.
    ///
    /// This passed for as long as every meeting had exactly one speaker, because the meetings id
    /// and the speakers rowid then advanced in lockstep. The second speaker breaks the tie — so
    /// the first meeting here is given extra speakers before the second one is created.
    #[test]
    fn create_meeting_returns_a_meeting_id_not_a_speakers_rowid() {
        let (store, dir) = store();

        let first = store.create_meeting("first", "You", 1).expect("first");
        // Speaker detection finding more voices is what knocked the two counters out of step.
        for speaker in ["spk_2", "spk_3", "spk_4"] {
            store.upsert_speaker(first, speaker, "Speaker").expect("speaker");
        }

        let second = store.create_meeting("second", "You", 2).expect("second");
        assert_eq!(
            second,
            first + 1,
            "ids should advance by one; got {second} after {first}"
        );

        // The real symptom: looking the meeting up again.
        let record = store.get_meeting(second);
        assert!(
            record.is_ok(),
            "the id create_meeting returned does not name a meeting: {:?}",
            record.err()
        );
        assert_eq!(record.unwrap().title, "second");

        let _ = std::fs::remove_dir_all(dir);
    }

    /// A meeting's utterances have to land on the meeting the caller was given.
    #[test]
    fn utterances_land_on_the_meeting_that_was_created() {
        let (store, dir) = store();
        let first = store.create_meeting("first", "You", 1).expect("first");
        store.upsert_speaker(first, "spk_2", "Speaker 2").expect("speaker");
        let second = store.create_meeting("second", "You", 2).expect("second");

        store
            .insert_utterance(second, "you", "You", AudioSource::Microphone, 0, 1000, "hello")
            .expect("utterance");

        let record = store.get_meeting(second).expect("meeting should exist");
        assert_eq!(record.utterances.len(), 1);
        assert_eq!(record.utterances[0].text, "hello");

        let _ = std::fs::remove_dir_all(dir);
    }
}

