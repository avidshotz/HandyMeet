use super::types::{MeetingNotes, MeetingUtterance};
use chrono::{TimeZone, Utc};

pub fn empty_notes() -> MeetingNotes {
    MeetingNotes::default()
}

pub fn inbox_placeholder(path: &str) -> MeetingNotes {
    MeetingNotes {
        summary: format!(
            "Transcript saved to Head Secretary inbox. Summary, action items, and decisions are generated there (or in Note Catcher), not in Handy.\n\n{path}"
        ),
        action_items: Vec::new(),
        decisions: Vec::new(),
    }
}

pub fn to_markdown(
    title: &str,
    your_name: &str,
    started_at: i64,
    utterances: &[MeetingUtterance],
    speakers: &[String],
) -> String {
    let mut lines = Vec::new();
    lines.push(format!("# {title}"));
    lines.push(String::new());
    lines.push(format!("**Date:** {}", format_meeting_datetime(started_at)));
    lines.push("**Account:** HandyMeet".to_string());
    lines.push(format!("**Organizer:** {your_name}"));
    if !speakers.is_empty() {
        lines.push(format!("**Participants:** {}", speakers.join(", ")));
    }
    if let Some(duration) = format_duration(utterances) {
        lines.push(format!("**Duration:** {duration}"));
    }
    lines.push(String::new());
    lines.push("---".to_string());
    lines.push(String::new());
    lines.push("## Transcript".to_string());
    lines.push(String::new());
    for utterance in utterances {
        if utterance.text.trim().is_empty() {
            continue;
        }
        lines.push(format!(
            "**{}** [{}]: {}",
            utterance.speaker_name,
            format_ts(utterance.start_ms),
            utterance.text.trim()
        ));
    }
    lines.push(String::new());
    lines.join("\n")
}

pub fn stash_filename(started_at: i64, title: &str) -> String {
    let local = chrono::Local
        .timestamp_millis_opt(started_at)
        .single()
        .unwrap_or_else(chrono::Local::now);
    format!(
        "{}_{}_{}.md",
        local.format("%Y-%m-%d"),
        local.format("%H%M"),
        slugify_title(title)
    )
}

fn slugify_title(title: &str) -> String {
    let mut slug = String::new();
    let mut prev_dash = false;
    for ch in title.trim().chars() {
        if matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' )
            || ch.is_control()
        {
            continue;
        }
        if ch.is_whitespace() {
            if !prev_dash && !slug.is_empty() {
                slug.push('-');
                prev_dash = true;
            }
            continue;
        }
        slug.push(ch);
        prev_dash = false;
    }
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        "Meeting".to_string()
    } else {
        slug.chars().take(80).collect()
    }
}

fn format_meeting_datetime(started_at: i64) -> String {
    let local = chrono::Local
        .timestamp_millis_opt(started_at)
        .single()
        .unwrap_or_else(chrono::Local::now);
    let utc = local.with_timezone(&Utc);
    let month_day = local.format("%B %e, %Y").to_string().replace("  ", " ");
    let time_local = local
        .format("%I:%M %p")
        .to_string()
        .trim_start_matches('0')
        .to_string();
    let tz = local.format("%Z").to_string();
    format!(
        "{month_day} — {time_local} {tz} ({} UTC)",
        utc.format("%H:%M")
    )
}

fn format_duration(utterances: &[MeetingUtterance]) -> Option<String> {
    let max = utterances.iter().map(|u| u.end_ms).max().unwrap_or(0);
    if max <= 0 {
        return None;
    }
    let minutes = (max as f64 / 60_000.0).ceil() as i64;
    Some(format!("{minutes} minutes"))
}

fn format_ts(ms: i64) -> String {
    let total = ms.max(0) / 1000;
    format!("{:02}:{:02}", total / 60, total % 60)
}
