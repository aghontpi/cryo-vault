//! Nightly, local transcript discovery and archival.
//!
//! The capture pipeline intentionally has a small, conservative parser. Local
//! clients change their JSON envelopes fairly often, but the useful portion of
//! those files is remarkably consistent: a role, some content, and a session
//! identifier. Unknown envelope fields are ignored and hidden reasoning fields
//! are never copied into the archive.

use anyhow::{Context, Result, anyhow};
use chrono::DateTime;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

use crate::schema::{
    ChatGptConversation, ChatSessionV1, MessageRole, MessageV1, ToolCall, ToolOutput,
};
use crate::storage::Storage;

pub const DEFAULT_CAPTURE_TIME: &str = "23:00";
const STATE_FILE: &str = "capture-state.json";
const DUPLICATE_FILE: &str = "capture-duplicate-keys.json";
const HOOK_FILE: &str = "capture-hooks.jsonl";
const HINT_QUEUE_DIR: &str = "capture-hints";
const DEFAULT_STABLE_AGE_SECS: u64 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Platform {
    All,
    Codex,
    ClaudeCode,
    CopilotCli,
    Cursor,
    GeminiCli,
    Antigravity,
    Generic,
}

impl Platform {
    pub fn slug(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Codex => "codex",
            Self::ClaudeCode => "claude-code",
            Self::CopilotCli => "copilot-cli",
            Self::Cursor => "cursor",
            Self::GeminiCli => "gemini-cli",
            Self::Antigravity => "antigravity",
            Self::Generic => "generic",
        }
    }

    pub fn sources(self) -> Vec<Self> {
        if self == Self::All {
            vec![
                Self::Codex,
                Self::ClaudeCode,
                Self::CopilotCli,
                Self::Cursor,
                Self::GeminiCli,
                Self::Antigravity,
                Self::Generic,
            ]
        } else {
            vec![self]
        }
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.slug())
    }
}

