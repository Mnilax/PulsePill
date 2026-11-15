//! Shared data contracts between the data layers and the UI, plus text/time formatting.

use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AgentState {
    Running,
    NeedsInput,
    Completed,
    Failed,
    Idle,
}

#[derive(Clone, PartialEq, Debug)]
pub struct AgentThread {
    pub id: String,
    pub title: String,
    pub provider: String,
    pub model: Option<String>,
    pub project_name: Option<String>,
    pub state: AgentState,
    /// Unix milliseconds.
    pub updated_at: i64,
    pub run_started_at: Option<i64>,
    pub last_message_preview: Option<String>,
    pub pending_request_summary: Option<String>,
    pub subagent_count: i64,
    pub is_subagent: bool,
}

#[derive(Clone, PartialEq, Debug, Default)]
pub struct AgentsSnapshot {
    pub threads: Vec<AgentThread>,
    pub running_count: usize,
    pub needs_input_count: usize,
    pub server_online: bool,
    pub last_error: Option<String>,
    pub environment_id: Option<String>,
}

#[derive(Clone, PartialEq, Debug, Default)]
pub struct SystemSnapshot {
    pub cpu_usage: f64,
    pub cpu_history: Vec<f64>,
    pub gpu_usage: Option<f64>,
    pub gpu_history: Vec<f64>,
    pub memory_used: u64,
    pub memory_total: u64,
    pub memory_load: f64,
    pub battery_level: Option<f64>,
    pub is_charging: bool,
    pub is_plugged_in: bool,
    pub battery_minutes: Option<u32>,
    pub net_down: f64,
    pub net_up: f64,
    pub self_memory: u64,
}

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Parses T3's ISO-8601 timestamps ("2026-11-15T10:05:56.657Z", offsets allowed) to Unix ms.
pub fn parse_iso(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || (b[10] != b'T' && b[10] != b' ') {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> { s.get(r)?.parse().ok() };
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let mut i = 19;
    let mut ms = 0i64;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let mut scale = 100;
        while i < b.len() && b[i].is_ascii_digit() {
            ms += (b[i] - b'0') as i64 * scale;
            scale /= 10;
            i += 1;
        }
    }
    let mut offset_min = 0i64;
    if let Some(&c) = b.get(i) {
        if c == b'+' || c == b'-' {
            let oh = num(i + 1..i + 3)?;
            let om = num(i + 4..i + 6).or_else(|| num(i + 3..i + 5)).unwrap_or(0);
            offset_min = (oh * 60 + om) * if c == b'+' { 1 } else { -1 };
        }
    }
    // Days from civil (Howard Hinnant).
    let yy = if mo <= 2 { y - 1 } else { y };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 86_400 + h * 3600 + mi * 60 + sec - offset_min * 60) * 1000) + ms)
}

// MARK: - Formatting

pub fn fmt_percent(v: f64) -> String {
    format!("{}%", (v * 100.0).round() as i64)
}

pub fn fmt_gb(bytes: u64, decimals: usize) -> String {
    format!("{:.*}", decimals, bytes as f64 / 1_073_741_824.0)
}

pub fn fmt_rate(bps: f64) -> String {
    let kb = bps.max(0.0) / 1024.0;
    if kb < 1000.0 {
        return format!("{} KB/s", kb.round() as i64);
    }
    let mb = kb / 1024.0;
    if mb < 10.0 {
        format!("{mb:.1} MB/s")
    } else if mb < 1000.0 {
        format!("{} MB/s", mb.round() as i64)
    } else {
        format!("{:.1} GB/s", mb / 1024.0)
    }
}

pub fn fmt_duration(minutes: u32) -> String {
    let (h, m) = (minutes / 60, minutes % 60);
    match (h, m) {
        (0, m) => format!("{m}m"),
        (h, 0) => format!("{h}h"),
        (h, m) => format!("{h}h {m}m"),
    }
}

