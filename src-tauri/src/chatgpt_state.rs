//! Read the ChatGPT/Codex Desktop state that the GUI itself persists.
//!
//! ChatGPT Desktop currently stores the selected reasoning effort in the
//! Codex state database. Keep this adapter read-only: changing the database
//! directly would bypass the app's own state and validation.

use std::{
    collections::HashMap,
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    process::Command,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const ENCODER_SECTION: &str = "[desktop.codex-micro-layout]";

#[derive(Debug, Clone)]
pub struct LocalSession {
    pub id: String,
    pub title: String,
    pub cwd: String,
    /// When the thread last received content, in epoch milliseconds. The
    /// desktop's own clock, so a row discovered at launch keeps its real age.
    pub last_activity_ms: Option<u128>,
}

#[derive(Debug)]
pub struct LocalUsage {
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub remaining_5h_percent: Option<u8>,
    pub remaining_weekly_percent: Option<u8>,
    pub reset_5h_seconds: Option<u32>,
    pub reset_5h_time: Option<String>,
    pub reset_weekly_time: Option<String>,
    /// When the quota record itself was written, so the deck can show how stale
    /// this fallback is. Zero when no quota record exists.
    pub snapshot_at_ms: u128,
}

pub fn current_reasoning_level() -> Option<u8> {
    let home = std::env::var_os("HOME")?;
    let database = std::path::PathBuf::from(home).join(".codex/state_5.sqlite");
    let query = "SELECT reasoning_effort FROM threads WHERE archived = 0 AND preview <> '' AND reasoning_effort IS NOT NULL AND agent_path IS NULL ORDER BY COALESCE(updated_at_ms, updated_at * 1000) DESC LIMIT 1;";
    let output = Command::new("sqlite3")
        .args(["-readonly", "-noheader", database.to_str()?, query])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    match String::from_utf8_lossy(&output.stdout)
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "low" => Some(0),
        "medium" => Some(1),
        "high" => Some(2),
        "xhigh" | "extrhigh" => Some(3),
        "ultra" => Some(4),
        _ => None,
    }
}

pub fn level_name(level: u8) -> &'static str {
    match level {
        0 => "low",
        1 => "medium",
        2 => "high",
        _ => "extrhigh",
    }
}

pub fn encoder_mode() -> Option<String> {
    let home = std::env::var_os("HOME")?;
    let config =
        std::fs::read_to_string(std::path::PathBuf::from(home).join(".codex/config.toml")).ok()?;
    parse_encoder_mode(&config).map(str::to_owned)
}

/// Facts extracted from one rollout JSONL file.
#[derive(Clone, Default)]
struct RolloutFacts {
    usage: Option<(u64, u64)>,
    rate: Option<RateFacts>,
    activity: Option<crate::state::AgentStatus>,
    activity_at_ms: u128,
    output_at_ms: u128,
    submitted_at_ms: u128,
    completed_at_ms: u128,
    output_signature: Option<Vec<u8>>,
}

#[derive(Clone)]
struct RateFacts {
    timestamp: String,
    remaining_5h_percent: Option<u8>,
    remaining_weekly_percent: Option<u8>,
    reset_5h_at: Option<u64>,
    reset_weekly_at: Option<u64>,
}

struct CachedRollout {
    mtime: SystemTime,
    size: u64,
    offset: u64,
    facts: RolloutFacts,
}

fn rollout_cache() -> &'static Mutex<HashMap<String, CachedRollout>> {
    static CACHE: OnceLock<Mutex<HashMap<String, CachedRollout>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Reads the latest locally persisted model and token counters. This is a