impl FromStr for Platform {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().replace('_', "-").as_str() {
            "all" => Ok(Self::All),
            "codex" | "openai-codex" => Ok(Self::Codex),
            "claude" | "claude-code" => Ok(Self::ClaudeCode),
            "copilot" | "copilot-cli" => Ok(Self::CopilotCli),
            "cursor" => Ok(Self::Cursor),
            "gemini" | "gemini-cli" => Ok(Self::GeminiCli),
            "antigravity" => Ok(Self::Antigravity),
            "generic" | "json" => Ok(Self::Generic),
            other => Err(anyhow!("unsupported capture platform: {other}")),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CaptureOptions {
    pub platform: Platform,
    pub dry_run: bool,
    pub time: String,
    pub settle: Option<Duration>,
    /// Retained for API compatibility; stability is now based on two
    /// identical observations (or a concrete lifecycle hint).
    pub stable_age_secs: u64,
}

impl Default for CaptureOptions {
    fn default() -> Self {
        Self {
            platform: Platform::All,
            dry_run: false,
            time: DEFAULT_CAPTURE_TIME.to_string(),
            settle: None,
            stable_age_secs: DEFAULT_STABLE_AGE_SECS,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CandidateReport {
    pub platform: Platform,
    pub path: String,
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Streaming diagnostics emitted while a capture pass is running.  Events
/// intentionally contain paths and classification only; transcript bodies
/// never cross this boundary.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum CaptureEvent {
    DiscoveryStarted {
        pass: u8,
        candidates: usize,
    },
    Candidate {
        pass: u8,
        platform: Platform,
        path: String,
        outcome: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    PassCompleted {
        pass: u8,
    },
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct CaptureReport {
    pub discovered_files: usize,
    pub stable_files: usize,
    pub imported_sessions: usize,
    pub skipped_unchanged: usize,
    pub skipped_unstable: usize,
    pub skipped_empty: usize,
    pub skipped_unsupported: usize,
    pub failed_files: usize,
    pub hook_hints: usize,
    pub dry_run: bool,
    pub discovered_by_platform: BTreeMap<String, usize>,
    pub candidates: Vec<CandidateReport>,
    #[serde(default)]
    pub extraction_metrics: BTreeMap<String, Value>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct CaptureState {
    pub version: u32,
    pub files: HashMap<String, FileState>,
    pub pending_hooks: Vec<HookHint>,
    pub last_run_at: Option<u64>,
    #[serde(default)]
    pub schedule: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileState {
    pub size: u64,
    pub modified_at: u64,
    pub fingerprint: String,
    pub captured_fingerprint: Option<String>,
    pub last_seen_at: u64,
    #[serde(default)]
    pub stable_observations: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookHint {
    pub platform: Platform,
    pub session_id: Option<String>,
    pub path: Option<String>,
    pub seen_at: u64,
}

#[derive(Debug, Clone)]
struct Candidate {
    platform: Platform,
    path: PathBuf,
}

pub fn parse_schedule_time(value: &str) -> Result<(u8, u8)> {
    let mut parts = value.split(':');
    let hour = parts
        .next()
        .ok_or_else(|| anyhow!("capture time must be HH:MM"))?
        .parse::<u8>()?;
    let minute = parts
        .next()
        .ok_or_else(|| anyhow!("capture time must be HH:MM"))?
        .parse::<u8>()?;
    if parts.next().is_some() || hour > 23 || minute > 59 {
        return Err(anyhow!(
            "capture time must be HH:MM in the 00:00–23:59 range"
        ));
    }
    Ok((hour, minute))
}

/// Parse the compact duration syntax accepted by `capture run --settle`.
/// Bare numbers are seconds; suffixes are `ms`, `s`, `m`, or `h`.
pub fn parse_settle_duration(value: &str) -> Result<Duration> {
    let value = value.trim().to_ascii_lowercase();
    if value.is_empty() {
        return Err(anyhow!("settle duration must be a positive duration"));
    }
    let (number, multiplier) = if let Some(number) = value.strip_suffix("ms") {
        (number, 1u64)
    } else if let Some(number) = value.strip_suffix('s') {
        (number, 1_000)
    } else if let Some(number) = value.strip_suffix('m') {
        (number, 60_000)
    } else if let Some(number) = value.strip_suffix('h') {
        (number, 3_600_000)
    } else {
        (value.as_str(), 1_000)
    };
    let number = number
        .parse::<u64>()
        .map_err(|_| anyhow!("settle duration must be an integer followed by ms, s, m, or h"))?;
    let milliseconds = number
        .checked_mul(multiplier)
        .ok_or_else(|| anyhow!("settle duration is too large"))?;
    Ok(Duration::from_millis(milliseconds))
}

pub fn run(db_path: &Path, options: &CaptureOptions) -> Result<CaptureReport> {
    let roots = default_roots(options.platform);
    run_with_roots(db_path, options, &roots)
}

pub fn run_with_observer(
    db_path: &Path,
    options: &CaptureOptions,
    observer: &mut dyn FnMut(CaptureEvent),
) -> Result<CaptureReport> {
    let roots = default_roots(options.platform);
    run_with_roots_observer(db_path, options, &roots, observer)
}

/// Testable form of [`run`]. Each tuple is a platform-specific discovery root.
pub fn run_with_roots(
    db_path: &Path,
    options: &CaptureOptions,
    roots: &[(Platform, PathBuf)],
) -> Result<CaptureReport> {
    run_with_roots_observer(db_path, options, roots, &mut |_| {})
}

pub fn run_with_roots_observer(
    db_path: &Path,
    options: &CaptureOptions,
    roots: &[(Platform, PathBuf)],
    observer: &mut dyn FnMut(CaptureEvent),
) -> Result<CaptureReport> {
    let first = run_once(db_path, options, roots, 1, observer)?;
    if options.dry_run || options.settle.is_none() || first.skipped_unstable == 0 {
        return Ok(first);
    }

    if let Some(settle) = options.settle {
        std::thread::sleep(settle);
    }
    let mut second_options = options.clone();
    second_options.settle = None;
    let second = run_once(db_path, &second_options, roots, 2, observer)?;
    Ok(merge_settled_reports(first, second))
}

fn run_once(
    db_path: &Path,
    options: &CaptureOptions,
    roots: &[(Platform, PathBuf)],
    pass: u8,
    observer: &mut dyn FnMut(CaptureEvent),
) -> Result<CaptureReport> {
    parse_schedule_time(&options.time)?;
    // The CLI holds the database lock while this function runs. Hooks never
    // take that lock: they append one immutable queue record instead.
    let (mut state, queued_records, legacy_journal_removable) = load_capture_inputs(db_path)?;
    let now = now_secs();
    let mut candidates = discover_candidates(roots);
    let mut known_paths = candidates
        .iter()
        .map(|candidate| candidate.path.clone())
        .collect::<HashSet<_>>();
    let hinted_paths = state
        .pending_hooks
        .iter()
        .filter_map(|hint| hint.path.as_ref().map(PathBuf::from))
        .collect::<HashSet<_>>();
    for hint in &state.pending_hooks {
        if let Some(path) = &hint.path {
            let path = PathBuf::from(path);
            if path.is_file() && known_paths.insert(path.clone()) {
                candidates.push(Candidate {
                    platform: hint.platform,
                    path,
                });
            }
        }
    }
    let mut report = CaptureReport {
        discovered_files: candidates.len(),
        dry_run: options.dry_run,
        hook_hints: state.pending_hooks.len(),
        ..Default::default()
    };
    observer(CaptureEvent::DiscoveryStarted {
        pass,
        candidates: candidates.len(),
    });
    for candidate in &candidates {
        *report
            .discovered_by_platform
            .entry(candidate.platform.slug().to_string())
            .or_default() += 1;
    }
    let storage = Storage::new(db_path.to_path_buf());
    let mut duplicate_owners = load_duplicate_owners(db_path)?;
    let mut imports = Vec::new();
    let mut duplicate_state_changed = false;
    let mut resolved_hint_paths = HashSet::new();
    let mut resolved_session_ids = HashSet::new();

    for candidate in candidates {
        let metadata = match fs::metadata(&candidate.path) {
            Ok(metadata) => metadata,
            Err(error) => {
                report.failed_files += 1;
                add_candidate_report(
                    &mut report,
                    &candidate,
                    "failed",
                    Some(format!("could not inspect transcript: {error}")),
                    pass,
                    observer,
                );
                continue;
            }
        };
        let size = metadata.len();
        let modified_at = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(now);
        let bytes = match fs::read(&candidate.path) {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::warn!(path = %candidate.path.display(), %error, "capture could not read transcript");
                report.failed_files += 1;
                add_candidate_report(
                    &mut report,
                    &candidate,
                    "failed",
                    Some("could not read transcript".to_string()),
                    pass,
                    observer,
                );
                continue;
            }
        };
        let fingerprint = fingerprint_bytes(&bytes);
        let key = candidate.path.to_string_lossy().to_string();
        let previous = state.files.get(&key);
        let unchanged_observation = previous.is_some_and(|old| {
            old.size == size && old.modified_at == modified_at && old.fingerprint == fingerprint
        });
        let previously_captured = previous.and_then(|old| old.captured_fingerprint.clone());
        let already_captured = previously_captured.as_deref() == Some(&fingerprint);
        let stable_observations = if unchanged_observation {
            previous
                .map(|old| old.stable_observations.max(1).saturating_add(1).min(2))
                .unwrap_or(2)
        } else {
            1
        };
        let hinted_path = hinted_paths.contains(&candidate.path);
        let stable = hinted_path || stable_observations >= 2;

        state.files.insert(
            key.clone(),
            FileState {
                size,
                modified_at,
                fingerprint: fingerprint.clone(),
                captured_fingerprint: previously_captured,
                last_seen_at: now,
                stable_observations,
            },
        );

        if !stable {
            report.skipped_unstable += 1;
            add_candidate_report(
                &mut report,
                &candidate,
                "awaiting second observation",
                Some(
                    "first observation recorded; waiting for an unchanged second observation"
                        .to_string(),
                ),
                pass,
                observer,
            );
            continue;
        }
        report.stable_files += 1;

        if already_captured {
            report.skipped_unchanged += 1;
            if hinted_path {
                // A duplicate lifecycle hook has done its job even when the
                // transcript fingerprint was already archived. Leaving it in
                // the queue would make a later resumed write bypass the
                // two-observation stability guard.
                resolved_hint_paths.insert(candidate.path.clone());
            }
            add_candidate_report(
                &mut report,
                &candidate,
                "unchanged",
                Some("duplicate already archived".to_string()),
                pass,
                observer,
            );
            continue;
        }

        let parsed = match parse_transcript_detailed(candidate.platform, &candidate.path, &bytes) {
            Ok(parsed) => parsed,
            Err(error) => {
                tracing::warn!(path = %candidate.path.display(), %error, "capture skipped malformed transcript");
                report.failed_files += 1;
                add_candidate_report(
                    &mut report,
                    &candidate,
                    "failed",
                    Some("malformed transcript".to_string()),
                    pass,
                    observer,
                );
                continue;
            }
        };
        let sessions = parsed.sessions;
        if sessions.is_empty() {
            report.skipped_empty += 1;
            let reason = parsed.empty_reason.unwrap_or(EmptyTranscriptReason::Empty);
            if reason == EmptyTranscriptReason::UnsupportedSourceRecord {
                report.skipped_unsupported += 1;
            }
            add_candidate_report(
                &mut report,
                &candidate,
                "empty",
                Some(reason.as_str().to_string()),
                pass,
                observer,
            );
            continue;
        }
        if let Some(session) = sessions.first()
            && let Ok(metadata) = serde_json::from_str::<Value>(&session.metadata_json)
        {
            report.extraction_metrics.insert(
                candidate.path.to_string_lossy().into_owned(),
                serde_json::json!({
                    "records_read": metadata.get("records_read").cloned().unwrap_or(Value::Null),
                    "visible_messages_extracted": metadata.get("visible_messages_extracted").cloned().unwrap_or(Value::Null),
                    "records_skipped_by_reason": metadata.get("records_skipped_by_reason").cloned().unwrap_or(Value::Null),
                    "malformed_records": metadata.get("malformed_records").cloned().unwrap_or(Value::Null)
                }),
            );
        }
        if hinted_path {
            resolved_hint_paths.insert(candidate.path.clone());
        }

        let mut imported_from_candidate = false;
        for (session_index, mut session) in sessions.into_iter().enumerate() {
            if let Some(source_id) = metadata_value(&session, "platform_session_id") {
                resolved_session_ids.insert(source_id);
            }
            let source_id = metadata_value(&session, "platform_session_id")
                .filter(|id| !id.is_empty() && id != "unknown");
            let existing = storage.get_session_by_id(&session.id)?;
            let session_fingerprint = fingerprint_for_session(&session);
            let is_unchanged = existing
                .as_ref()
                .and_then(|existing| metadata_value(existing, "content_fingerprint"))
                .is_some_and(|old| old == session_fingerprint);
            let duplicate_key = source_id.is_none().then(|| {
                canonical_duplicate_key(
                    &metadata_platform(&session),
                    &normalized_visible_message_fingerprint(&session),
                )
            });
            let duplicate_archived = duplicate_key
                .as_ref()
                .is_some_and(|key| duplicate_owners.values().any(|archived| archived == key));
            if is_unchanged || duplicate_archived {
                report.skipped_unchanged += 1;
            } else {
                if let Some(existing) = existing.as_ref() {
                    if source_id.is_none() {
                        session.id = existing.id.clone();
                    }
                    set_capture_revision(
                        &mut session,
                        metadata_u64(existing, "capture_revision").unwrap_or(1) + 1,
                    );
                }
                imports.push(session);
                if let Some(duplicate_key) = duplicate_key {
                    duplicate_owners.insert(
                        duplicate_owner_key(&candidate, session_index),
                        duplicate_key,
                    );
                    duplicate_state_changed = true;
                }
                imported_from_candidate = true;
            }
        }
        add_candidate_report(
            &mut report,
            &candidate,
            if imported_from_candidate {
                "imported"
            } else {
                "unchanged"
            },
            if imported_from_candidate {
                None
            } else {
                Some("duplicate already archived".to_string())
            },
            pass,
            observer,
        );
        if imported_from_candidate && let Some(file_state) = state.files.get_mut(&key) {
            file_state.captured_fingerprint = Some(fingerprint);
        }
    }

    let import_count = imports.len();
    if !options.dry_run && !imports.is_empty() {
        report.imported_sessions = storage.append_bulk(imports)?;
    }
    if !options.dry_run && duplicate_state_changed {
        save_duplicate_owners(db_path, &duplicate_owners)?;
    } else if options.dry_run {
        // Count parsed, deduplicated sessions rather than candidate files.
        // One transcript can contain multiple sessions, and malformed or
        // unsupported stable files must not be reported as imported.
        report.imported_sessions = import_count;
    }

    state.last_run_at = Some(now);
    if !options.dry_run {
        state.pending_hooks.retain(|hint| {
            if let Some(path) = &hint.path {
                return !resolved_hint_paths.contains(&PathBuf::from(path));
            }
            hint.session_id
                .as_ref()
                .is_none_or(|session_id| !resolved_session_ids.contains(session_id))
        });
        save_state(db_path, &state)?;
        remove_capture_inputs(db_path, &queued_records, legacy_journal_removable)?;
    }
    observer(CaptureEvent::PassCompleted { pass });
    Ok(report)
}

fn add_candidate_report(
    report: &mut CaptureReport,
    candidate: &Candidate,
    outcome: &str,
    reason: Option<String>,
    pass: u8,
    observer: &mut dyn FnMut(CaptureEvent),
) {
    let path = candidate.path.to_string_lossy().into_owned();
    report.candidates.push(CandidateReport {
        platform: candidate.platform,
        path: path.clone(),
        outcome: outcome.to_string(),
        reason: reason.clone(),
    });
    observer(CaptureEvent::Candidate {
        pass,
        platform: candidate.platform,
        path,
        outcome: outcome.to_string(),
        reason,
    });
}

fn merge_settled_reports(first: CaptureReport, mut second: CaptureReport) -> CaptureReport {
    let mut candidates = first.candidates;
    let mut paths = candidates
        .iter()
        .map(|candidate| candidate.path.clone())
        .collect::<HashSet<_>>();
    for candidate in second.candidates.drain(..) {
        if paths.insert(candidate.path.clone()) {
            candidates.push(candidate);
        } else if let Some(existing) = candidates
            .iter_mut()
            .find(|existing| existing.path == candidate.path)
        {
            *existing = candidate;
        }
    }
    candidates.sort_by(|left, right| left.path.cmp(&right.path));
    second.candidates = candidates;
    second.discovered_files = second.candidates.len();
    second.discovered_by_platform.clear();
    for candidate in &second.candidates {
        *second
            .discovered_by_platform
            .entry(candidate.platform.slug().to_string())
            .or_default() += 1;
    }
    second
}

pub fn record_hook_hint(db_path: &Path, hint: HookHint) -> Result<()> {
    let queue = db_path.join(HINT_QUEUE_DIR);
    fs::create_dir_all(&queue)?;
    let path = queue.join(format!("hint-{}.json", Uuid::new_v4().simple()));
    // create_new makes every hook event an independent record and avoids
    // append races between multiple lifecycle hooks.
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    serde_json::to_writer(&mut file, &hint)?;
    file.sync_all()?;
    Ok(())
}

pub fn load_state(db_path: &Path) -> Result<CaptureState> {
    Ok(load_capture_inputs(db_path)?.0)
}

fn load_capture_inputs(db_path: &Path) -> Result<(CaptureState, Vec<PathBuf>, bool)> {
    let path = db_path.join(STATE_FILE);
    let mut state = if !path.exists() {
        CaptureState {
            version: 1,
            ..Default::default()
        }
    } else {
        let bytes = fs::read(path).context("failed to read capture state")?;
        serde_json::from_slice(&bytes).context("failed to decode capture state")?
    };

    let (queued_records, mut queued_hints) = read_hint_queue(db_path)?;
    let (legacy_journal_present, legacy_hints, legacy_journal_removable) =
        if db_path.join(HOOK_FILE).exists() {
            let (hints, removable) = read_legacy_journal(db_path)?;
            (true, hints, removable)
        } else {
            (false, Vec::new(), false)
        };
    if legacy_journal_present {
        queued_hints.extend(legacy_hints);
    }
    state.pending_hooks.extend(queued_hints);
    deduplicate_hints(&mut state.pending_hooks);
    Ok((state, queued_records, legacy_journal_removable))
}

fn read_hint_queue(db_path: &Path) -> Result<(Vec<PathBuf>, Vec<HookHint>)> {
    let queue = db_path.join(HINT_QUEUE_DIR);
    let mut records = Vec::new();
    let mut hints = Vec::new();
    let Ok(entries) = fs::read_dir(queue) else {
        return Ok((records, hints));
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        if let Ok(bytes) = fs::read(&path)
            && let Ok(hint) = serde_json::from_slice::<HookHint>(&bytes)
        {
            records.push(path.clone());
            hints.push(hint);
        }
    }
    records.sort();
    Ok((records, hints))
}

fn read_legacy_journal(db_path: &Path) -> Result<(Vec<HookHint>, bool)> {
    let path = db_path.join(HOOK_FILE);
    if !path.exists() {
        return Ok((Vec::new(), false));
    }
    let bytes = fs::read(path)?;
    let mut hints = Vec::new();
    let mut removable = true;
    for line in bytes.split(|byte| *byte == b'\n') {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        if let Ok(hint) = serde_json::from_slice::<HookHint>(line) {
            hints.push(hint);
        } else {
            removable = false;
        }
    }
    Ok((hints, removable))
}

fn deduplicate_hints(hints: &mut Vec<HookHint>) {
    let mut latest = HashMap::new();
    for hint in hints.drain(..) {
        let key = (hint.platform, hint.session_id.clone(), hint.path.clone());
        latest
            .entry(key)
            .and_modify(|existing: &mut HookHint| {
                if hint.seen_at >= existing.seen_at {
                    *existing = hint.clone();
                }
            })
            .or_insert(hint);
    }
    hints.extend(latest.into_values());
    hints.sort_by_key(|hint| hint.seen_at);
}

fn remove_capture_inputs(
    db_path: &Path,
    records: &[PathBuf],
    legacy_journal_present: bool,
) -> Result<()> {
    for path in records {
        if path.exists() {
            fs::remove_file(path)?;
        }
    }
    if legacy_journal_present {
        let path = db_path.join(HOOK_FILE);
        if path.exists() {
            fs::remove_file(path)?;
        }
    }
    let queue = db_path.join(HINT_QUEUE_DIR);
    if queue.exists() && fs::read_dir(&queue)?.next().is_none() {
        fs::remove_dir(queue)?;
    }
    Ok(())
}

fn save_state(db_path: &Path, state: &CaptureState) -> Result<()> {
    fs::create_dir_all(db_path)?;
    let path = db_path.join(STATE_FILE);
    let temp = db_path.join("capture-state.json.tmp");
    fs::write(&temp, serde_json::to_vec_pretty(state)?)?;
    fs::rename(temp, path)?;
    Ok(())
}

fn duplicate_owner_key(candidate: &Candidate, session_index: usize) -> String {
    format!(
        "{}:{}#{session_index}",
        candidate.platform.slug(),
        candidate.path.to_string_lossy()
    )
}

fn load_duplicate_owners(db_path: &Path) -> Result<HashMap<String, String>> {
    let path = db_path.join(DUPLICATE_FILE);
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let bytes = fs::read(path).context("failed to read capture duplicate keys")?;
    let value: Value =
        serde_json::from_slice(&bytes).context("failed to decode capture duplicate keys")?;
    if value.is_array() {
        // The old format was an append-only array. It cannot identify which
        // logical source currently owns a key, so discard it during migration
        // rather than retaining stale suppression forever.
        return Ok(HashMap::new());
    }
    serde_json::from_value(value).context("failed to decode capture duplicate owners")
}

fn save_duplicate_owners(db_path: &Path, owners: &HashMap<String, String>) -> Result<()> {
    fs::create_dir_all(db_path)?;
    let path = db_path.join(DUPLICATE_FILE);
    let temp = db_path.join("capture-duplicate-keys.json.tmp");
    fs::write(&temp, serde_json::to_vec(owners)?)?;
    fs::rename(temp, path)?;
    Ok(())
}

pub fn clear_schedule(db_path: &Path) -> Result<()> {
    if !db_path.exists() {
        return Ok(());
    }
    let mut state = load_state(db_path)?;
    state.schedule = None;
    save_state(db_path, &state)
}

pub fn parse_transcript(
    platform: Platform,
    path: &Path,
    bytes: &[u8],
) -> Result<Vec<ChatSessionV1>> {
    Ok(parse_transcript_detailed(platform, path, bytes)?.sessions)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmptyTranscriptReason {
    Empty,
    NoVisibleConversationRecords,
    UnsupportedSourceRecord,
}

impl EmptyTranscriptReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Empty => "empty transcript",
            Self::NoVisibleConversationRecords => "no visible conversation records",
            Self::UnsupportedSourceRecord => "unsupported source record",
        }
    }
}

struct ParsedTranscript {
    sessions: Vec<ChatSessionV1>,
    empty_reason: Option<EmptyTranscriptReason>,
}

fn parse_transcript_detailed(
    platform: Platform,
    path: &Path,
    bytes: &[u8],
) -> Result<ParsedTranscript> {
    let file_fingerprint = fingerprint_bytes(bytes);
    let value = serde_json::from_slice::<Value>(bytes).ok();
    let mut empty_reason = None;
    let mut sessions = if platform == Platform::Antigravity {
        let parsed = parse_antigravity_transcript(path, bytes, &file_fingerprint)?;
        empty_reason = parsed.empty_reason;
        parsed.sessions
    } else if platform == Platform::Codex {
        parse_codex_jsonl(path, bytes, &file_fingerprint)?
    } else if platform == Platform::GeminiCli {
        parse_gemini_transcript(path, bytes, &file_fingerprint)?
    } else if platform == Platform::CopilotCli {
        let copilot = parse_copilot_jsonl(path, bytes, &file_fingerprint)?;
        if copilot.is_empty() {
            if let Some(value) = value.as_ref() {
                parse_document(platform, path, value, &file_fingerprint)?
            } else {
                parse_jsonl(platform, path, bytes, &file_fingerprint)?
            }
        } else {
            copilot
        }
    } else if let Some(value) = value {
        parse_document(platform, path, &value, &file_fingerprint)?
    } else {
        parse_jsonl(platform, path, bytes, &file_fingerprint)?
    };
    sessions.retain(|s| !s.messages.is_empty());
    if sessions.is_empty() && empty_reason.is_none() {
        empty_reason = Some(classify_empty_transcript(bytes));
    }
    let records_read = if let Ok(value) = serde_json::from_slice::<Value>(bytes) {
        value.as_array().map_or(1, Vec::len)
    } else {
        bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.iter().all(u8::is_ascii_whitespace))
            .count()
    };
    let malformed_records = bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.iter().all(u8::is_ascii_whitespace))
        .filter(|line| serde_json::from_slice::<Value>(line).is_err())
        .count();
    let hidden_reasoning = bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| {
            let text = String::from_utf8_lossy(line).to_ascii_lowercase();
            text.contains("reason") || text.contains("thinking") || text.contains("progress")
        })
        .count();
    let visible_messages_extracted = sessions.iter().map(|s| s.messages.len()).sum::<usize>();
    for session in &mut sessions {
        update_capture_metrics(
            session,
            records_read,
            visible_messages_extracted,
            malformed_records,
            hidden_reasoning,
        );
    }
    Ok(ParsedTranscript {
        sessions,
        empty_reason,
    })
}

fn classify_empty_transcript(bytes: &[u8]) -> EmptyTranscriptReason {
    let has_non_blank = bytes.iter().any(|byte| !byte.is_ascii_whitespace());
    if !has_non_blank {
        return EmptyTranscriptReason::Empty;
    }
    let valid_document = serde_json::from_slice::<Value>(bytes).is_ok();
    let valid_jsonl = bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.iter().all(u8::is_ascii_whitespace))
        .all(|line| serde_json::from_slice::<Value>(line).is_ok());
    if valid_document || valid_jsonl {
        EmptyTranscriptReason::UnsupportedSourceRecord
    } else {
        EmptyTranscriptReason::Empty
    }
}

fn update_capture_metrics(
    session: &mut ChatSessionV1,
    records_read: usize,
    visible_messages_extracted: usize,
    malformed_records: usize,
    hidden_reasoning: usize,
) {
    let mut metadata = serde_json::from_str::<Value>(&session.metadata_json)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    metadata.insert(
        "records_read".into(),
        Value::Number((records_read as u64).into()),
    );
    metadata.insert(
        "visible_messages_extracted".into(),
        Value::Number((visible_messages_extracted as u64).into()),
    );
    let mut skipped = Map::new();
    if hidden_reasoning > 0 {
        skipped.insert(
            "opaque reasoning".into(),
            Value::Number((hidden_reasoning as u64).into()),
        );
    }
    metadata.insert("records_skipped_by_reason".into(), Value::Object(skipped));
    metadata.insert(
        "malformed_records".into(),
        Value::Number((malformed_records as u64).into()),
    );
    session.metadata_json = Value::Object(metadata).to_string();
}

fn parse_document(
    platform: Platform,
    path: &Path,
    value: &Value,
    file_fingerprint: &str,
) -> Result<Vec<ChatSessionV1>> {
    if let Some(items) = value.as_array() {
        let mut result = Vec::new();
        for item in items {
            if item.get("mapping").is_some() {
                if let Ok(conversation) =
                    serde_json::from_value::<ChatGptConversation>(item.clone())
                {
                    let source_id = conversation.id.clone();
                    if let Ok(session) = ChatSessionV1::try_from(conversation) {
                        result.push(with_capture_metadata(
                            session,
                            platform,
                            path,
                            Some(&source_id),
                            file_fingerprint,
                        ));
                    }
                }
            } else if let Some(session) =
                parse_value_as_session(platform, path, item, file_fingerprint)
            {
                result.push(session);
            }
        }
        return Ok(result);
    }
    if value.get("mapping").is_some() {
        let conversation: ChatGptConversation = serde_json::from_value(value.clone())?;
        let source_id = conversation.id.clone();
        let session: ChatSessionV1 = conversation.try_into()?;
        return Ok(vec![with_capture_metadata(
            session,
            platform,
            path,
            Some(&source_id),
            file_fingerprint,
        )]);
    }
    Ok(
        parse_value_as_session(platform, path, value, file_fingerprint)
            .into_iter()
            .collect(),
    )
}

fn parse_jsonl(
    platform: Platform,
    path: &Path,
    bytes: &[u8],
    file_fingerprint: &str,
) -> Result<Vec<ChatSessionV1>> {
    let mut messages = Vec::new();
    let mut session_id = None;
    let mut model = None;
    let mut created_at = None;
    let mut invalid_lines = 0;
    for line in bytes.split(|b| *b == b'\n') {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let value = match serde_json::from_slice::<Value>(line) {
            Ok(value) => value,
            Err(_) => {
                invalid_lines += 1;
                continue;
            }
        };
        let is_session_meta = value.get("type").and_then(Value::as_str) == Some("session_meta");
        session_id = session_id.or_else(|| {
            find_string(
                &value,
                &[
                    "session_id",
                    "sessionId",
                    "conversation_id",
                    "conversationId",
                ],
            )
            .or_else(|| {
                is_session_meta
                    .then(|| value.get("payload"))
                    .flatten()
                    .and_then(|payload| {
                        find_string(
                            payload,
                            &[
                                "session_id",
                                "sessionId",
                                "conversation_id",
                                "conversationId",
                                "id",
                            ],
                        )
                    })
            })
        });
        model = model.or_else(|| {
            find_string(&value, &["model", "model_name", "modelName"]).or_else(|| {
                is_session_meta
                    .then(|| value.get("payload"))
                    .flatten()
                    .and_then(|payload| find_string(payload, &["model", "model_name", "modelName"]))
            })
        });
        created_at = created_at.or_else(|| {
            find_timestamp(&value).or_else(|| {
                is_session_meta
                    .then(|| value.get("payload"))
                    .flatten()
                    .and_then(find_timestamp)
            })
        });
        collect_message_records(&value, &mut messages);
    }
    if messages.is_empty() {
        if invalid_lines > 0 {
            return Err(anyhow!("malformed JSONL transcript"));
        }
        return Ok(Vec::new());
    }
    let id = session_id.clone().unwrap_or_else(|| {
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("session")
            .to_string()
    });
    let mut session = make_session(platform, path, &id, model, created_at, messages);
    session = with_capture_metadata(
        session,
        platform,
        path,
        session_id.as_deref(),
        file_fingerprint,
    );
    Ok(vec![session])
}

struct AntigravityParse {
    sessions: Vec<ChatSessionV1>,
    empty_reason: Option<EmptyTranscriptReason>,
}

/// Parse Antigravity's system-generated event stream. These records are not
/// role/content JSONL: `source` and `type` describe user input, model
/// responses, tool execution, and internal system state separately.
fn parse_antigravity_transcript(
    path: &Path,
    bytes: &[u8],
    file_fingerprint: &str,
) -> Result<AntigravityParse> {
    let mut messages = Vec::new();
    let mut session_id = None;
    let mut model = None;
    let mut created_at = None;
    let mut invalid_lines = 0;
    let mut recognized_record = false;
    let mut unsupported_record = false;

    for line in bytes.split(|byte| *byte == b'\n') {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let value = match serde_json::from_slice::<Value>(line) {
            Ok(value) => value,
            Err(_) => {
                invalid_lines += 1;
                continue;
            }
        };
        let Some(object) = value.as_object() else {
            unsupported_record = true;
            continue;
        };

        session_id = session_id.or_else(|| {
            find_string(
                &value,
                &[
                    "conversation_id",
                    "conversationId",
                    "session_id",
                    "sessionId",
                ],
            )
        });
        model = model.or_else(|| antigravity_model_id(object));
        created_at = created_at.or_else(|| find_timestamp(&value));

        match antigravity_record_message(object) {
            AntigravityRecord::Message(message) => {
                recognized_record = true;
                messages.push(message);
            }
            AntigravityRecord::Recognized => recognized_record = true,
            AntigravityRecord::Unsupported => unsupported_record = true,
        }
    }

    if messages.is_empty() {
        if invalid_lines > 0 {
            return Err(anyhow!("malformed Antigravity transcript"));
        }
        return Ok(AntigravityParse {
            sessions: Vec::new(),
            empty_reason: Some(if unsupported_record {
                EmptyTranscriptReason::UnsupportedSourceRecord
            } else if recognized_record {
                EmptyTranscriptReason::NoVisibleConversationRecords
            } else {
                EmptyTranscriptReason::Empty
            }),
        });
    }

    let source_id = session_id.or_else(|| antigravity_brain_session_id(path));
    let display_id = source_id.clone().unwrap_or_else(|| {
        path.file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("session")
            .to_string()
    });
    let session = make_session(
        Platform::Antigravity,
        path,
        &display_id,
        model,
        created_at,
        messages,
    );
    Ok(AntigravityParse {
        sessions: vec![with_capture_metadata(
            session,
            Platform::Antigravity,
            path,
            source_id.as_deref(),
            file_fingerprint,
        )],
        empty_reason: None,
    })
}

enum AntigravityRecord {
    Message(MessageV1),
    Recognized,
    Unsupported,
}

fn antigravity_record_message(object: &Map<String, Value>) -> AntigravityRecord {
    let source = object
        .get("source")
        .and_then(Value::as_str)
        .map(|source| source.to_ascii_uppercase());
    let event_type = object
        .get("type")
        .and_then(Value::as_str)
        .map(|event_type| event_type.to_ascii_uppercase());
    let Some(event_type) = event_type else {
        return AntigravityRecord::Unsupported;
    };
    if source.as_deref() == Some("SYSTEM") {
        return AntigravityRecord::Recognized;
    }

    if source.as_deref() == Some("USER_EXPLICIT") && event_type == "USER_INPUT" {
        let content = antigravity_visible_content(object);
        if content.trim().is_empty() {
            return AntigravityRecord::Recognized;
        }
        return AntigravityRecord::Message(antigravity_message(
            MessageRole::User,
            content,
            None,
            &event_type,
            object,
        ));
    }

    if source.as_deref() != Some("MODEL") {
        return AntigravityRecord::Unsupported;
    }

    if antigravity_model_response_type(&event_type) {
        let tool_calls = antigravity_tool_calls(object);
        let content = antigravity_visible_content(object);
        if content.trim().is_empty() && tool_calls.is_none() {
            return AntigravityRecord::Recognized;
        }
        return AntigravityRecord::Message(antigravity_message(
            MessageRole::Model,
            content,
            tool_calls,
            &event_type,
            object,
        ));
    }

    if antigravity_tool_event_type(&event_type) {
        let content = antigravity_visible_content(object);
        if content.trim().is_empty() {
            return AntigravityRecord::Recognized;
        }
        return AntigravityRecord::Message(antigravity_message(
            MessageRole::Tool,
            content,
            None,
            &event_type,
            object,
        ));
    }

    AntigravityRecord::Unsupported
}

fn antigravity_message(
    role: MessageRole,
    content: String,
    tool_calls: Option<Vec<ToolCall>>,
    event_type: &str,
    object: &Map<String, Value>,
) -> MessageV1 {
    MessageV1 {
        role,
        content: content.trim().to_string(),
        tool_calls,
        tool_outputs: None,
        id: find_string(
            &Value::Object(object.clone()),
            &["id", "message_id", "messageId"],
        ),
        parent_id: find_string(&Value::Object(object.clone()), &["parent_id", "parentId"]),
        metadata_json: json!({"event_type": event_type}).to_string(),
    }
}

fn antigravity_model_response_type(event_type: &str) -> bool {
    matches!(
        event_type,
        "PLANNER_RESPONSE"
            | "MODEL_RESPONSE"
            | "ASSISTANT_RESPONSE"
            | "FINAL_RESPONSE"
            | "TEXT_RESPONSE"
    )
}

fn antigravity_tool_event_type(event_type: &str) -> bool {
    matches!(
        event_type,
        "LIST_DIRECTORY"
            | "VIEW_FILE"
            | "RUN_COMMAND"
            | "GENERIC"
            | "SEARCH_FILES"
            | "READ_FILE"
            | "WRITE_FILE"
            | "EDIT_FILE"
            | "DELETE_FILE"
            | "BROWSER"
            | "BROWSER_ACTION"
            | "TASK_STATUS"
            | "TASK_STARTED"
            | "TASK_UPDATED"
            | "TASK_COMPLETED"
            | "TASK_FAILED"
            | "TASK_CANCELLED"
    )
}

fn antigravity_visible_content(object: &Map<String, Value>) -> String {
    for key in ["content", "text", "result", "output", "details", "command"] {
        if let Some(value) = object.get(key) {
            let content = extract_visible_text(value);
            if !content.trim().is_empty() {
                return content;
            }
        }
    }
    String::new()
}

fn antigravity_tool_calls(object: &Map<String, Value>) -> Option<Vec<ToolCall>> {
    let calls = object
        .get("tool_calls")
        .or_else(|| object.get("toolCalls"))
        .and_then(|value| value.as_array())?;
    let calls = calls
        .iter()
        .filter_map(|call| {
            let call = call.as_object()?;
            let name = call
                .get("name")
                .or_else(|| call.get("tool_name"))
                .and_then(Value::as_str)?;
            let arguments = call
                .get("arguments")
                .or_else(|| call.get("args"))
                .or_else(|| call.get("input"))
                .or_else(|| call.get("parameters"))
                .map(antigravity_json_text)
                .unwrap_or_default();
            Some(ToolCall {
                name: name.to_string(),
                arguments,
                id: call
                    .get("id")
                    .or_else(|| call.get("tool_call_id"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
        })
        .collect::<Vec<_>>();
    (!calls.is_empty()).then_some(calls)
}

fn antigravity_json_text(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_string)
        .or_else(|| serde_json::to_string(value).ok())
        .unwrap_or_default()
}

fn antigravity_model_id(object: &Map<String, Value>) -> Option<String> {
    ["model", "model_name", "modelName"].iter().find_map(|key| {
        object
            .get(*key)
            .and_then(Value::as_str)
            .filter(|model| !model.trim().is_empty())
            .map(str::to_string)
    })
}

fn antigravity_brain_session_id(path: &Path) -> Option<String> {
    let components = path
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect::<Vec<_>>();
    components
        .windows(2)
        .find(|components| components[0] == "brain" && Uuid::parse_str(components[1]).is_ok())
        .map(|components| components[1].to_string())
}

/// Parse Codex rollouts using the canonical response-item history. Codex can
/// also emit legacy `event_msg.user_message` records that mirror the same
/// user input; importing both would create duplicate user turns.
fn parse_codex_jsonl(
    path: &Path,
    bytes: &[u8],
    file_fingerprint: &str,
) -> Result<Vec<ChatSessionV1>> {
    let mut records = Vec::new();
    let mut invalid_lines = 0;
    for line in bytes.split(|byte| *byte == b'\n') {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice::<Value>(line) {
            Ok(value) => records.push(value),
            Err(_) => invalid_lines += 1,
        }
    }

    let has_canonical_messages = records.iter().any(|value| {
        value.get("type").and_then(Value::as_str) == Some("response_item")
            && value
                .get("payload")
                .and_then(Value::as_object)
                .is_some_and(|payload| {
                    payload.get("type").and_then(Value::as_str) == Some("message")
                        && payload
                            .get("role")
                            .and_then(Value::as_str)
                            .and_then(normalize_role)
                            .is_some_and(|role| role != MessageRole::Thought)
                })
    });

    let mut messages = Vec::new();
    let mut session_id = None;
    let mut model = None;
    let mut created_at = None;
    for value in records {
        let is_session_meta = value.get("type").and_then(Value::as_str) == Some("session_meta");
        session_id = session_id.or_else(|| {
            find_string(
                &value,
                &[
                    "session_id",
                    "sessionId",
                    "conversation_id",
                    "conversationId",
                ],
            )
            .or_else(|| {
                is_session_meta
                    .then(|| value.get("payload"))
                    .flatten()
                    .and_then(|payload| {
                        find_string(
                            payload,
                            &[
                                "session_id",
                                "sessionId",
                                "conversation_id",
                                "conversationId",
                                "id",
                            ],
                        )
                    })
            })
        });
        model = model.or_else(|| {
            find_string(&value, &["model", "model_name", "modelName"]).or_else(|| {
                is_session_meta
                    .then(|| value.get("payload"))
                    .flatten()
                    .and_then(|payload| find_string(payload, &["model", "model_name", "modelName"]))
            })
        });
        created_at = created_at.or_else(|| {
            find_timestamp(&value).or_else(|| {
                is_session_meta
                    .then(|| value.get("payload"))
                    .flatten()
                    .and_then(find_timestamp)
            })
        });

        match value.get("type").and_then(Value::as_str) {
            Some("response_item") => {
                if let Some(payload) = value.get("payload") {
                    collect_message_records(payload, &mut messages);
                }
            }
            Some("event_msg")
                if has_canonical_messages
                    && value
                        .get("payload")
                        .and_then(|payload| payload.get("type"))
                        .and_then(Value::as_str)
                        == Some("user_message") => {}
            _ => collect_message_records(&value, &mut messages),
        }
    }

    if messages.is_empty() {
        if invalid_lines > 0 {
            return Err(anyhow!("malformed JSONL transcript"));
        }
        return Ok(Vec::new());
    }
    let id = session_id.clone().unwrap_or_else(|| {
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("session")
            .to_string()
    });
    let session = make_session(Platform::Codex, path, &id, model, created_at, messages);
    Ok(vec![with_capture_metadata(
        session,
        Platform::Codex,
        path,
        session_id.as_deref(),
        file_fingerprint,
    )])
}

/// Parse Gemini CLI recordings without treating control records as messages.
///
/// Modern Gemini sessions are JSONL streams whose records are either session
/// metadata, visible `user`/`gemini` messages, or state changes. `$rewindTo`
/// removes the record at the supplied message ID and everything after it.
/// If the target is absent, the current message set is cleared. `$set.messages`
/// is a checkpoint and replaces the current message set.
/// Legacy `session-*.json` files contain the same metadata and messages in one
/// JSON object and are handled by the same state machine.
fn parse_gemini_transcript(
    path: &Path,
    bytes: &[u8],
    file_fingerprint: &str,
) -> Result<Vec<ChatSessionV1>> {
    let mut metadata = Map::new();
    let mut message_values = Vec::new();
    let mut invalid_records = 0;

    if let Ok(value) = serde_json::from_slice::<Value>(bytes) {
        if let Some(object) = value.as_object() {
            merge_gemini_metadata(&mut metadata, object);
            if let Some(messages) = object.get("messages").and_then(Value::as_array) {
                message_values = messages.clone();
            }
        } else {
            return Ok(Vec::new());
        }
    } else {
        for line in bytes.split(|byte| *byte == b'\n') {
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let value = match serde_json::from_slice::<Value>(line) {
                Ok(value) => value,
                Err(_) => {
                    invalid_records += 1;
                    continue;
                }
            };
            apply_gemini_record(&value, &mut metadata, &mut message_values);
        }
    }

    if message_values.is_empty() {
        if invalid_records > 0 {
            return Err(anyhow!("malformed Gemini transcript"));
        }
        return Ok(Vec::new());
    }

    let mut messages = Vec::new();
    let mut model = metadata
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string);
    for value in &message_values {
        if let Some(message) = gemini_message(value, &mut model) {
            messages.push(message);
        }
    }
    if messages.is_empty() {
        return Ok(Vec::new());
    }

    let session_id = metadata
        .get("sessionId")
        .or_else(|| metadata.get("session_id"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let display_id = session_id.clone().unwrap_or_else(|| {
        path.file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("session")
            .to_string()
    });
    let session = make_session(
        Platform::GeminiCli,
        path,
        &display_id,
        model,
        metadata.get("startTime").and_then(value_timestamp),
        messages,
    );
    Ok(vec![with_capture_metadata(
        session,
        Platform::GeminiCli,
        path,
        session_id.as_deref(),
        file_fingerprint,
    )])
}

fn apply_gemini_record(
    value: &Value,
    metadata: &mut Map<String, Value>,
    messages: &mut Vec<Value>,
) {
    let Some(object) = value.as_object() else {
        return;
    };
    if let Some(rewind_to) = object.get("$rewindTo").and_then(Value::as_str) {
        if let Some(index) = messages
            .iter()
            .position(|message| message.get("id").and_then(Value::as_str) == Some(rewind_to))
        {
            messages.truncate(index);
        } else {
            messages.clear();
        }
        return;
    }
    if let Some(set) = object.get("$set").and_then(Value::as_object) {
        merge_gemini_metadata(metadata, set);
        if let Some(set_messages) = set.get("messages").and_then(Value::as_array) {
            *messages = set_messages.clone();
        }
        return;
    }
    if object.get("type").and_then(Value::as_str) == Some("message_update") {
        let Some(id) = object.get("id").and_then(Value::as_str) else {
            return;
        };
        if let Some(existing) = messages
            .iter_mut()
            .find(|message| message.get("id").and_then(Value::as_str) == Some(id))
            && let Some(existing) = existing.as_object_mut()
        {
            for (key, value) in object {
                if key != "type" {
                    existing.insert(key.clone(), value.clone());
                }
            }
        }
        return;
    }
    merge_gemini_metadata(metadata, object);
    if matches!(
        object.get("type").and_then(Value::as_str),
        Some("user" | "gemini")
    ) {
        messages.push(value.clone());
    }
}

fn merge_gemini_metadata(metadata: &mut Map<String, Value>, object: &Map<String, Value>) {
    for key in [
        "sessionId",
        "session_id",
        "projectHash",
        "startTime",
        "lastUpdated",
        "summary",
        "model",
        "kind",
    ] {
        if let Some(value) = object.get(key) {
            metadata.insert(key.to_string(), value.clone());
        }
    }
}

fn gemini_message(value: &Value, model: &mut Option<String>) -> Option<MessageV1> {
    let object = value.as_object()?;
    let kind = object.get("type").and_then(Value::as_str)?;
    let role = match kind {
        "user" => MessageRole::User,
        "gemini" => MessageRole::Model,
        _ => return None,
    };
    if role == MessageRole::Model {
        *model = object
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| model.clone());
    }
    let content = object
        .get("content")
        .map(extract_visible_text)
        .unwrap_or_default();
    let tool_calls = object
        .get("toolCalls")
        .and_then(Value::as_array)
        .map(|calls| {
            calls
                .iter()
                .filter_map(|call| {
                    let call = call.as_object()?;
                    let name = call.get("name")?.as_str()?.to_string();
                    let arguments = call
                        .get("args")
                        .or_else(|| call.get("arguments"))
                        .map(|args| {
                            args.as_str()
                                .map(str::to_string)
                                .or_else(|| serde_json::to_string(args).ok())
                                .unwrap_or_default()
                        })
                        .unwrap_or_default();
                    Some(ToolCall {
                        name,
                        arguments,
                        id: call.get("id").and_then(Value::as_str).map(str::to_string),
                    })
                })
                .collect::<Vec<_>>()
        })
        .filter(|calls| !calls.is_empty());
    let tool_outputs = object
        .get("toolCalls")
        .and_then(Value::as_array)
        .map(|calls| {
            calls
                .iter()
                .filter_map(|call| {
                    let call = call.as_object()?;
                    let result = call.get("result")?;
                    if result.is_null() {
                        return None;
                    }
                    Some(ToolOutput {
                        tool_call_id: call.get("id").and_then(Value::as_str).map(str::to_string),
                        content: extract_visible_text(result),
                    })
                })
                .collect::<Vec<_>>()
        })
        .filter(|outputs| !outputs.is_empty());
    if content.trim().is_empty() && tool_calls.is_none() {
        return None;
    }
    Some(MessageV1 {
        role,
        content: content.trim().to_string(),
        tool_calls,
        tool_outputs,
        id: object.get("id").and_then(Value::as_str).map(str::to_string),
        parent_id: object
            .get("parentId")
            .or_else(|| object.get("parent_id"))
            .and_then(Value::as_str)
            .map(str::to_string),
        metadata_json: String::new(),
    })
}

fn value_timestamp(value: &Value) -> Option<u64> {
    value.as_str().and_then(|value| {
        DateTime::parse_from_rfc3339(value)
            .ok()
            .map(|date| date.timestamp().max(0) as u64)
    })
}

/// Copilot CLI writes an envelope whose visible payload is nested under
/// `data`.  Keep this adapter deliberately narrow so metadata files and
/// opaque reasoning events cannot become archive messages.
fn parse_copilot_jsonl(
    path: &Path,
    bytes: &[u8],
    file_fingerprint: &str,
) -> Result<Vec<ChatSessionV1>> {
    let mut messages = Vec::new();
    let mut session_id = None;
    let mut model = None;
    let mut created_at = None;
    let mut invalid_lines = 0;
    for line in bytes.split(|b| *b == b'\n') {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let value = match serde_json::from_slice::<Value>(line) {
            Ok(value) => value,
            Err(_) => {
                invalid_lines += 1;
                continue;
            }
        };
        session_id = session_id.or_else(|| {
            find_string(
                &value,
                &[
                    "session_id",
                    "sessionId",
                    "conversation_id",
                    "conversationId",
                ],
            )
            .or_else(|| {
                value.get("data").and_then(|data| {
                    find_string(
                        data,
                        &[
                            "session_id",
                            "sessionId",
                            "conversation_id",
                            "conversationId",
                        ],
                    )
                })
            })
        });
        model = model.or_else(|| {
            find_string(&value, &["model", "model_name", "modelName"]).or_else(|| {
                value
                    .get("data")
                    .and_then(|data| find_string(data, &["model", "model_name", "modelName"]))
            })
        });
        created_at = created_at.or_else(|| {
            find_timestamp(&value).or_else(|| value.get("data").and_then(find_timestamp))
        });

        let event_type = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if event_type.contains("reason")
            || event_type.contains("thinking")
            || event_type.contains("progress")
        {
            continue;
        }
        let role = if event_type.starts_with("user")
            || (event_type.is_empty()
                && value
                    .get("role")
                    .and_then(Value::as_str)
                    .is_some_and(|role| normalize_role(role) == Some(MessageRole::User)))
        {
            Some(MessageRole::User)
        } else if event_type.starts_with("assistant")
            || event_type.starts_with("model")
            || (event_type.is_empty()
                && value
                    .get("role")
                    .and_then(Value::as_str)
                    .is_some_and(|role| normalize_role(role) == Some(MessageRole::Model)))
        {
            Some(MessageRole::Model)
        } else if event_type.starts_with("tool")
            || (event_type.is_empty()
                && value
                    .get("role")
                    .and_then(Value::as_str)
                    .is_some_and(|role| normalize_role(role) == Some(MessageRole::Tool)))
        {
            Some(MessageRole::Tool)
        } else {
            None
        };
        let Some(role) = role else { continue };
        let payload = value.get("data").unwrap_or(&value);
        let mut content = payload
            .get("content")
            .or_else(|| payload.get("text"))
            .or_else(|| payload.get("result"))
            .map(extract_visible_text)
            .unwrap_or_default();
        if role == MessageRole::Tool
            && content.trim().is_empty()
            && let Some(name) = payload
                .get("toolName")
                .or_else(|| payload.get("name"))
                .and_then(Value::as_str)
        {
            let args = payload
                .get("arguments")
                .or_else(|| payload.get("input"))
                .map(extract_visible_text)
                .unwrap_or_default();
            let args = if args.is_empty() {
                payload
                    .get("arguments")
                    .or_else(|| payload.get("input"))
                    .and_then(|value| serde_json::to_string(value).ok())
                    .unwrap_or_default()
            } else {
                args
            };
            content = if args.is_empty() {
                format!("[tool: {name}]")
            } else {
                format!("[tool: {name}] {args}")
            };
        }
        if content.trim().is_empty() {
            continue;
        }
        messages.push(MessageV1 {
            role,
            content: content.trim().to_string(),
            tool_calls: None,
            tool_outputs: None,
            id: find_string(payload, &["id", "message_id", "messageId"]),
            parent_id: find_string(payload, &["parent_id", "parentId"]),
            metadata_json: String::new(),
        });
    }
    if messages.is_empty() {
        if invalid_lines > 0 {
            return Err(anyhow!("malformed JSONL transcript"));
        }
        return Ok(Vec::new());
    }
    let display_id = session_id.clone().unwrap_or_else(|| {
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("session")
            .to_string()
    });
    let session = make_session(
        Platform::CopilotCli,
        path,
        &display_id,
        model,
        created_at,
        messages,
    );
    Ok(vec![with_capture_metadata(
        session,
        Platform::CopilotCli,
        path,
        session_id.as_deref(),
        file_fingerprint,
    )])
}

fn parse_value_as_session(
    platform: Platform,
    path: &Path,
    value: &Value,
    file_fingerprint: &str,
) -> Option<ChatSessionV1> {
    if let Some(messages_value) = value.get("messages").or_else(|| value.get("conversation")) {
        let mut messages = Vec::new();
        if let Some(items) = messages_value.as_array() {
            for item in items {
                collect_message_records(item, &mut messages);
            }
        }
        if !messages.is_empty() {
            let id = find_string(
                value,
                &[
                    "session_id",
                    "sessionId",
                    "conversation_id",
                    "conversationId",
                    "id",
                ],
            )
            .unwrap_or_else(|| {
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("session")
                    .to_string()
            });
            let source_id = find_string(
                value,
                &[
                    "session_id",
                    "sessionId",
                    "conversation_id",
                    "conversationId",
                ],
            );
            let display_id = source_id.clone().unwrap_or_else(|| id.clone());
            let session = make_session(
                platform,
                path,
                &display_id,
                find_string(value, &["model", "model_name", "modelName"]),
                find_timestamp(value),
                messages,
            );
            return Some(with_capture_metadata(
                session,
                platform,
                path,
                source_id.as_deref(),
                file_fingerprint,
            ));
        }
    }
    None
}

fn make_session(
    platform: Platform,
    path: &Path,
    source_id: &str,
    model: Option<String>,
    created_at: Option<u64>,
    messages: Vec<MessageV1>,
) -> ChatSessionV1 {
    let title = title_from_messages(&messages, platform);
    ChatSessionV1 {
        id: stable_session_id(platform, Some(source_id), path),
        title: Some(title),
        source: Some(platform.slug().to_string()),
        model,
        created_at,
        metadata_json: String::new(),
        messages,
    }
}

fn with_capture_metadata(
    mut session: ChatSessionV1,
    platform: Platform,
    path: &Path,
    source_id: Option<&str>,
    file_fingerprint: &str,
) -> ChatSessionV1 {
    let session_fingerprint = fingerprint_for_session(&session);
    let source_id = source_id.filter(|id| !id.is_empty() && *id != "unknown");
    let mut metadata = serde_json::Map::new();
    metadata.insert(
        "source_path".into(),
        Value::String(path.to_string_lossy().to_string()),
    );
    metadata.insert(
        "platform_session_id".into(),
        Value::String(source_id.unwrap_or("unknown").to_string()),
    );
    metadata.insert(
        "content_fingerprint".into(),
        Value::String(session_fingerprint.clone()),
    );
    metadata.insert(
        "file_fingerprint".into(),
        Value::String(file_fingerprint.to_string()),
    );
    metadata.insert("last_seen_at".into(), Value::Number(now_secs().into()));
    metadata.insert("capture_revision".into(), Value::Number(1.into()));
    metadata.insert("importer".into(), Value::String("capture".into()));
    metadata.insert("parser_version".into(), Value::String("capture-v2".into()));
    metadata.insert("ingest_time".into(), Value::Number(now_secs().into()));
    metadata.insert(
        "source_platform".into(),
        Value::String(platform.slug().into()),
    );
    metadata.insert(
        "source_session_id".into(),
        Value::String(source_id.unwrap_or("unknown").into()),
    );
    if source_id.is_none() {
        metadata.insert(
            "duplicate_key".into(),
            Value::String(canonical_duplicate_key(
                platform.slug(),
                &normalized_visible_message_fingerprint(&session),
            )),
        );
    }
    session.metadata_json = Value::Object(metadata).to_string();
    // A platform session ID is the primary identity. Without one, retain a
    // path-based ID so changed content can be stored as a new revision while
    // duplicate content is suppressed separately via duplicate_key.
    session.id = stable_session_id(platform, source_id, path);
    session
}

fn collect_message_records(value: &Value, out: &mut Vec<MessageV1>) {
    let Some(object) = value.as_object() else {
        return;
    };
    if let Some(nested) = object.get("message")
        && nested.is_object()
    {
        collect_message_records(nested, out);
        return;
    }
    let role = object
        .get("role")
        .and_then(Value::as_str)
        .or_else(|| object.get("type").and_then(Value::as_str));
    let role = role.and_then(normalize_role);
    if let Some(role) = role {
        if role == MessageRole::Thought {
            return;
        }
        if is_hidden_record(object) {
            return;
        }
        let content = object
            .get("content")
            .or_else(|| object.get("text"))
            .or_else(|| object.get("parts"))
            .or_else(|| object.get("result"))
            .or_else(|| object.get("message"))
            .map(extract_visible_text)
            .unwrap_or_default();
        if !content.trim().is_empty() {
            out.push(MessageV1 {
                role,
                content: content.trim().to_string(),
                tool_calls: None,
                tool_outputs: None,
                id: find_string(value, &["id", "message_id", "messageId"]),
                parent_id: find_string(value, &["parent_id", "parentId"]),
                metadata_json: String::new(),
            });
        }
        return;
    }
    for child in object.values() {
        collect_message_records(child, out);
    }
}

fn normalize_role(role: &str) -> Option<MessageRole> {
    match role.to_ascii_lowercase().as_str() {
        "user" | "human" | "user.message" | "user_message" | "prompt" => Some(MessageRole::User),
        "assistant" | "model" | "ai" | "gemini" | "assistant.message" => Some(MessageRole::Model),
        "system" | "system.message" => Some(MessageRole::System),
        "tool" | "tool_result" | "tool-use" | "tool_call" | "function" => Some(MessageRole::Tool),
        "thought" | "thinking" | "reasoning" | "analysis" => Some(MessageRole::Thought),
        _ => None,
    }
}

fn is_hidden_record(object: &Map<String, Value>) -> bool {
    object.get("isMeta").and_then(Value::as_bool) == Some(true)
        || object.get("is_internal").and_then(Value::as_bool) == Some(true)
        || object.get("hidden").and_then(Value::as_bool) == Some(true)
        || object
            .get("subtype")
            .and_then(Value::as_str)
            .is_some_and(|s| matches!(s, "thinking" | "reasoning" | "internal" | "progress"))
}

fn extract_visible_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Array(values) => values
            .iter()
            .map(extract_visible_text)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(object) => {
            if object.get("type").and_then(Value::as_str).is_some_and(|t| {
                matches!(
                    t,
                    "thinking" | "reasoning" | "analysis" | "redacted_thinking"
                )
            }) {
                return String::new();
            }
            if let Some(text) = object.get("text") {
                return extract_visible_text(text);
            }
            if let Some(content) = object.get("content") {
                return extract_visible_text(content);
            }
            if let Some(parts) = object.get("parts") {
                return extract_visible_text(parts);
            }
            if let Some(name) = object.get("name").and_then(Value::as_str) {
                let args = object
                    .get("arguments")
                    .or_else(|| object.get("input"))
                    .map(extract_visible_text)
                    .unwrap_or_default();
                return if args.is_empty() {
                    format!("[tool: {name}]")
                } else {
                    format!("[tool: {name}] {args}")
                };
            }
            String::new()
        }
        _ => String::new(),
    }
}

fn find_string(value: &Value, keys: &[&str]) -> Option<String> {
    let object = value.as_object()?;
    keys.iter()
        .find_map(|key| object.get(*key).and_then(Value::as_str).map(str::to_string))
}

fn find_timestamp(value: &Value) -> Option<u64> {
    let object = value.as_object()?;
    for key in [
        "created_at",
        "create_time",
        "timestamp",
        "createdAt",
        "date",
    ] {
        if let Some(number) = object.get(key).and_then(Value::as_u64) {
            return Some(if number > 10_000_000_000 {
                number / 1000
            } else {
                number
            });
        }
        if let Some(text) = object.get(key).and_then(Value::as_str) {
            if let Ok(number) = text.parse::<u64>() {
                return Some(if number > 10_000_000_000 {
                    number / 1000
                } else {
                    number
                });
            }
            if let Ok(date) = DateTime::parse_from_rfc3339(text) {
                return Some(date.timestamp().max(0) as u64);
            }
        }
    }
    None
}

fn title_from_messages(messages: &[MessageV1], platform: Platform) -> String {
    let text = messages
        .iter()
        .find(|m| m.role == MessageRole::User)
        .map(|m| m.content.as_str())
        .unwrap_or("conversation");
    let words: Vec<&str> = text.split_whitespace().take(6).collect();
    if words.is_empty() {
        return format!("{} conversation", platform.slug());
    }
    words
        .join(" ")
        .trim_end_matches(&['.', '!', '?', ':', ';'][..])
        .to_string()
}

fn stable_session_id(platform: Platform, source_id: Option<&str>, path: &Path) -> String {
    let identity = source_id
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| path.to_string_lossy().to_string());
    format!(
        "capture-{}-{:016x}",
        platform.slug(),
        fnv1a(identity.as_bytes())
    )
}

fn metadata_platform(session: &ChatSessionV1) -> String {
    let metadata = serde_json::from_str::<Value>(&session.metadata_json).ok();
    metadata
        .as_ref()
        .and_then(|value| value.get("source_platform"))
        .and_then(Value::as_str)
        .filter(|platform| !platform.is_empty() && *platform != "unknown")
        .or(session.source.as_deref())
        .unwrap_or("unknown")
        .to_ascii_lowercase()
}

fn canonical_duplicate_key(platform: &str, normalized_fingerprint: &str) -> String {
    format!(
        "capture-duplicate-{}-{:016x}",
        platform,
        fnv1a(normalized_fingerprint.as_bytes())
    )
}

fn normalized_visible_message_fingerprint(session: &ChatSessionV1) -> String {
    let mut normalized = String::new();
    for message in &session.messages {
        let content = message
            .content
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let tool_calls = message.tool_calls.as_ref().map(|calls| {
            calls
                .iter()
                .map(|call| (&call.name, &call.arguments))
                .collect::<Vec<_>>()
        });
        let tool_outputs = message.tool_outputs.as_ref().map(|outputs| {
            outputs
                .iter()
                .map(|output| &output.content)
                .collect::<Vec<_>>()
        });
        normalized.push_str(
            &serde_json::to_string(&(&message.role, content, tool_calls, tool_outputs))
                .unwrap_or_default(),
        );
        normalized.push('\n');
    }
    format!("{:016x}", fnv1a(normalized.as_bytes()))
}

pub fn fingerprint_for_session(session: &ChatSessionV1) -> String {
    let mut bytes = Vec::new();
    for message in &session.messages {
        let semantic_message = (
            &message.role,
            &message.content,
            &message.tool_calls,
            &message.tool_outputs,
            &message.id,
            &message.parent_id,
        );
        bytes.extend_from_slice(&serde_json::to_vec(&semantic_message).unwrap_or_default());
        bytes.push(b'\n');
    }
    format!("{:016x}", fnv1a(&bytes))
}

fn fingerprint_bytes(bytes: &[u8]) -> String {
    format!("{:016x}", fnv1a(bytes))
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn metadata_value(session: &ChatSessionV1, key: &str) -> Option<String> {
    serde_json::from_str::<Value>(&session.metadata_json)
        .ok()?
        .get(key)?
        .as_str()
        .map(str::to_string)
}

fn metadata_u64(session: &ChatSessionV1, key: &str) -> Option<u64> {
    serde_json::from_str::<Value>(&session.metadata_json)
        .ok()?
        .get(key)?
        .as_u64()
}

fn set_capture_revision(session: &mut ChatSessionV1, revision: u64) {
    let mut metadata = serde_json::from_str::<Value>(&session.metadata_json)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    metadata.insert("capture_revision".into(), Value::Number(revision.into()));
    session.metadata_json = Value::Object(metadata).to_string();
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn default_roots(platform: Platform) -> Vec<(Platform, PathBuf)> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let mut roots = Vec::new();
    for source in platform.sources() {
        let root = match source {
            Platform::Codex => home.join(".codex/sessions"),
            Platform::ClaudeCode => home.join(".claude/projects"),
            Platform::CopilotCli => home.join(".copilot/session-state"),
            Platform::Cursor => home.join(".cursor/projects"),
            Platform::GeminiCli => home.join(".gemini/tmp"),
            Platform::Antigravity => home.join(".gemini/antigravity-cli"),
            Platform::Generic => continue,
            Platform::All => unreachable!(),
        };
        roots.push((source, root));
    }
    if platform.sources().contains(&Platform::Generic)
        && let Some(import_roots) = std::env::var_os("CRYO_CAPTURE_IMPORT_ROOTS")
    {
        roots.extend(std::env::split_paths(&import_roots).map(|root| (Platform::Generic, root)));
    }
    if platform.sources().contains(&Platform::Antigravity) {
        roots.push((Platform::Antigravity, home.join(".gemini/antigravity-ide")));
        roots.push((Platform::Antigravity, home.join(".gemini/antigravity")));
    }
    roots
}

fn discover_candidates(roots: &[(Platform, PathBuf)]) -> Vec<Candidate> {
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    for (platform, root) in roots {
        collect_files(root, root, &mut candidates, *platform, &mut seen);
    }
    candidates
}

fn collect_files(
    discovery_root: &Path,
    root: &Path,
    out: &mut Vec<Candidate>,
    platform: Platform,
    seen: &mut HashSet<PathBuf>,
) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|name| {
                (name.starts_with('.')
                    && !(platform == Platform::Antigravity && name == ".system_generated"))
                    || name == "node_modules"
            })
        {
            continue;
        }
        if file_type.is_dir() {
            collect_files(discovery_root, &path, out, platform, seen);
        } else if file_type.is_file()
            && allowed_transcript_path(platform, discovery_root, &path)
            && seen.insert(path.clone())
        {
            out.push(Candidate { platform, path });
        }
    }
}

fn allowed_transcript_path(platform: Platform, root: &Path, path: &Path) -> bool {
    let extension = path.extension().and_then(|e| e.to_str());
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let relative = path.strip_prefix(root).unwrap_or(path);
    let components = relative
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect::<Vec<_>>();
    match platform {
        Platform::Codex => extension == Some("jsonl") && file_name.starts_with("rollout-"),
        Platform::ClaudeCode => matches!(extension, Some("jsonl" | "ndjson")),
        Platform::CopilotCli => {
            matches!(extension, Some("json" | "jsonl"))
                && (file_name.eq_ignore_ascii_case("events.jsonl")
                    // Legacy Copilot session snapshots are retained for
                    // backwards-compatible discovery; metadata artifacts are
                    // still excluded.
                    || file_name.eq_ignore_ascii_case("session.json"))
                && (components.contains(&"session-state")
                    || root.file_name().and_then(|name| name.to_str()) == Some("session-state"))
        }
        Platform::Cursor => {
            matches!(extension, Some("json" | "jsonl" | "ndjson"))
                && components.iter().any(|component| {
                    matches!(*component, "agent-transcripts" | "transcripts" | "sessions")
                })
        }
        Platform::GeminiCli => {
            matches!(extension, Some("json" | "jsonl"))
                && components
                    .iter()
                    .any(|component| matches!(*component, "chats" | "sessions" | "tmp"))
                && !file_name.eq_ignore_ascii_case("settings.json")
        }
        Platform::Antigravity => {
            matches!(extension, Some("jsonl"))
                && file_name == "transcript.jsonl"
                && !components.iter().any(|component| {
                    matches!(
                        *component,
                        "history" | "cache" | "settings" | "database" | "databases"
                    )
                })
        }
        Platform::Generic => matches!(extension, Some("json" | "jsonl" | "ndjson")),
        Platform::All => false,
    }
}

const HOOK_MARKER: &str = "cryo-vault:nightly-capture";
const ANTIGRAVITY_HOOK_NAME: &str = "cryo-vault";

#[derive(Debug, Clone)]
struct HookTarget {
    profile: HookProfile,
    path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HookSchema {
    NestedMatcher,
    DirectCommand,
    CopilotCommand,
    NamedCommand,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HookCommandField {
    Command,
    Bash,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HookTimeoutUnits {
    Seconds,
    Milliseconds,
}

#[derive(Debug, Clone, Copy)]
struct HookProfile {
    platform: Platform,
    global_path: &'static str,
    event: &'static str,
    schema: HookSchema,
    command_field: HookCommandField,
    timeout_field: &'static str,
    timeout_value: u64,
    timeout_units: HookTimeoutUnits,
}

const HOOK_PROFILES: [HookProfile; 5] = [
    HookProfile {
        platform: Platform::ClaudeCode,
        global_path: ".claude/settings.json",
        event: "SessionEnd",
        schema: HookSchema::NestedMatcher,
        command_field: HookCommandField::Command,
        timeout_field: "timeout",
        timeout_value: 2,
        timeout_units: HookTimeoutUnits::Seconds,
    },
    HookProfile {
        platform: Platform::Cursor,
        global_path: ".cursor/hooks.json",
        event: "sessionEnd",
        schema: HookSchema::DirectCommand,
        command_field: HookCommandField::Command,
        timeout_field: "",
        timeout_value: 0,
        timeout_units: HookTimeoutUnits::Seconds,
    },
    HookProfile {
        platform: Platform::GeminiCli,
        global_path: ".gemini/settings.json",
        event: "SessionEnd",
        schema: HookSchema::NestedMatcher,
        command_field: HookCommandField::Command,
        timeout_field: "timeout",
        timeout_value: 2000,
        timeout_units: HookTimeoutUnits::Milliseconds,
    },
    HookProfile {
        platform: Platform::CopilotCli,
        global_path: ".copilot/hooks/cryo-vault.json",
        event: "agentStop",
        schema: HookSchema::CopilotCommand,
        command_field: HookCommandField::Bash,
        timeout_field: "timeoutSec",
        timeout_value: 2,
        timeout_units: HookTimeoutUnits::Seconds,
    },
    HookProfile {
        platform: Platform::Antigravity,
        global_path: ".gemini/config/hooks.json",
        event: "Stop",
        schema: HookSchema::NamedCommand,
        command_field: HookCommandField::Command,
        timeout_field: "timeout",
        timeout_value: 2,
        timeout_units: HookTimeoutUnits::Seconds,
    },
];

fn hook_targets(platform: Platform, home: &Path) -> Vec<HookTarget> {
    HOOK_PROFILES
        .iter()
        .copied()
        .filter(|profile| platform == Platform::All || platform == profile.platform)
        .map(|profile| HookTarget {
            profile,
            path: home.join(profile.global_path),
        })
        .collect()
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HookShell {
    Unix,
    PowerShell,
}

fn host_hook_shell() -> HookShell {
    #[cfg(target_os = "windows")]
    {
        HookShell::PowerShell
    }
    #[cfg(not(target_os = "windows"))]
    {
        HookShell::Unix
    }
}

fn capture_executable() -> Result<PathBuf> {
    let executable = std::env::var_os("CRYO_CAPTURE_COMMAND")
        .map(PathBuf::from)
        .or_else(|| {
            dirs::home_dir().map(|home| {
                #[cfg(target_os = "windows")]
                {
                    let mut path = home.join(".cryo-vault/bin/cryo-vault");
                    path.set_extension("exe");
                    path
                }
                #[cfg(not(target_os = "windows"))]
                home.join(".cryo-vault/bin/cryo-vault")
            })
        })
        .ok_or_else(|| anyhow!("could not determine the installed cryo executable"))?;
    Ok(executable)
}

fn capture_command_for_shell(platform: Platform, shell: HookShell) -> Result<String> {
    let executable = capture_executable()?;
    let path = executable.to_string_lossy();
    let args = format!("capture hint --platform {} --stdin", platform.slug());
    Ok(match shell {
        HookShell::Unix => format!("{} {args}", shell_quote_unix(&path)),
        HookShell::PowerShell => format!("& {} {args}", shell_quote_powershell(&path)),
    })
}

fn shell_quote_unix(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn shell_quote_powershell(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn json_object(entries: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    Value::Object(
        entries
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    )
}

fn hook_command(entry: &Value, profile: HookProfile) -> Option<&str> {
    let field = if profile.schema == HookSchema::CopilotCommand {
        match host_hook_shell() {
            HookShell::Unix => "bash",
            HookShell::PowerShell => "powershell",
        }
    } else {
        match profile.command_field {
            HookCommandField::Command => "command",
            HookCommandField::Bash => "bash",
        }
    };
    entry.get(field).and_then(Value::as_str)
}

fn is_cryo_command(command: &str, platform: Platform) -> bool {
    command.contains(&format!(
        "capture hint --platform {} --stdin",
        platform.slug()
    ))
}

fn is_marked_hook(entry: &Value, profile: HookProfile) -> bool {
    entry.get("name").and_then(Value::as_str) == Some(HOOK_MARKER)
        || hook_command(entry, profile)
            .is_some_and(|command| is_cryo_command(command, profile.platform))
}

fn remove_marked_hooks_from_container(container: Option<&mut Value>, profile: HookProfile) -> bool {
    let Some(events) = container.and_then(Value::as_object_mut) else {
        return false;
    };
    let Some(event_hooks) = events.get_mut(profile.event).and_then(Value::as_array_mut) else {
        return false;
    };
    let mut changed = false;
    event_hooks.retain_mut(|entry| {
        if profile.schema == HookSchema::NestedMatcher {
            if let Some(nested) = entry.get_mut("hooks").and_then(Value::as_array_mut) {
                let before = nested.len();
                nested.retain(|hook| !is_marked_hook(hook, profile));
                changed |= nested.len() != before;
                if nested.is_empty() && entry.get("matcher").is_some() {
                    changed = true;
                    return false;
                }
            }
            true
        } else if is_marked_hook(entry, profile) {
            changed = true;
            false
        } else if profile.schema == HookSchema::DirectCommand
            && let Some(nested) = entry.get_mut("hooks").and_then(Value::as_array_mut)
        {
            let before = nested.len();
            nested.retain(|hook| !is_marked_hook(hook, profile));
            changed |= nested.len() != before;
            !(nested.is_empty() && entry.get("matcher").is_some())
        } else {
            true
        }
    });
    changed
}

fn remove_marked_hooks(root: &mut Value, profile: HookProfile) -> bool {
    let legacy_changed = if profile.schema == HookSchema::NamedCommand {
        remove_marked_hooks_from_container(root.get_mut("hooks"), profile)
    } else {
        false
    };
    let container = if profile.schema == HookSchema::NamedCommand {
        root.get_mut(ANTIGRAVITY_HOOK_NAME)
    } else {
        root.get_mut("hooks")
    };
    legacy_changed || remove_marked_hooks_from_container(container, profile)
}

fn hook_command_value(profile: HookProfile, command: &str, shell: HookShell) -> Value {
    let field = if profile.schema == HookSchema::CopilotCommand {
        match shell {
            HookShell::Unix => "bash",
            HookShell::PowerShell => "powershell",
        }
    } else {
        match profile.command_field {
            HookCommandField::Command => "command",
            HookCommandField::Bash => "bash",
        }
    };
    let mut object = Map::new();
    if matches!(
        profile.schema,
        HookSchema::NestedMatcher | HookSchema::CopilotCommand
    ) {
        object.insert("type".into(), Value::String("command".into()));
    }
    if profile.schema == HookSchema::NestedMatcher {
        object.insert("name".into(), Value::String(HOOK_MARKER.into()));
    }
    object.insert(field.into(), Value::String(command.into()));
    if !profile.timeout_field.is_empty() {
        let timeout_value = match profile.timeout_units {
            HookTimeoutUnits::Seconds | HookTimeoutUnits::Milliseconds => profile.timeout_value,
        };
        object.insert(
            profile.timeout_field.into(),
            Value::Number(timeout_value.into()),
        );
    }
    Value::Object(object)
}

fn render_hook_entry(profile: HookProfile, command: &str, shell: HookShell) -> Value {
    let command_entry = hook_command_value(profile, command, shell);
    match profile.schema {
        HookSchema::NestedMatcher => json_object([
            ("matcher", Value::String("*".into())),
            ("hooks", Value::Array(vec![command_entry])),
        ]),
        HookSchema::DirectCommand | HookSchema::CopilotCommand | HookSchema::NamedCommand => {
            command_entry
        }
    }
}

fn install_hook_file(target: &HookTarget, dry_run: bool) -> Result<()> {
    let profile = target.profile;
    let command = capture_command_for_shell(profile.platform, host_hook_shell())?;
    install_hook_file_with_command(target, &command, dry_run)
}

fn install_hook_file_with_command(target: &HookTarget, command: &str, dry_run: bool) -> Result<()> {
    let profile = target.profile;
    let mut root = if target.path.exists() {
        let bytes = fs::read(&target.path).with_context(|| {
            format!(
                "failed to read hook configuration {}",
                target.path.display()
            )
        })?;
        serde_json::from_slice::<Value>(&bytes).with_context(|| {
            format!(
                "failed to decode hook configuration {}",
                target.path.display()
            )
        })?
    } else {
        Value::Object(Map::new())
    };
    if !root.is_object() {
        return Err(anyhow!(
            "hook configuration must be a JSON object: {}",
            target.path.display()
        ));
    }
    if matches!(
        profile.schema,
        HookSchema::CopilotCommand | HookSchema::DirectCommand
    ) {
        root.as_object_mut()
            .expect("checked hook configuration object")
            .entry("version")
            .or_insert_with(|| Value::Number(1.into()));
    }
    remove_marked_hooks(&mut root, profile);
    add_marked_hook(&mut root, profile, command, &target.path)?;

    if !dry_run {
        if let Some(parent) = target.path.parent() {
            fs::create_dir_all(parent)?;
        }
        atomic_write_json(&target.path, &root)?;
    }
    Ok(())
}

fn add_marked_hook(
    root: &mut Value,
    profile: HookProfile,
    command: &str,
    path: &Path,
) -> Result<()> {
    let entry = render_hook_entry(profile, command, host_hook_shell());
    let container_key = if profile.schema == HookSchema::NamedCommand {
        ANTIGRAVITY_HOOK_NAME
    } else {
        "hooks"
    };
    let events = root
        .as_object_mut()
        .expect("checked hook configuration object")
        .entry(container_key)
        .or_insert_with(|| Value::Object(Map::new()));
    let events = events
        .as_object_mut()
        .ok_or_else(|| anyhow!("hook container must be a JSON object: {}", path.display()))?;
    events
        .entry(profile.event)
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| anyhow!("hook event must be a JSON array: {}", path.display()))?
        .push(entry);
    Ok(())
}

fn uninstall_hook_file(target: &HookTarget, dry_run: bool) -> Result<()> {
    if !target.path.exists() {
        return Ok(());
    }
    let bytes = fs::read(&target.path)?;
    let mut root: Value = serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "failed to decode hook configuration {}",
            target.path.display()
        )
    })?;
    let changed = remove_marked_hooks(&mut root, target.profile);
    if !changed || dry_run {
        return Ok(());
    }
    if target.profile.schema == HookSchema::CopilotCommand && only_empty_hook_config(&root) {
        fs::remove_file(&target.path)?;
    } else {
        atomic_write_json(&target.path, &root)?;
    }
    Ok(())
}

fn only_empty_hook_config(root: &Value) -> bool {
    let Some(object) = root.as_object() else {
        return false;
    };
    let hooks_empty = object
        .get("hooks")
        .and_then(Value::as_object)
        .is_some_and(|events| {
            events
                .values()
                .all(|entries| entries.as_array().is_some_and(Vec::is_empty))
        });
    object.iter().all(|(key, value)| {
        key == "version" || (key == "hooks" && value.as_object().is_some() && hooks_empty)
    })
}

fn atomic_write_json(path: &Path, value: &Value) -> Result<()> {
    let temp = path.with_extension("json.tmp");
    fs::write(&temp, serde_json::to_vec_pretty(value)?)?;
    fs::rename(temp, path)?;
    Ok(())
}

pub fn install_hooks(platform: Platform, dry_run: bool) -> Result<Vec<PathBuf>> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("could not determine home directory"))?;
    let targets = hook_targets(platform, &home);
    for target in &targets {
        install_hook_file(target, dry_run)?;
    }
    Ok(targets.into_iter().map(|target| target.path).collect())
}

pub fn uninstall_hooks(platform: Platform, dry_run: bool) -> Result<Vec<PathBuf>> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("could not determine home directory"))?;
    let targets = hook_targets(platform, &home);
    for target in &targets {
        uninstall_hook_file(target, dry_run)?;
    }
    Ok(targets.into_iter().map(|target| target.path).collect())
}