pub fn fmt_relative(at: i64, now: i64) -> String {
    let s = ((now - at).max(0)) / 1000;
    match s {
        0..=59 => "now".into(),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

pub fn fmt_elapsed(since: i64, now: i64) -> String {
    let s = ((now - since).max(0)) / 1000;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else {
        fmt_duration((s / 60) as u32)
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// "claude-opus-5-5" → "Opus 5.5", "gpt-5.2-codex" → "GPT-5.2 Codex"; falls back to provider.
pub fn model_chip(provider: &str, model: Option<&str>) -> String {
    if let Some(model) = model.filter(|m| !m.is_empty()) {
        let m = model.to_lowercase();
        if let Some(rest) = m.strip_prefix("claude-") {
            let parts: Vec<&str> = rest.split('-').collect();
            let family = capitalize(parts[0]);
            let nums: Vec<&str> = parts[1..].iter().copied().filter(|p| p.len() <= 2 && p.parse::<u32>().is_ok()).take(2).collect();
            return if nums.is_empty() { family } else { format!("{family} {}", nums.join(".")) };
        }
        if let Some(rest) = m.strip_prefix("gpt-") {
            let parts: Vec<&str> = rest.split('-').collect();
            let head = format!("GPT-{}", parts[0]);
            let tail: Vec<String> = parts[1..].iter().map(|p| capitalize(p)).collect();
            return if tail.is_empty() { head } else { format!("{head} {}", tail.join(" ")) };
        }
        return model.to_string();
    }
    match provider.to_lowercase().as_str() {
        "claudeagent" | "claude" | "claudecode" => "Claude".into(),
        "codex" => "Codex".into(),
        "gemini" => "Gemini".into(),
        "" => "Agent".into(),
        _ => capitalize(provider),
    }
}

// MARK: - Preview text

const ATTACHMENT_PLACEHOLDER: &str =
    "[User attached one or more files without additional text. Respond using the conversation context and the attached files.]";

/// Replaces `[text](url)` with `text` and `![alt](url)` with `alt`.
fn strip_links(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let start = if chars[i] == '!' && chars.get(i + 1) == Some(&'[') { i + 1 } else { i };
        if chars[start] == '[' {
            if let Some(close) = chars[start + 1..].iter().position(|&c| c == ']').map(|p| p + start + 1) {
                if chars.get(close + 1) == Some(&'(') {
                    if let Some(end) = chars[close + 2..].iter().position(|&c| c == ')').map(|p| p + close + 2) {
                        out.extend(&chars[start + 1..close]);
                        i = end + 1;
                        continue;
                    }
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn strip_line_prefix(line: &str) -> &str {
    let t = line.trim_start();
    let hashes = t.chars().take_while(|&c| c == '#').count();
    if (1..=6).contains(&hashes) && t[hashes..].starts_with([' ', '\t']) {
        return t[hashes..].trim_start();
    }
    if let Some(r) = t.strip_prefix('>') {
        return r.trim_start();
    }
    for m in ["- ", "* ", "+ "] {
        if let Some(r) = t.strip_prefix(m) {
            let r = r.trim_start();
            for cb in ["[ ] ", "[x] ", "[X] "] {
                if let Some(rr) = r.strip_prefix(cb) {
                    return rr.trim_start();
                }
            }
            return r;
        }
    }
    let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
    if (1..=3).contains(&digits) && (t[digits..].starts_with(". ") || t[digits..].starts_with(") ")) {
        return t[digits + 2..].trim_start();
    }
    t
}

fn is_rule(line: &str) -> bool {
    let t: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    t.len() >= 3 && (t.chars().all(|c| c == '-') || t.chars().all(|c| c == '*') || t.chars().all(|c| c == '_'))
}

/// Single line, whitespace collapsed, rough markdown stripped, at most `limit` characters.
pub fn preview(raw: &str, limit: usize) -> Option<String> {
    let mut text = raw.trim();
    if text.is_empty() {
        return None;
    }
    if text == ATTACHMENT_PLACEHOLDER {
        return Some("(attached files)".into());
    }
    if text.get(..11).is_some_and(|p| p.eq_ignore_ascii_case("ultrathink:")) {
        text = &text[11..];
    }
    let mut joined = String::with_capacity(text.len());
    for line in text.lines() {
        let lt = line.trim_start();
        if lt.starts_with("```") || lt.starts_with("~~~") || is_rule(line) {
            joined.push(' ');
            continue;
        }
        joined.push_str(strip_line_prefix(line));
        joined.push(' ');
    }
    let joined = strip_links(&joined).replace("**", "").replace("__", "").replace("~~", "").replace('`', "");
    let collapsed = joined.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }
    Some(truncate(&collapsed, limit))
}

pub fn truncate(text: &str, limit: usize) -> String {
    let count = text.chars().count();
    if count <= limit || limit <= 1 {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(limit - 1).collect();
    if let Some(space) = cut.rfind(' ') {
        if cut[..space].chars().count() > limit * 3 / 4 {
            cut.truncate(space);
        }
    }
    format!("{}…", cut.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso() {
        assert_eq!(parse_iso("1970-01-01T00:00:01.5Z"), Some(1500));
        assert_eq!(parse_iso("2026-11-15T10:05:56.657Z"), Some(1_794_737_156_657));
        assert_eq!(parse_iso("1970-01-01T01:00:00+01:00"), Some(0));
    }

    #[test]
    fn chips() {
        assert_eq!(model_chip("", Some("claude-opus-5-5")), "Opus 5.5");
        assert_eq!(model_chip("", Some("claude-sonnet-4-5-20250929")), "Sonnet 4.5");
        assert_eq!(model_chip("", Some("gpt-5.2-codex")), "GPT-5.2 Codex");
        assert_eq!(model_chip("claudeAgent", None), "Claude");
    }

    #[test]
    fn previews() {
        assert_eq!(preview("## Title\n- **bold** [link](http://x) `code`", 140).unwrap(), "Title bold link code");
        assert_eq!(preview("```rust\nfn x()\n```", 140).unwrap(), "fn x()");
        assert!(preview("abcdefghij klmnop", 10).unwrap().ends_with('…'));
        assert_eq!(preview("привет, мир", 140).unwrap(), "привет, мир");
        assert_eq!(preview("# Заголовок\n1. пункт", 140).unwrap(), "Заголовок пункт");
    }
}