/// read-only best-effort adapter for the Codex/ChatGPT desktop state. Quota
/// read from disk is a historical snapshot; `chatgpt_quota` is the live source
/// and this one only fills in when the app server is unavailable.
pub fn local_usage() -> Option<LocalUsage> {
    let home = std::env::var_os("HOME")?;
    let database = std::path::PathBuf::from(&home).join(".codex/state_5.sqlite");
    let query = "SELECT model, rollout_path FROM threads WHERE archived = 0 AND preview <> '' AND model IS NOT NULL AND rollout_path IS NOT NULL ORDER BY COALESCE(updated_at_ms, updated_at * 1000) DESC;";
    let output = Command::new("sqlite3")
        .args([
            "-readonly",
            "-separator",
            "\t",
            "-noheader",
            database.to_str().unwrap_or_default(),
            query,
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let rows = String::from_utf8_lossy(&output.stdout);
    let model = rows.lines().next()?.split_once('\t')?.0.to_owned();
    let mut input_tokens = 0u64;
    let mut output_tokens = 0u64;
    let mut best_rate = None::<RateFacts>;
    for row in rows.lines() {
        let Some((row_model, rollout_path)) = row.split_once('\t') else {
            continue;
        };
        if row_model != model {
            continue;
        }
        let Some(facts) = scan_rollout(rollout_path) else {
            continue;
        };
        if let Some((input, output)) = facts.usage {
            input_tokens = input_tokens.saturating_add(input);
            output_tokens = output_tokens.saturating_add(output);
        }
        if let Some(rate) = facts.rate {
            if best_rate
                .as_ref()
                .is_none_or(|best| rate.timestamp >= best.timestamp)
            {
                best_rate = Some(rate);
            }
        }
    }

    let mut remaining_5h_percent = None;
    let mut remaining_weekly_percent = None;
    let mut reset_5h_seconds = None;
    let mut reset_5h_time = None;
    let mut reset_weekly_time = None;
    let mut snapshot_at_ms = 0;
    if let Some(rate) = best_rate {
        snapshot_at_ms = parse_rfc3339_millis(&rate.timestamp).unwrap_or(0);
        let now = unix_seconds();
        // A stored window that has already rolled over is fully available
        // again, and its recorded reset instant must not be shown as if it were
        // still upcoming. This is why the deck used to display a 5H reset time
        // that the desktop never showed.
        let five_h_expired = rate.reset_5h_at.is_some_and(|reset| reset <= now);
        remaining_5h_percent = if five_h_expired {
            Some(100)
        } else {
            rate.remaining_5h_percent
        };
        reset_5h_seconds = (!five_h_expired)
            .then(|| rate.reset_5h_at)
            .flatten()
            .map(|reset| reset.saturating_sub(now).min(u32::MAX as u64) as u32);
        reset_5h_time = (!five_h_expired)
            .then(|| rate.reset_5h_at)
            .flatten()
            .and_then(reset_time);
        let weekly_expired = rate.reset_weekly_at.is_some_and(|reset| reset <= now);
        remaining_weekly_percent = if weekly_expired {
            Some(100)
        } else {
            rate.remaining_weekly_percent
        };
        reset_weekly_time = (!weekly_expired)
            .then(|| rate.reset_weekly_at)
            .flatten()
            .and_then(reset_date);
    }
    Some(LocalUsage {
        model,
        input_tokens,
        output_tokens,
        remaining_5h_percent,
        remaining_weekly_percent,
        reset_5h_seconds,
        reset_5h_time,
        reset_weekly_time,
        snapshot_at_ms,
    })
}

/// Scans one append-only rollout log for the latest token counters and rate
/// limits. Parse state is cached per file, so a poll only re-reads the bytes
/// appended since the previous scan instead of re-parsing the whole file.
fn scan_rollout(path: &str) -> Option<RolloutFacts> {
    let metadata = std::fs::metadata(path).ok()?;
    let mtime = metadata.modified().ok()?;
    let size = metadata.len();
    let mut cache = rollout_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(entry) = cache.get(path) {
        if entry.mtime == mtime && entry.size == size {
            return Some(entry.facts.clone());
        }
    }
    let previous = cache.remove(path);
    let (start, mut facts) = match &previous {
        Some(entry) if size >= entry.offset => (entry.offset, entry.facts.clone()),
        _ => (0, RolloutFacts::default()),
    };
    let scanned = read_and_scan(path, start, &mut facts);
    if scanned.is_err() {
        return Some(previous.map(|entry| entry.facts)?);
    }
    let complete = scanned.ok()?;
    cache.insert(
        path.to_owned(),
        CachedRollout {
            mtime,
            size,
            offset: start + complete,
            facts: facts.clone(),
        },
    );
    Some(facts)
}

/// Parses rollout lines from `start` onward, updating `facts`; returns the
/// number of bytes consumed (a trailing partial line is left for the next
/// scan once the writer completes it).
fn read_and_scan(path: &str, start: u64, facts: &mut RolloutFacts) -> std::io::Result<u64> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer)?;
    let complete = buffer
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    for line in BufReader::new(&buffer[..complete])
        .lines()
        .map_while(Result::ok)
    {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        update_io_facts(&value, facts);
        if let Some(status) = rollout_status(&value) {
            facts.activity = Some(status);
            facts.activity_at_ms = value
                .get("timestamp")
                .and_then(|v| v.as_str())
                .and_then(parse_rfc3339_millis)
                .unwrap_or(0);
        }
        if value
            .get("payload")
            .and_then(|p| p.get("type"))
            .and_then(|v| v.as_str())
            != Some("token_count")
        {
            continue;
        }
        if let Some(usage) = value.pointer("/payload/info/total_token_usage") {
            facts.usage = Some((
                usage
                    .get("input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                usage
                    .get("output_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
            ));
        }
        if value.pointer("/payload/rate_limits").is_none() {
            continue;
        }
        let timestamp = value
            .get("timestamp")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if facts
            .rate
            .as_ref()
            .is_some_and(|best| best.timestamp.as_str() > timestamp)
        {
            continue;
        }
        let primary = value.pointer("/payload/rate_limits/primary");
        let secondary = value.pointer("/payload/rate_limits/secondary");
        facts.rate = Some(RateFacts {
            timestamp: timestamp.to_owned(),
            remaining_5h_percent: primary
                .and_then(|limit| limit.get("used_percent"))
                .and_then(|value| value.as_f64())
                .map(remaining_percent),
            remaining_weekly_percent: secondary
                .and_then(|limit| limit.get("used_percent"))
                .and_then(|value| value.as_f64())
                .map(remaining_percent),
            reset_5h_at: primary
                .and_then(|limit| limit.get("resets_at"))
                .and_then(|value| value.as_u64()),
            reset_weekly_at: secondary
                .and_then(|limit| limit.get("resets_at"))
                .and_then(|value| value.as_u64()),
        });
    }
    Ok(complete as u64)
}

fn remaining_percent(used: f64) -> u8 {
    (100.0 - used).clamp(0.0, 100.0).round() as u8
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// Epoch milliseconds for the rollout records' RFC 3339 timestamps, so the
/// deck can report how old this disk-only snapshot is.
fn parse_rfc3339_millis(value: &str) -> Option<u128> {
    let (date, clock) = value.split_once('T')?;
    let mut days = date.split('-');
    let year: i64 = days.next()?.parse().ok()?;
    let month: u32 = days.next()?.parse().ok()?;
    let day: u32 = days.next()?.parse().ok()?;
    let (clock, zone) = match clock.find(['Z', '+', '-']) {
        Some(index) => (&clock[..index], &clock[index..]),
        None => (clock, "Z"),
    };
    let mut parts = clock.split(':');
    let hour: u32 = parts.next()?.parse().ok()?;
    let minute: u32 = parts.next()?.parse().ok()?;
    let stamp = parts.next()?;
    let (second, fraction) = match stamp.split_once('.') {
        Some((second, fraction)) => (second, fraction),
        None => (stamp, ""),
    };
    let second: u32 = second.parse().ok()?;
    let millis: u32 = format!("{:0<3}", &fraction[..fraction.len().min(3)])
        .parse()
        .ok()?;
    let offset_seconds = match *zone.as_bytes().first()? {
        b'Z' => 0,
        b'+' | b'-' => {
            let hours: i64 = zone.get(1..3)?.parse().ok()?;
            let minutes: i64 = zone.get(4..6)?.parse().ok()?;
            let value = hours * 3_600 + minutes * 60;
            if zone.starts_with('-') {
                -value
            } else {
                value
            }
        }
        _ => return None,
    };
    let seconds = days_from_civil(year, month, day) as i128 * 86_400
        + hour as i128 * 3_600
        + minute as i128 * 60
        + second as i128
        - offset_seconds as i128;
    Some((seconds * 1_000 + millis as i128).max(0) as u128)
}

// Inverse of `civil_from_days`: day count relative to 1970-01-01.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted_month = month as i64 + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day as i64 - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    day_of_era + era * 146_097 - 719_468
}

/// Seconds east of local time, refreshed rarely so formatting reset times
/// never spawns a subprocess on the poll path.
fn local_utc_offset_seconds() -> Option<i64> {
    static CACHE: OnceLock<Mutex<(Option<i64>, Instant)>> = OnceLock::new();
    let cache =
        CACHE.get_or_init(|| Mutex::new((None, Instant::now() - Duration::from_secs(24 * 3600))));
    let mut guard = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.1.elapsed() < Duration::from_secs(6 * 3600) {
        return guard.0;
    }
    let stale = guard.0;
    if let Some(value) = utc_offset_from_system() {
        *guard = (Some(value), Instant::now());
        return Some(value);
    }
    guard.0 = stale;
    guard.1 = Instant::now();
    stale
}

fn utc_offset_from_system() -> Option<i64> {
    let output = Command::new("date").arg("+%z").output().ok()?;
    if !output.status.success() {
        return None;
    }
    // e.g. "+0800" or "-0530"
    let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let sign = match value.as_bytes().first()? {
        b'-' => -1i64,
        b'+' => 1i64,
        _ => return None,
    };
    let hours: i64 = value.get(1..3)?.parse().ok()?;
    let minutes: i64 = value.get(3..5)?.parse().ok()?;
    Some(sign * (hours * 3600 + minutes * 60))
}

pub(crate) fn reset_time(reset_at: u64) -> Option<String> {
    let seconds = local_reset_seconds(reset_at)?.rem_euclid(86_400);
    Some(format!(
        "{:02}:{:02}",
        seconds / 3_600,
        (seconds % 3_600) / 60
    ))
}

fn local_reset_seconds(reset_at: u64) -> Option<i64> {
    Some(reset_at as i64 + local_utc_offset_seconds()?)
}

/// Weekly reset is a date, not a clock time. Fits the six-character wire
/// field exactly (`Sep 25`).
pub(crate) fn reset_date(reset_at: u64) -> Option<String> {
    let days = local_reset_seconds(reset_at)?.div_euclid(86_400);
    let (_, month, day) = civil_from_days(days);
    let month_name = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .get(month as usize - 1)?;
    Some(format!("{month_name} {day:02}"))
}

// Gregorian calendar conversion from a day count relative to 1970-01-01.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if month <= 2 { 1 } else { 0 };
    (year, month as u32, day as u32)
}

/// The UI polls state every ~1.5s; the sqlite lookup is cached briefly so a
/// poll does not spawn a subprocess unless the recent list may have changed.
pub fn recent_sessions(limit: usize) -> Vec<LocalSession> {
    static CACHE: OnceLock<Mutex<HashMap<usize, (Instant, Vec<LocalSession>)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((refreshed_at, sessions)) = guard.get(&limit) {
        if refreshed_at.elapsed() < Duration::from_secs(2) {
            return sessions.clone();
        }
    }
    let sessions = query_recent_sessions(limit);
    guard.insert(limit, (Instant::now(), sessions.clone()));
    sessions
}

fn query_recent_sessions(limit: usize) -> Vec<LocalSession> {
    let Some(home) = std::env::var_os("HOME") else {
        return Vec::new();
    };
    let database = std::path::PathBuf::from(home).join(".codex/state_5.sqlite");
    // recency_at_ms tracks selection/navigation recency, while updated_at_ms
    // only changes when the thread receives new content.
    let query = format!(
        "SELECT id, COALESCE(NULLIF(name, ''), NULLIF(title, ''), preview), cwd, COALESCE(NULLIF(updated_at_ms, 0), updated_at * 1000) FROM threads WHERE archived = 0 AND preview <> '' AND model LIKE 'gpt-%' AND thread_source = 'user' ORDER BY recency_at_ms DESC, updated_at_ms DESC LIMIT {};",
        // The desktop projection lists more than the LCD's three rows, but a
        // disk scan still needs a bound.
        limit.clamp(1, crate::state::DESKTOP_SESSION_LIMIT.max(3))
    );
    let output = Command::new("sqlite3")
        .args([
            "-readonly",
            "-separator",
            "\t",
            "-noheader",
            database.to_str().unwrap_or_default(),
            &query,
        ])
        .output()
        .ok();
    let Some(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|row| {
            let mut fields = row.splitn(4, '\t');
            Some(LocalSession {
                id: fields.next()?.to_owned(),
                title: fields.next()?.to_owned(),
                cwd: fields.next()?.to_owned(),
                // A thread without a readable timestamp stays visible; only its
                // age goes blank.
                last_activity_ms: fields.next().and_then(|value| value.trim().parse().ok()),
            })
        })
        .collect()
}

fn parse_encoder_mode(config: &str) -> Option<&str> {
    let mut in_section = false;
    for raw_line in config.lines() {
        let line = raw_line.trim();
        if line.starts_with('[') {
            in_section = line == ENCODER_SECTION;
            continue;
        }
        if !in_section {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            if key.trim() == "encoderMode" {
                return Some(value.trim().trim_matches([' ', '\t', '"']));
            }
        }
    }
    None
}

#[cfg(target_os = "macos")]
pub fn native_encoder_available() -> bool {
    Command::new("ioreg")
        .args(["-r", "-c", "IOHIDDevice", "-l"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .is_some_and(|output| {
            String::from_utf8_lossy(&output.stdout).contains("\"Product\" = \"Codex Micro\"")
        })
}

#[cfg(not(target_os = "macos"))]
pub fn native_encoder_available() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::{
        civil_from_days, days_from_civil, level_name, parse_encoder_mode, parse_rfc3339_millis,
        read_and_scan, RolloutFacts,
    };

    #[test]
    fn levels_have_stable_names() {
        assert_eq!(level_name(0), "low");
        assert_eq!(level_name(1), "medium");
        assert_eq!(level_name(2), "high");
        assert_eq!(level_name(3), "extrhigh");
        assert_eq!(level_name(99), "extrhigh");
    }

    fn token_line(input: u64, output: u64) -> String {
        format!(
            r#"{{"timestamp":"2026-01-01T00:00:00.000Z","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":{input},"output_tokens":{output}}}}}}}}}"#
        )
    }

    #[test]
    fn rollout_scan_resumes_after_partial_line() {
        let dir = std::env::temp_dir().join(format!(
            "deck-rollout-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rollout.jsonl");

        let first = token_line(10, 2);
        let second = token_line(30, 4);
        let partial_len = second.len() / 2;
        std::fs::write(&path, format!("{first}\n{}", &second[..partial_len])).unwrap();

        let mut facts = RolloutFacts::default();
        let consumed = read_and_scan(path.to_str().unwrap(), 0, &mut facts).unwrap();
        assert_eq!(consumed as usize, first.len() + 1);
        assert_eq!(facts.usage, Some((10, 2)));

        std::fs::write(&path, format!("{first}\n{second}\n")).unwrap();
        let consumed = read_and_scan(path.to_str().unwrap(), consumed, &mut facts).unwrap();
        assert!(facts.usage.is_some());
        assert_eq!(consumed as usize, second.len() + 1);
        assert_eq!(facts.usage, Some((30, 4)));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn encoder_mode_only_reads_the_codex_micro_section() {
        let config = r#"
encoderMode = "custom"
[desktop.codex-micro-layout]
encoderMode = "reasoning"
"#;
        assert_eq!(parse_encoder_mode(config), Some("reasoning"));
        assert_eq!(
            parse_encoder_mode("model_reasoning_effort = \"high\""),
            None
        );
    }

    #[test]
    fn civil_date_conversion_is_gregorian() {
        assert_eq!(civil_from_days(20_721), (2026, 9, 25));
        assert_eq!(days_from_civil(2026, 9, 25), 20_721);
        for days in [0, 19_786, 20_000, 20_721, -300] {
            let (year, month, day) = civil_from_days(days);
            assert_eq!(days_from_civil(year, month, day), days, "day {days}");
        }
    }

    #[test]
    fn rollout_timestamps_become_epoch_milliseconds() {
        let midnight: u128 = 20_721 * 86_400 * 1_000;
        assert_eq!(
            parse_rfc3339_millis("2026-09-25T00:00:00.000Z"),
            Some(midnight)
        );
        assert_eq!(
            parse_rfc3339_millis("2026-09-25T08:00:00+08:00"),
            Some(midnight)
        );
        assert_eq!(
            parse_rfc3339_millis("2026-09-24T19:00:00-05:00"),
            Some(midnight)
        );
        assert_eq!(
            parse_rfc3339_millis("2026-09-25T00:00:00Z").map(|value| value - midnight),
            Some(0)
        );
        assert_eq!(
            parse_rfc3339_millis("2026-09-25T00:00:00.123456Z").map(|v| v - midnight),
            Some(123)
        );
        assert_eq!(parse_rfc3339_millis("not-a-timestamp"), None);
        assert_eq!(parse_rfc3339_millis(""), None);
    }
}

/// Read execution events from the same rollout the desktop writes. A separate
/// app-server process cannot observe turns running in the desktop process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActivityRecord {
    pub status: crate::state::AgentStatus,
    pub event_at_ms: u128,
    pub output_at_ms: u128,
    pub submitted_at_ms: u128,
    pub completed_at_ms: u128,
}
pub fn session_activity() -> Option<HashMap<String, ActivityRecord>> {
    // Discovery may lag by at most one second; already known rollouts still
    // receive the 500 ms incremental scan. Keep the query off every UI refresh.
    type Routes = Option<Vec<(String, String)>>;
    static ROUTES: OnceLock<Mutex<(Instant, Routes)>> = OnceLock::new();
    let cache = ROUTES.get_or_init(|| Mutex::new((Instant::now() - Duration::from_secs(2), None)));
    let mut cached = cache.lock().unwrap_or_else(|p| p.into_inner());
    if cached.0.elapsed() >= Duration::from_secs(1) {
        cached.1 = activity_routes();
        cached.0 = Instant::now();
    }
    let routes = cached.1.clone()?;
    drop(cached);
    Some(
        routes
            .into_iter()
            .filter_map(|(id, path)| {
                let facts = scan_rollout(&path)?;
                Some((
                    id.to_owned(),
                    ActivityRecord {
                        status: facts.activity?,
                        event_at_ms: facts.activity_at_ms,
                        output_at_ms: facts.output_at_ms,
                        submitted_at_ms: facts.submitted_at_ms,
                        completed_at_ms: facts.completed_at_ms,
                    },
                ))
            })
            .collect(),
    )
}

fn activity_routes() -> Option<Vec<(String, String)>> {
    let Some(home) = std::env::var_os("HOME") else {
        return None;
    };
    let database = std::path::PathBuf::from(home).join(".codex/state_5.sqlite");
    let output = Command::new("sqlite3").args(["-readonly", "-separator", "\t", "-noheader"])
        .arg(database)
        .arg("SELECT id, rollout_path FROM threads WHERE archived = 0 AND thread_source = 'user' AND model LIKE 'gpt-%' ORDER BY updated_at_ms DESC LIMIT 32;")
        .output();
    let Ok(output) = output else {
        return None;
    };
    if !output.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|row| {
                let (id, path) = row.split_once('\t')?;
                Some((id.to_owned(), path.to_owned()))
            })
            .collect(),
    )
}

fn update_io_facts(value: &serde_json::Value, facts: &mut RolloutFacts) {
    use sha2::{Digest, Sha256};
    let Some(at) = value
        .get("timestamp")
        .and_then(|v| v.as_str())
        .and_then(parse_rfc3339_millis)
    else {
        return;
    };
    if at > crate::state::now_ms() {
        return;
    }
    let Some(payload) = value.get("payload") else {
        return;
    };
    let kind = payload
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if value["type"] == "event_msg" && kind == "user_message" {
        facts.submitted_at_ms = facts.submitted_at_ms.max(at);
        facts.output_signature = None;
    }
    if value["type"] == "event_msg" && kind == "task_complete" {
        facts.completed_at_ms = facts.completed_at_ms.max(at);
    }
    let text = if value["type"] == "response_item"
        && kind == "message"
        && payload["role"] == "assistant"
    {
        payload
            .get("content")
            .and_then(|v| v.as_array())
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(|v| v.as_str()))
                    .collect::<Vec<_>>()
                    .join("")
            })
    } else if value["type"] == "event_msg" && kind == "agent_message" {
        payload
            .get("message")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    } else {
        None
    };
    if let Some(text) = text.filter(|text| !text.trim().is_empty()) {
        let hash = Sha256::digest(text.as_bytes()).to_vec();
        if facts.output_signature.as_ref() != Some(&hash) {
            facts.output_at_ms = facts.output_at_ms.max(at);
            facts.output_signature = Some(hash);
        }
    }
}