#[derive(Debug, Clone, Serialize)]
pub struct HookStatus {
    pub platform: Platform,
    pub configuration_path: String,
    pub event: String,
    pub installed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

pub fn hook_statuses() -> Result<Vec<HookStatus>> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("could not determine home directory"))?;
    Ok(hook_statuses_for_home(&home))
}

fn hook_statuses_for_home(home: &Path) -> Vec<HookStatus> {
    hook_targets(Platform::All, home)
        .into_iter()
        .map(|target| {
            let profile = target.profile;
            let configuration_path = target.path.to_string_lossy().to_string();
            let missing_reason = format!(
                "hook is missing; run `cryo capture install --platform {}`",
                profile.platform.slug()
            );
            let (installed, malformed) = if !target.path.exists() {
                (false, false)
            } else {
                match fs::read(&target.path)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                {
                    Some(root) => profile_hook_state(&root, profile),
                    None => (false, true),
                }
            };
            let reason = if installed {
                None
            } else if malformed {
                Some(format!(
                    "hook configuration is malformed; run `cryo capture uninstall` then `cryo capture install --platform {}`",
                    profile.platform.slug()
                ))
            } else {
                Some(missing_reason)
            };
            HookStatus {
                platform: profile.platform,
                configuration_path,
                event: profile.event.to_string(),
                installed,
                reason,
            }
        })
        .collect()
}

fn profile_hook_state(root: &Value, profile: HookProfile) -> (bool, bool) {
    let container_key = if profile.schema == HookSchema::NamedCommand {
        ANTIGRAVITY_HOOK_NAME
    } else {
        "hooks"
    };
    let Some(container) = root.get(container_key) else {
        return (false, false);
    };
    let Some(events) = container.as_object() else {
        return (false, true);
    };
    let Some(entries) = events.get(profile.event) else {
        return (false, false);
    };
    let Some(entries) = entries.as_array() else {
        return (false, true);
    };
    for entry in entries {
        match profile.schema {
            HookSchema::NestedMatcher => {
                let Some(nested) = entry.get("hooks").and_then(Value::as_array) else {
                    continue;
                };
                for hook in nested {
                    if is_marked_hook(hook, profile) {
                        let valid = hook.get("type").and_then(Value::as_str) == Some("command")
                            && hook_command(hook, profile).is_some()
                            && hook.get(profile.timeout_field).and_then(Value::as_u64)
                                == Some(profile.timeout_value);
                        return (valid, !valid);
                    }
                }
            }
            HookSchema::DirectCommand | HookSchema::CopilotCommand | HookSchema::NamedCommand => {
                if is_marked_hook(entry, profile) {
                    let field = if profile.schema == HookSchema::CopilotCommand {
                        match host_hook_shell() {
                            HookShell::Unix => "bash",
                            HookShell::PowerShell => "powershell",
                        }
                    } else {
                        "command"
                    };
                    let valid = entry.get(field).and_then(Value::as_str).is_some()
                        && (profile.timeout_field.is_empty()
                            || entry.get(profile.timeout_field).and_then(Value::as_u64)
                                == Some(profile.timeout_value));
                    return (valid, !valid);
                }
            }
        }
    }
    (false, false)
}