#[cfg(test)]
mod io_evidence_tests {
    use super::*;
    #[test]
    fn semantic_messages_are_separate_from_status_submissions_and_completion() {
        let mut facts = RolloutFacts::default();
        let event = |at: &str, kind: &str, payload: serde_json::Value| serde_json::json!({"timestamp":format!("2026-09-25T00:00:{at}Z"),"type":kind,"payload":payload});
        update_io_facts(
            &event(
                "00",
                "event_msg",
                serde_json::json!({"type":"task_started"}),
            ),
            &mut facts,
        );
        assert_eq!(facts.output_at_ms, 0);
        let message = serde_json::json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"New output"}]});
        update_io_facts(&event("01", "response_item", message.clone()), &mut facts);
        let first = facts.output_at_ms;
        assert!(first > 0);
        update_io_facts(&event("02", "response_item", message.clone()), &mut facts);
        assert_eq!(facts.output_at_ms, first);
        update_io_facts(
            &event(
                "03",
                "event_msg",
                serde_json::json!({"type":"user_message"}),
            ),
            &mut facts,
        );
        assert_eq!(facts.output_at_ms, first);
        assert!(facts.submitted_at_ms > first);
        update_io_facts(&event("04", "response_item", message), &mut facts);
        assert!(facts.output_at_ms > first);
        let last = facts.output_at_ms;
        update_io_facts(
            &event(
                "05",
                "event_msg",
                serde_json::json!({"type":"task_complete"}),
            ),
            &mut facts,
        );
        assert_eq!(facts.output_at_ms, last);
        assert!(facts.completed_at_ms > last);
    }
}