fn scheduler_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("could not determine home directory"))?;
    #[cfg(target_os = "macos")]
    {
        return Ok(home.join("Library/LaunchAgents"));
    }
    #[cfg(target_os = "linux")]
    {
        return Ok(home.join(".config/systemd/user"));
    }
    #[cfg(target_os = "windows")]
    {
        return Ok(home.join(".cryo-vault"));
    }
    #[allow(unreachable_code)]
    Err(anyhow!("unsupported operating system"))
}

pub fn scheduler_artifacts() -> Result<Vec<PathBuf>> {
    let dir = scheduler_dir()?;
    #[cfg(target_os = "macos")]
    {
        return Ok(vec![dir.join("com.cryo-vault.nightly.plist")]);
    }
    #[cfg(target_os = "linux")]
    {
        return Ok(vec![
            dir.join("cryo-vault-nightly.service"),
            dir.join("cryo-vault-nightly.timer"),
        ]);
    }
    #[cfg(target_os = "windows")]
    {
        return Ok(Vec::new());
    }
    #[allow(unreachable_code)]
    Ok(Vec::new())
}

#[allow(dead_code)]
fn launchd_domain_for_uid(uid: &str) -> String {
    format!("gui/{uid}")
}

#[allow(dead_code)]
fn launchd_service_target_for_uid(uid: &str, label: &str) -> String {
    format!("{}/{}", launchd_domain_for_uid(uid), label)
}

#[allow(dead_code)]
fn launchd_plist(
    executable: &Path,
    db_path: &Path,
    platform: Platform,
    hour: u8,
    minute: u8,
) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict><key>Label</key><string>com.cryo-vault.nightly</string><key>ProgramArguments</key><array><string>{}</string><string>--db</string><string>{}</string><string>capture</string><string>run</string><string>--platform</string><string>{}</string></array><key>StartCalendarInterval</key><dict><key>Hour</key><integer>{}</integer><key>Minute</key><integer>{}</integer></dict><key>RunAtLoad</key><false/><key>StandardOutPath</key><string>{}</string><key>StandardErrorPath</key><string>{}</string></dict></plist>\n",
        xml_escape(&executable.to_string_lossy()),
        xml_escape(&db_path.to_string_lossy()),
        platform.slug(),
        hour,
        minute,
        xml_escape(&db_path.join("capture.log").to_string_lossy()),
        xml_escape(&db_path.join("capture.err.log").to_string_lossy())
    )
}

#[allow(dead_code)]
fn systemd_service_text(executable: &Path, db_path: &Path, platform: Platform) -> String {
    format!(
        "[Unit]\nDescription=Cryo Vault nightly conversation capture\n\n[Service]\nType=oneshot\nExecStart={} --db {} capture run --platform {}\n",
        systemd_escape_path(&executable.to_string_lossy()),
        systemd_escape_path(&db_path.to_string_lossy()),
        platform.slug()
    )
}

#[allow(dead_code)]
fn systemd_timer_text(hour: u8, minute: u8) -> String {
    format!(
        "[Unit]\nDescription=Run Cryo Vault capture at {:02}:{:02} local time\n\n[Timer]\nOnCalendar=*-*-* {:02}:{:02}:00\nPersistent=true\nUnit=cryo-vault-nightly.service\n\n[Install]\nWantedBy=timers.target\n",
        hour, minute, hour, minute
    )
}

#[allow(dead_code)]
fn windows_command_line(executable: &Path, db_path: &Path, platform: Platform) -> String {
    format!(
        "{} --db {} capture run --platform {}",
        windows_quote(&executable.to_string_lossy()),
        windows_quote(&db_path.to_string_lossy()),
        platform.slug()
    )
}

#[allow(dead_code)]
fn windows_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\\\""))
}

pub fn install_scheduler(
    db_path: &Path,
    platform: Platform,
    time: &str,
    dry_run: bool,
) -> Result<Vec<PathBuf>> {
    let (hour, minute) = parse_schedule_time(time)?;
    let executable = scheduler_executable()?;
    #[cfg(target_os = "macos")]
    {
        let dir = scheduler_dir()?;
        let path = dir.join("com.cryo-vault.nightly.plist");
        let xml = launchd_plist(&executable, db_path, platform, hour, minute);
        if !dry_run {
            fs::create_dir_all(&dir)?;
            if path.exists() && scheduler_installed()? {
                run_checked(
                    "launchctl",
                    &[
                        "bootout",
                        &launchctl_service_target("com.cryo-vault.nightly")?,
                    ],
                    "could not unload the existing Cryo Vault LaunchAgent",
                )?;
            }
            fs::write(&path, xml)?;
            let domain = launchctl_domain()?;
            run_checked(
                "launchctl",
                &["bootstrap", &domain, &path.to_string_lossy()],
                "could not bootstrap the Cryo Vault LaunchAgent",
            )?;
            let target = launchctl_service_target("com.cryo-vault.nightly")?;
            run_checked(
                "launchctl",
                &["kickstart", "-k", &target],
                "could not start the Cryo Vault LaunchAgent",
            )?;
            let mut state = load_state(db_path)?;
            state.schedule = Some(time.to_string());
            save_state(db_path, &state)?;
        }
        return Ok(vec![path]);
    }
    #[cfg(target_os = "linux")]
    {
        let dir = scheduler_dir()?;
        let service = dir.join("cryo-vault-nightly.service");
        let timer = dir.join("cryo-vault-nightly.timer");
        let service_text = systemd_service_text(&executable, db_path, platform);
        let timer_text = systemd_timer_text(hour, minute);
        if !dry_run {
            fs::create_dir_all(&dir)?;
            fs::write(&service, service_text)?;
            fs::write(&timer, timer_text)?;
            run_checked(
                "systemctl",
                &["--user", "daemon-reload"],
                "systemd could not reload the user unit files",
            )?;
            run_checked(
                "systemctl",
                &["--user", "enable", "--now", "cryo-vault-nightly.timer"],
                "systemd could not enable the nightly capture timer",
            )?;
            run_checked(
                "systemctl",
                &[
                    "--user",
                    "is-enabled",
                    "--quiet",
                    "cryo-vault-nightly.timer",
                ],
                "systemd did not report the nightly capture timer as enabled",
            )?;
            let mut state = load_state(db_path)?;
            state.schedule = Some(time.to_string());
            save_state(db_path, &state)?;
        }
        return Ok(vec![service, timer]);
    }
    #[cfg(target_os = "windows")]
    {
        let task = "Cryo Vault Nightly Capture";
        let command = windows_command_line(&executable, db_path, platform);
        if !dry_run {
            let status = std::process::Command::new("schtasks")
                .args([
                    "/Create",
                    "/TN",
                    task,
                    "/SC",
                    "DAILY",
                    "/ST",
                    &format!("{:02}:{:02}", hour, minute),
                    "/TR",
                    &command,
                    "/F",
                ])
                .status()
                .context("failed to invoke Task Scheduler")?;
            if !status.success() {
                return Err(anyhow!("Task Scheduler rejected the nightly capture task"));
            }
            let mut state = load_state(db_path)?;
            state.schedule = Some(time.to_string());
            save_state(db_path, &state)?;
        }
        return Ok(Vec::new());
    }
    #[allow(unreachable_code)]
    Err(anyhow!("unsupported operating system"))
}

pub fn uninstall_scheduler(dry_run: bool) -> Result<Vec<PathBuf>> {
    #[cfg(target_os = "macos")]
    {
        let paths = scheduler_artifacts()?;
        if !dry_run {
            if paths[0].exists() && scheduler_installed()? {
                let target = launchctl_service_target("com.cryo-vault.nightly")?;
                run_checked(
                    "launchctl",
                    &["bootout", &target],
                    "could not unload the Cryo Vault LaunchAgent",
                )?;
            }
            for path in &paths {
                if path.exists() {
                    fs::remove_file(path)?;
                }
            }
        }
        return Ok(paths);
    }
    #[cfg(target_os = "linux")]
    {
        let paths = scheduler_artifacts()?;
        if !dry_run {
            if paths.iter().any(|path| path.exists()) {
                run_checked(
                    "systemctl",
                    &["--user", "disable", "--now", "cryo-vault-nightly.timer"],
                    "systemd could not disable the nightly capture timer",
                )?;
            }
            for path in &paths {
                if path.exists() {
                    fs::remove_file(path)?;
                }
            }
            run_checked(
                "systemctl",
                &["--user", "daemon-reload"],
                "systemd could not reload the user unit files",
            )?;
        }
        return Ok(paths);
    }
    #[cfg(target_os = "windows")]
    {
        if !dry_run {
            let query = std::process::Command::new("schtasks")
                .args(["/Query", "/TN", "Cryo Vault Nightly Capture"])
                .status()
                .context("failed to query Task Scheduler")?;
            if query.success() {
                run_checked(
                    "schtasks",
                    &["/Delete", "/TN", "Cryo Vault Nightly Capture", "/F"],
                    "Task Scheduler could not remove the nightly capture task",
                )?;
            }
        }
        return Ok(Vec::new());
    }
    #[allow(unreachable_code)]
    Err(anyhow!("unsupported operating system"))
}