fn rollout_status(value: &serde_json::Value) -> Option<crate::state::AgentStatus> {
    use crate::state::AgentStatus;
    let payload = value.get("payload")?;
    let kind = payload.get("type")?.as_str()?;
    match (value.get("type")?.as_str()?, kind) {
        ("event_msg", "task_started") => Some(AgentStatus::Working),
        ("event_msg", "task_complete" | "turn_aborted") => Some(AgentStatus::Idle),
        ("event_msg", "turn_failed" | "error") => Some(AgentStatus::Error),
        ("event_msg", "permission_request" | "request_user_input") => Some(AgentStatus::NeedsInput),
        ("response_item", "function_call") => match payload.get("name").and_then(|v| v.as_str()) {
            Some(name)
                if name.ends_with("request_user_input")
                    || name.ends_with("request_user_input_async") =>
            {
                Some(AgentStatus::NeedsInput)
            }
            _ => Some(AgentStatus::Working),
        },
        (
            "response_item",
            "function_call_output" | "custom_tool_call" | "custom_tool_call_output",
        ) => Some(AgentStatus::Working),
        _ => None,
    }
}

#[cfg(test)]
mod activity_tests {
    use super::*;
    use crate::state::AgentStatus;
    #[test]
    fn desktop_turn_events_and_input_requests() {
        for (kind, status) in [
            ("task_started", AgentStatus::Working),
            ("task_complete", AgentStatus::Idle),
            ("turn_failed", AgentStatus::Error),
        ] {
            assert_eq!(
                rollout_status(&serde_json::json!({"type":"event_msg","payload":{"type":kind}})),
                Some(status)
            );
        }
        assert_eq!(
            rollout_status(
                &serde_json::json!({"type":"response_item","payload":{"type":"function_call","name":"functions.request_user_input_async"}})
            ),
            Some(AgentStatus::NeedsInput)
        );
        assert_eq!(
            rollout_status(
                &serde_json::json!({"type":"event_msg","payload":{"type":"token_count"}})
            ),
            None
        );
    }
}