pub fn scheduler_installed() -> Result<bool> {
    #[cfg(target_os = "macos")]
    {
        let target = launchctl_service_target("com.cryo-vault.nightly")?;
        return Ok(std::process::Command::new("launchctl")
            .args(["print", &target])
            .output()
            .is_ok_and(|output| output.status.success()));
    }
    #[cfg(target_os = "linux")]
    {
        return Ok(std::process::Command::new("systemctl")
            .args([
                "--user",
                "is-enabled",
                "--quiet",
                "cryo-vault-nightly.timer",
            ])
            .status()
            .is_ok_and(|status| status.success()));
    }
    #[cfg(target_os = "windows")]
    {
        return Ok(std::process::Command::new("schtasks")
            .args(["/Query", "/TN", "Cryo Vault Nightly Capture"])
            .output()
            .is_ok_and(|o| o.status.success()));
    }
    #[allow(unreachable_code)]
    Ok(false)
}

fn scheduler_executable() -> Result<PathBuf> {
    std::env::var_os("CRYO_CAPTURE_COMMAND")
        .map(PathBuf::from)
        .or_else(|| {
            dirs::home_dir().map(|home| {
                #[cfg(target_os = "windows")]
                {
                    let mut path = home.join(".cryo-vault/bin/cryo-vault");
                    path.set_extension("exe");
                    path
                }
                #[cfg(not(target_os = "windows"))]
                home.join(".cryo-vault/bin/cryo-vault")
            })
        })
        .ok_or_else(|| anyhow!("could not determine the installed cryo executable"))
}

fn run_checked(program: &str, args: &[&str], context: &str) -> Result<()> {
    let status = std::process::Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("{context}: failed to invoke {program}"))?;
    if !status.success() {
        return Err(anyhow!("{context} (exit status {status})"));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn launchctl_domain() -> Result<String> {
    let output = std::process::Command::new("id")
        .arg("-u")
        .output()
        .context("could not determine the current macOS user")?;
    if !output.status.success() {
        return Err(anyhow!("could not determine the current macOS user"));
    }
    let uid = String::from_utf8(output.stdout)?.trim().to_string();
    Ok(launchd_domain_for_uid(&uid))
}

#[cfg(target_os = "macos")]
fn launchctl_service_target(label: &str) -> Result<String> {
    let domain = launchctl_domain()?;
    Ok(format!("{domain}/{label}"))
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[allow(dead_code)]
fn systemd_escape_path(value: &str) -> String {
    value.replace(' ', "\\x20")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    #[test]
    fn parses_claude_like_jsonl_and_omits_reasoning() {
        let input = br#"{"sessionId":"abc","type":"user","message":{"role":"user","content":[{"type":"text","text":"Fix the nightly collector"}]}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"secret"},{"type":"text","text":"I will inspect it"}]}}
{"type":"tool_result","content":"cargo test"}"#;
        let sessions =
            parse_transcript(Platform::ClaudeCode, Path::new("session.jsonl"), input).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].messages.len(), 3);
        assert!(!sessions[0].extract_full_text().contains("secret"));
    }

    #[test]
    fn capture_is_stable_and_unchanged_on_second_run() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("transcripts");
        fs::create_dir_all(&root).unwrap();
        let file = root.join("session.json");
        fs::write(
            &file,
            json!({"session_id":"one","messages":[{"role":"user","content":"hello"}]}).to_string(),
        )
        .unwrap();
        let db = dir.path().join("db");
        let options = CaptureOptions {
            stable_age_secs: 0,
            ..Default::default()
        };
        let roots = vec![(Platform::Generic, root)];
        let first = run_with_roots(&db, &options, &roots).unwrap();
        let second = run_with_roots(&db, &options, &roots).unwrap();
        assert_eq!(first.imported_sessions, 0);
        assert_eq!(second.imported_sessions, 1);
        assert!(
            run_with_roots(&db, &options, &roots)
                .unwrap()
                .skipped_unchanged
                > 0
        );
    }

    #[test]
    fn versioned_platform_fixtures_extract_visible_sessions() {
        let fixtures = [
            (Platform::Codex, "codex-v1.jsonl", "codex-fixture-1"),
            (
                Platform::ClaudeCode,
                "claude-code-v1.jsonl",
                "claude-fixture-1",
            ),
            (
                Platform::CopilotCli,
                "copilot-cli-v1.jsonl",
                "copilot-fixture-1",
            ),
            (Platform::Cursor, "cursor-v1.jsonl", "cursor-fixture-1"),
            (
                Platform::GeminiCli,
                "gemini-cli-v1.jsonl",
                "gemini-fixture-1",
            ),
            (
                Platform::Antigravity,
                "antigravity-v1.jsonl",
                "antigravity-fixture-1",
            ),
        ];
        for (platform, name, source_id) in fixtures {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/capture")
                .join(name);
            let input = fs::read(&path).unwrap();
            let sessions = parse_transcript(platform, &path, &input).unwrap();
            assert_eq!(sessions.len(), 1, "{platform}");
            let expected_messages = match platform {
                Platform::Codex => 2,
                Platform::Antigravity => 4,
                _ => 3,
            };
            assert_eq!(sessions[0].messages.len(), expected_messages, "{platform}");
            if platform == Platform::Codex {
                assert_eq!(
                    sessions[0]
                        .messages
                        .iter()
                        .filter(|message| message.role == MessageRole::User)
                        .count(),
                    1
                );
                let archive_dir = TempDir::new().unwrap();
                let storage = Storage::new(archive_dir.path().to_path_buf());
                storage.append_session(sessions[0].clone()).unwrap();
                let archived = storage.scan_all().unwrap();
                assert_eq!(archived[0].messages.len(), 2);
                assert_eq!(
                    archived[0]
                        .messages
                        .iter()
                        .filter(|message| message.role == MessageRole::User)
                        .count(),
                    1
                );
            }
            assert_eq!(
                metadata_value(&sessions[0], "platform_session_id").as_deref(),
                Some(source_id)
            );
            assert!(!sessions[0].title.as_deref().unwrap().is_empty());
            assert_eq!(metadata_u64(&sessions[0], "capture_revision"), Some(1));
            assert!(
                !sessions[0]
                    .extract_full_text()
                    .contains("private reasoning")
            );
        }
    }

    #[test]
    fn antigravity_events_preserve_visible_turns_tool_calls_and_metadata() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/capture/antigravity-v1.jsonl");
        let sessions =
            parse_transcript(Platform::Antigravity, &path, &fs::read(&path).unwrap()).unwrap();
        let session = &sessions[0];

        assert_eq!(session.messages[0].role, MessageRole::User);
        assert_eq!(session.messages[1].role, MessageRole::Model);
        assert_eq!(session.messages[2].role, MessageRole::Tool);
        assert_eq!(session.messages[3].role, MessageRole::Model);
        assert_eq!(session.messages[2].content, "path verified");
        assert_eq!(
            session.messages[3].tool_calls.as_ref().unwrap()[0],
            ToolCall {
                name: "list_directory".into(),
                arguments: r#"{"path":"."}"#.into(),
                id: Some("call-1".into()),
            }
        );
        assert_eq!(
            serde_json::from_str::<Value>(&session.messages[1].metadata_json).unwrap()["event_type"],
            "PLANNER_RESPONSE"
        );
        assert_eq!(session.created_at, Some(1_700_000_005));
        assert_eq!(
            metadata_value(session, "platform_session_id").as_deref(),
            Some("antigravity-fixture-1")
        );
        assert!(!session.extract_full_text().contains("private checkpoint"));
        assert_eq!(metadata_u64(session, "malformed_records"), Some(0));
    }

    #[test]
    fn antigravity_path_identity_and_partial_record_recovery_are_stable() {
        let dir = TempDir::new().unwrap();
        let brain_id = "123e4567-e89b-12d3-a456-426614174000";
        let path = dir.path().join(format!(
            "brain/{brain_id}/.system_generated/logs/transcript.jsonl"
        ));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/capture/antigravity-mixed-validity-v1.jsonl");
        let sessions =
            parse_transcript(Platform::Antigravity, &path, &fs::read(&fixture).unwrap()).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].messages.len(), 3);
        assert_eq!(
            metadata_value(&sessions[0], "platform_session_id").as_deref(),
            Some(brain_id)
        );
        assert_eq!(metadata_u64(&sessions[0], "malformed_records"), Some(1));
        assert_eq!(metadata_u64(&sessions[0], "records_read"), Some(4));
    }

    #[test]
    fn antigravity_empty_diagnostics_distinguish_system_only_and_malformed() {
        let fixture_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/capture");
        let system_only = fs::read(fixture_dir.join("antigravity-system-only-v1.jsonl")).unwrap();
        assert!(
            parse_transcript(
                Platform::Antigravity,
                Path::new("system-only.jsonl"),
                &system_only
            )
            .unwrap()
            .is_empty()
        );

        let invalid = fs::read(fixture_dir.join("antigravity-invalid-v1.jsonl")).unwrap();
        assert!(
            parse_transcript(Platform::Antigravity, Path::new("invalid.jsonl"), &invalid).is_err()
        );

        let root = TempDir::new().unwrap();
        let logs = root.path().join("brain/system-only/.system_generated/logs");
        fs::create_dir_all(&logs).unwrap();
        fs::write(logs.join("transcript.jsonl"), &system_only).unwrap();
        let invalid_logs = root.path().join("brain/invalid/.system_generated/logs");
        fs::create_dir_all(&invalid_logs).unwrap();
        fs::write(invalid_logs.join("transcript.jsonl"), &invalid).unwrap();
        let db = root.path().join("db");
        let options = CaptureOptions::default();
        run_with_roots(
            &db,
            &options,
            &[(Platform::Antigravity, root.path().to_path_buf())],
        )
        .unwrap();
        let report = run_with_roots(
            &db,
            &options,
            &[(Platform::Antigravity, root.path().to_path_buf())],
        )
        .unwrap();
        assert_eq!(
            report
                .candidates
                .iter()
                .find(|candidate| candidate.path.contains("system-only"))
                .unwrap()
                .reason
                .as_deref(),
            Some("no visible conversation records")
        );
        assert_eq!(
            report
                .candidates
                .iter()
                .find(|candidate| candidate.path.contains("brain/invalid"))
                .unwrap()
                .reason
                .as_deref(),
            Some("malformed transcript")
        );
    }

    #[test]
    fn antigravity_real_discovery_layout_is_idempotent() {
        let root = TempDir::new().unwrap();
        let logs = root
            .path()
            .join("brain/123e4567-e89b-12d3-a456-426614174001/.system_generated/logs");
        fs::create_dir_all(&logs).unwrap();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/capture/antigravity-v1.jsonl");
        fs::copy(&fixture, logs.join("transcript.jsonl")).unwrap();
        fs::write(
            logs.join("transcript_full.jsonl"),
            fs::read(&fixture).unwrap(),
        )
        .unwrap();
        let db = root.path().join("db");
        let roots = vec![(Platform::Antigravity, root.path().to_path_buf())];
        let options = CaptureOptions {
            dry_run: false,
            ..Default::default()
        };

        let first = run_with_roots(&db, &options, &roots).unwrap();
        assert_eq!(first.imported_sessions, 0);
        let second = run_with_roots(&db, &options, &roots).unwrap();
        assert_eq!(second.imported_sessions, 1);
        assert_eq!(second.extraction_metrics.len(), 1);
        assert_eq!(
            second.extraction_metrics.values().next().unwrap()["visible_messages_extracted"],
            4
        );
        let third = run_with_roots(&db, &options, &roots).unwrap();
        assert_eq!(third.imported_sessions, 0);
        assert!(third.skipped_unchanged > 0);
        assert_eq!(Storage::new(db).scan_all().unwrap().len(), 1);
    }

    #[test]
    fn gemini_modern_records_keep_tool_calls_and_checkpoint_state() {
        let path = Path::new("session-modern.jsonl");
        let input = br#"{"type":"session_metadata","sessionId":"modern-gemini","startTime":"2024-01-01T00:00:00Z"}
{"id":"u1","type":"user","content":[{"text":"old prompt"}]}
{"id":"g1","type":"gemini","content":"old answer"}
{"$rewindTo":"u1"}
{"$set":{"messages":[{"id":"u1","type":"user","content":[{"text":"visible prompt"}]},{"id":"g2","type":"gemini","model":"gemini-2","content":"visible answer","toolCalls":[{"id":"call-1","name":"read_file","args":{"path":"README.md"},"result":[{"text":"file contents"}],"status":"success","timestamp":"2024-01-01T00:00:03Z"}]}]}}
"#;
        let sessions = parse_transcript(Platform::GeminiCli, path, input).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].messages.len(), 2);
        assert_eq!(sessions[0].messages[1].role, MessageRole::Model);
        assert_eq!(
            sessions[0].messages[1].tool_calls.as_ref().unwrap()[0].name,
            "read_file"
        );
        assert_eq!(
            sessions[0].messages[1].tool_outputs.as_ref().unwrap()[0].content,
            "file contents"
        );
        assert_eq!(
            metadata_value(&sessions[0], "platform_session_id").as_deref(),
            Some("modern-gemini")
        );
    }

    #[test]
    fn gemini_rewind_removes_target_and_following_messages() {
        let input = br#"{"type":"session_metadata","sessionId":"rewind-gemini"}
{"id":"u1","type":"user","content":"first prompt"}
{"id":"g1","type":"gemini","content":"first answer"}
{"id":"u2","type":"user","content":"rewound prompt"}
{"id":"g2","type":"gemini","content":"rewound answer"}
{"$rewindTo":"u2"}
"#;
        let sessions =
            parse_transcript(Platform::GeminiCli, Path::new("session.jsonl"), input).unwrap();
        assert_eq!(sessions[0].messages.len(), 2);
        assert_eq!(sessions[0].messages[0].content, "first prompt");
        assert_eq!(sessions[0].messages[1].content, "first answer");
    }

    #[test]
    fn gemini_rewind_to_missing_message_clears_history() {
        let input = br#"{"type":"session_metadata","sessionId":"missing-rewind"}
{"id":"u1","type":"user","content":"first prompt"}
{"id":"g1","type":"gemini","content":"first answer"}
{"$rewindTo":"does-not-exist"}
"#;
        let sessions =
            parse_transcript(Platform::GeminiCli, Path::new("session.jsonl"), input).unwrap();
        assert!(sessions.is_empty());
    }

    #[test]
    fn gemini_discovery_accepts_current_project_chat_jsonl() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join(".gemini");
        let chats = root.join("tmp/project-hash/chats");
        fs::create_dir_all(&chats).unwrap();
        fs::write(chats.join("session-2024.jsonl"), "{}").unwrap();
        fs::write(chats.join("settings.json"), "{}").unwrap();
        let candidates = discover_candidates(&[(Platform::GeminiCli, root)]);
        assert_eq!(candidates.len(), 1);
        assert!(
            candidates[0]
                .path
                .ends_with("tmp/project-hash/chats/session-2024.jsonl")
        );
    }

    #[test]
    fn copilot_events_extract_nested_visible_turns_and_tool_context() {
        let path = Path::new("events.jsonl");
        let input = br#"{"type":"user.message","data":{"sessionId":"nested-1","timestamp":1700000000,"content":"hello"}}
{"type":"assistant.reasoning","data":{"content":"do not persist"}}
{"type":"assistant.message","data":{"sessionId":"nested-1","model":"copilot-x","content":"hi"}}
{"type":"tool.execution_start","data":{"sessionId":"nested-1","toolName":"shell","arguments":{"command":"pwd"}}}
"#;
        let sessions = parse_transcript(Platform::CopilotCli, path, input).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions[0]
                .messages
                .iter()
                .map(|message| (&message.role, message.content.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (&MessageRole::User, "hello"),
                (&MessageRole::Model, "hi"),
                (&MessageRole::Tool, "[tool: shell] {\"command\":\"pwd\"}"),
            ]
        );
        assert_eq!(
            metadata_value(&sessions[0], "source_session_id").as_deref(),
            Some("nested-1")
        );
    }

    #[test]
    fn malformed_and_partial_jsonl_keep_valid_records() {
        let input = br#"not json
{"type":"user","sessionId":"partial","content":"keep this"}
{"type":"assistant","content":[{"type":"text","text":"and this"}]}"#;
        let sessions =
            parse_transcript(Platform::Codex, Path::new("partial.jsonl"), input).unwrap();
        assert_eq!(sessions[0].messages.len(), 2);
    }

    #[test]
    fn session_fingerprint_changes_when_tool_values_change() {
        let mut session = ChatSessionV1 {
            id: "fingerprint".into(),
            title: None,
            source: None,
            model: None,
            created_at: None,
            metadata_json: String::new(),
            messages: vec![MessageV1 {
                role: MessageRole::Model,
                content: "Done".into(),
                tool_calls: Some(vec![ToolCall {
                    name: "read_file".into(),
                    arguments: r#"{"path":"a.txt"}"#.into(),
                    id: Some("call-1".into()),
                }]),
                tool_outputs: None,
                id: None,
                parent_id: None,
                metadata_json: String::new(),
            }],
        };
        let before = fingerprint_for_session(&session);
        session.messages[0].tool_calls.as_mut().unwrap()[0].arguments =
            r#"{"path":"b.txt"}"#.into();
        assert_ne!(before, fingerprint_for_session(&session));
    }

    #[test]
    fn a_path_hint_bypasses_stability_and_is_consumed() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("transcripts");
        fs::create_dir_all(&root).unwrap();
        let file = root.join("session.json");
        fs::write(
            &file,
            json!({"session_id":"hinted","messages":[{"role":"user","content":"hello"}]})
                .to_string(),
        )
        .unwrap();
        let db = dir.path().join("db");
        record_hook_hint(
            &db,
            HookHint {
                platform: Platform::Generic,
                session_id: Some("hinted".into()),
                path: Some(file.to_string_lossy().into()),
                seen_at: now_secs(),
            },
        )
        .unwrap();

        let report = run_with_roots(&db, &CaptureOptions::default(), &[]).unwrap();
        assert_eq!(report.imported_sessions, 1);
        assert!(load_state(&db).unwrap().pending_hooks.is_empty());
    }

    #[test]
    fn pathless_hints_are_retained_until_a_session_resolves() {
        let dir = TempDir::new().unwrap();
        let db = dir.path().join("db");
        record_hook_hint(
            &db,
            HookHint {
                platform: Platform::ClaudeCode,
                session_id: Some("not-yet-on-disk".into()),
                path: None,
                seen_at: now_secs(),
            },
        )
        .unwrap();
        run_with_roots(&db, &CaptureOptions::default(), &[]).unwrap();
        assert_eq!(load_state(&db).unwrap().pending_hooks.len(), 1);
    }

    #[test]
    fn discovery_uses_platform_allowlists() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("copilot");
        fs::create_dir_all(root.join("session-state")).unwrap();
        fs::write(root.join("settings.json"), "{}").unwrap();
        fs::write(root.join("session-state/session.json"), "{}").unwrap();
        let candidates = discover_candidates(&[(Platform::CopilotCli, root)]);
        assert_eq!(candidates.len(), 1);
        assert!(candidates[0].path.ends_with("session.json"));
    }

    #[test]
    fn hook_merge_is_idempotent_and_preserves_unrelated_configuration() {
        let mut root = json!({
            "permissions": {"allow": ["cargo test"]},
            "hooks": {"SessionEnd": [{
                "matcher": "*",
                "hooks": [{"name": "user-hook", "type": "command", "command": "user-command"}]
            }]}
        });
        add_marked_hook(
            &mut root,
            HOOK_PROFILES[0],
            "cryo capture hint --stdin",
            Path::new("settings.json"),
        )
        .unwrap();
        remove_marked_hooks(&mut root, HOOK_PROFILES[0]);
        add_marked_hook(
            &mut root,
            HOOK_PROFILES[0],
            "cryo capture hint --stdin",
            Path::new("settings.json"),
        )
        .unwrap();
        let serialized = root.to_string();
        assert!(serialized.contains("user-command"));
        assert_eq!(serialized.matches(HOOK_MARKER).count(), 1);
        assert!(serialized.contains("cargo test"));
    }

    #[test]
    fn all_hook_targets_exclude_scanner_only_codex() {
        let targets = hook_targets(Platform::All, Path::new("/tmp/cryo-home"));
        assert_eq!(targets.len(), 5);
        assert!(
            targets
                .iter()
                .all(|target| target.profile.platform != Platform::Codex)
        );
        assert!(hook_targets(Platform::Codex, Path::new("/tmp/cryo-home")).is_empty());
    }

    #[test]
    fn native_hook_snapshots_match_each_client_schema() {
        let command = "/opt/Cryo Vault/bin/cryo-vault capture hint --platform claude-code --stdin";
        assert_eq!(
            render_hook_entry(HOOK_PROFILES[0], command, HookShell::Unix),
            json!({
                "matcher": "*",
                "hooks": [{
                    "type": "command",
                    "name": HOOK_MARKER,
                    "command": command,
                    "timeout": 2
                }]
            })
        );
        assert_eq!(
            render_hook_entry(HOOK_PROFILES[1], command, HookShell::Unix),
            json!({"command": command})
        );
        assert_eq!(
            render_hook_entry(HOOK_PROFILES[2], command, HookShell::Unix),
            json!({
                "matcher": "*",
                "hooks": [{
                    "type": "command",
                    "name": HOOK_MARKER,
                    "command": command,
                    "timeout": 2000
                }]
            })
        );
        assert_eq!(
            render_hook_entry(HOOK_PROFILES[3], command, HookShell::Unix),
            json!({"type": "command", "bash": command, "timeoutSec": 2})
        );
        assert_eq!(
            render_hook_entry(HOOK_PROFILES[3], command, HookShell::PowerShell),
            json!({"type": "command", "powershell": command, "timeoutSec": 2})
        );
        assert_eq!(
            render_hook_entry(HOOK_PROFILES[4], command, HookShell::Unix),
            json!({"command": command, "timeout": 2})
        );
        assert_eq!(HOOK_PROFILES[3].event, "agentStop");
    }

    #[test]
    fn hook_install_repeat_and_uninstall_preserve_unrelated_settings() {
        let dir = TempDir::new().unwrap();
        for profile in HOOK_PROFILES {
            let path = dir.path().join(profile.global_path);
            let target = HookTarget {
                profile,
                path: path.clone(),
            };
            let root = json!({
                "permissions": {"allow": ["user-command"]},
                "hooks": {profile.event: []}
            });
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, serde_json::to_vec(&root).unwrap()).unwrap();
            let command = format!(
                "'/opt/Cryo Vault/bin/cryo-vault' capture hint --platform {} --stdin",
                profile.platform.slug()
            );
            install_hook_file_with_command(&target, &command, false).unwrap();
            install_hook_file_with_command(&target, &command, false).unwrap();
            let installed: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            assert_eq!(
                installed["permissions"]["allow"][0],
                Value::String("user-command".into())
            );
            if profile.schema == HookSchema::NamedCommand {
                assert!(installed.get("hooks").is_some());
                assert_eq!(
                    installed[ANTIGRAVITY_HOOK_NAME][profile.event]
                        .as_array()
                        .unwrap()
                        .len(),
                    1
                );
                assert!(profile_hook_state(&installed, profile).0);
            } else {
                assert_eq!(
                    installed["hooks"][profile.event].as_array().unwrap().len(),
                    1
                );
            }
            uninstall_hook_file(&target, false).unwrap();
            let uninstalled: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            assert_eq!(
                uninstalled["permissions"]["allow"][0],
                Value::String("user-command".into())
            );
            if profile.schema == HookSchema::NamedCommand {
                assert!(
                    uninstalled[ANTIGRAVITY_HOOK_NAME][profile.event]
                        .as_array()
                        .unwrap()
                        .is_empty()
                );
                assert!(!profile_hook_state(&uninstalled, profile).0);
            } else {
                assert!(
                    uninstalled["hooks"][profile.event]
                        .as_array()
                        .unwrap()
                        .is_empty()
                );
            }
        }
    }

    #[test]
    fn antigravity_uses_named_top_level_hook_and_status_ignores_legacy_shape() {
        let profile = HOOK_PROFILES[4];
        let command = "cryo capture hint --platform antigravity --stdin";
        let mut legacy = json!({
            "hooks": {profile.event: [render_hook_entry(profile, command, HookShell::Unix)]}
        });
        assert_eq!(profile_hook_state(&legacy, profile), (false, false));
        assert!(remove_marked_hooks(&mut legacy, profile));
        add_marked_hook(&mut legacy, profile, command, Path::new("hooks.json")).unwrap();
        assert!(legacy[ANTIGRAVITY_HOOK_NAME][profile.event].is_array());
        assert!(
            legacy["hooks"][profile.event]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(profile_hook_state(&legacy, profile).0);
    }

    #[test]
    fn queue_preserves_concurrent_hints_and_consumes_them_once() {
        let dir = TempDir::new().unwrap();
        let db = dir.path().join("db");
        let mut files = Vec::new();
        for index in 0..12 {
            let path = dir.path().join(format!("session-{index}.json"));
            fs::write(
                &path,
                json!({
                    "session_id": format!("queued-{index}"),
                    "messages": [{"role": "user", "content": format!("queued {index}")}]
                })
                .to_string(),
            )
            .unwrap();
            files.push(path);
        }
        let handles = files
            .iter()
            .enumerate()
            .map(|(index, path)| {
                let db = db.clone();
                let path = path.clone();
                std::thread::spawn(move || {
                    record_hook_hint(
                        &db,
                        HookHint {
                            platform: Platform::Generic,
                            session_id: Some(format!("queued-{index}")),
                            path: Some(path.to_string_lossy().into()),
                            seen_at: index as u64,
                        },
                    )
                    .unwrap();
                })
            })
            .collect::<Vec<_>>();
        for handle in handles {
            handle.join().unwrap();
        }
        let queue = db.join(HINT_QUEUE_DIR);
        assert_eq!(fs::read_dir(&queue).unwrap().count(), files.len());
        let report = run_with_roots(&db, &CaptureOptions::default(), &[]).unwrap();
        assert_eq!(report.imported_sessions, files.len());
        assert!(load_state(&db).unwrap().pending_hooks.is_empty());
        assert!(!queue.exists());
        let second = run_with_roots(&db, &CaptureOptions::default(), &[]).unwrap();
        assert_eq!(second.imported_sessions, 0);
    }

    #[test]
    fn duplicate_hook_delivery_does_not_duplicate_a_capture() {
        let dir = TempDir::new().unwrap();
        let db = dir.path().join("db");
        let file = dir.path().join("session.json");
        fs::write(
            &file,
            json!({
                "session_id": "duplicate-hook",
                "messages": [{"role": "user", "content": "one hook"}]
            })
            .to_string(),
        )
        .unwrap();
        let hint = HookHint {
            platform: Platform::Generic,
            session_id: Some("duplicate-hook".into()),
            path: Some(file.to_string_lossy().into()),
            seen_at: 1,
        };
        record_hook_hint(&db, hint.clone()).unwrap();
        record_hook_hint(&db, hint).unwrap();

        let report = run_with_roots(&db, &CaptureOptions::default(), &[]).unwrap();
        assert_eq!(report.imported_sessions, 1);
        assert!(load_state(&db).unwrap().pending_hooks.is_empty());
        assert_eq!(Storage::new(db).scan_all().unwrap().len(), 1);
    }

    #[test]
    fn duplicate_hint_for_captured_file_is_consumed_before_resume() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("transcripts");
        fs::create_dir_all(&root).unwrap();
        let file = root.join("session.json");
        fs::write(
            &file,
            json!({
                "session_id": "resumable-hint",
                "messages": [{"role": "user", "content": "initial"}]
            })
            .to_string(),
        )
        .unwrap();
        let db = dir.path().join("db");
        let roots = vec![(Platform::Generic, root)];

        assert_eq!(
            run_with_roots(&db, &CaptureOptions::default(), &roots)
                .unwrap()
                .imported_sessions,
            0
        );
        assert_eq!(
            run_with_roots(&db, &CaptureOptions::default(), &roots)
                .unwrap()
                .imported_sessions,
            1
        );

        record_hook_hint(
            &db,
            HookHint {
                platform: Platform::Generic,
                session_id: Some("resumable-hint".into()),
                path: Some(file.to_string_lossy().into()),
                seen_at: now_secs(),
            },
        )
        .unwrap();
        let unchanged = run_with_roots(&db, &CaptureOptions::default(), &roots).unwrap();
        assert_eq!(unchanged.imported_sessions, 0);
        assert!(load_state(&db).unwrap().pending_hooks.is_empty());

        fs::write(
            &file,
            json!({
                "session_id": "resumable-hint",
                "messages": [{"role": "user", "content": "resumed"}]
            })
            .to_string(),
        )
        .unwrap();
        let resumed = run_with_roots(&db, &CaptureOptions::default(), &roots).unwrap();
        assert_eq!(resumed.imported_sessions, 0);
        assert_eq!(resumed.skipped_unstable, 1);
    }

    #[test]
    fn antigravity_discovery_only_accepts_cli_transcripts() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("antigravity-cli");
        let logs = root.join("brain/id/.system_generated/logs");
        fs::create_dir_all(&logs).unwrap();
        fs::write(logs.join("transcript.jsonl"), "{}").unwrap();
        fs::write(logs.join("transcript_full.jsonl"), "{}").unwrap();
        for excluded in ["history", "cache", "settings", "database"] {
            let excluded_dir = root.join(excluded);
            fs::create_dir_all(&excluded_dir).unwrap();
            fs::write(excluded_dir.join("transcript.jsonl"), "{}").unwrap();
        }
        let candidates = discover_candidates(&[(Platform::Antigravity, root)]);
        assert_eq!(candidates.len(), 1);
        assert!(
            candidates[0]
                .path
                .ends_with(".system_generated/logs/transcript.jsonl")
        );
    }

    #[test]
    fn scheduler_command_helpers_quote_and_target_each_os() {
        assert_eq!(launchd_domain_for_uid("42"), "gui/42");
        assert_eq!(
            launchd_service_target_for_uid("42", "com.cryo-vault.nightly"),
            "gui/42/com.cryo-vault.nightly"
        );
        let executable = Path::new("C:/Program Files/Cryo Vault/cryo-vault.exe");
        let db = Path::new("C:/Users/A User/.cryo");
        assert_eq!(
            windows_command_line(executable, db, Platform::All),
            "\"C:/Program Files/Cryo Vault/cryo-vault.exe\" --db \"C:/Users/A User/.cryo\" capture run --platform all"
        );
        assert!(systemd_timer_text(1, 15).contains("OnCalendar=*-*-* 01:15:00"));
        assert!(
            launchd_plist(
                Path::new("/bin/cryo"),
                Path::new("/db"),
                Platform::All,
                23,
                0
            )
            .contains("<integer>23</integer>")
        );
    }
}
