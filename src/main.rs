use anyhow::Result;
use chrono::NaiveDate;
use clap::{Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};
use std::io::{self, BufRead, Read, Write};
use std::path::PathBuf;
use std::time::Duration;

use cryo_vault::capture::{self, CaptureOptions, HookHint, Platform};
use cryo_vault::lock::CryoLock;
use cryo_vault::schema::{ChatGptConversation, ChatSessionInput, ChatSessionV1, StreamEvent};
use cryo_vault::storage::Storage;

#[derive(Parser)]
#[command(name = "cryo")]
#[command(version)]
#[command(
    about = "High-performance AI Log Archiver",
    long_about = "High-performance AI Log Archiver\n\nLogging:\n  Control logging verbosity using the RUST_LOG environment variable.\n  Levels: trace, debug, info, warn, error\n  Default: warn\n  Example: RUST_LOG=debug cryo add session.json"
)]
struct Cli {
    /// Override database path (Default: ~/.cryo)
    #[arg(long, env = "CRYO_DB_PATH")]
    db: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Discover and archive local coding-agent transcripts
    Capture {
        #[command(subcommand)]
        command: CaptureCommands,
    },
    /// Ingest a chat log (File or Stdin)
    Add {
        /// Input file (Use "-" for stdin)
        #[arg(default_value = "-")]
        file: String,

        /// Treat input as streaming output (One JSON object per line)
        #[arg(long)]
        stream: bool,
    },

    /// Flush pending sessions to the database
    Flush,

    /// Search the archive
    Search {
        /// Query string (Regex supported)
        query: String,

        /// Filter by date after (YYYY-MM-DD or Unix timestamp)
        #[arg(long)]
        after: Option<String>,

        /// Filter by date before (YYYY-MM-DD or Unix timestamp)
        #[arg(long)]
        before: Option<String>,

        /// Output Raw JSON
        #[arg(long)]
        json: bool,
    },

    /// Show stats
    Stats,

    /// Show first N sessions (oldest)
    First {
        /// Number of sessions to show
        #[arg(default_value = "10")]
        count: usize,
        /// Filter by captured source platform
        #[arg(long)]
        source: Option<String>,
    },

    /// Show last N sessions (newest)
    Last {
        /// Number of sessions to show
        #[arg(default_value = "10")]
        count: usize,
        /// Filter by captured source platform
        #[arg(long)]
        source: Option<String>,
    },
    /// Audit archive provenance
    Audit {
        #[command(subcommand)]
        command: AuditCommands,
    },

    /// Show full session details
    Show {
        /// Session ID
        session_id: Option<String>,
        /// Render provenance and extraction metrics instead of message bodies
        #[arg(long)]
        diagnostics: bool,
    },

    /// Rebuild index from existing data files
    Reindex {
        /// Skip confirmation prompt
        #[arg(long)]
        yes: bool,
    },

    /// Optimise database into ~256KB compressed blocks
    Optimise {
        /// Target compressed block size in KB
        #[arg(long, default_value = "256")]
        chunk_kb: usize,

        /// Skip confirmation prompt
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum AuditCommands {
    /// List sessions whose original source cannot be reconstructed
    Provenance,
}

#[derive(Subcommand)]
enum CaptureCommands {
    /// Run one capture pass
    Run {
        /// Source platform to scan (default: all)
        #[arg(long, default_value = "all")]
        platform: String,
        /// Scheduled time used for validation and reporting
        #[arg(long, default_value = "23:00")]
        time: String,
        /// Inspect and parse without writing archive or state files
        #[arg(long)]
        dry_run: bool,
        /// Print one diagnostic for every discovered candidate
        #[arg(short = 'v', long, conflicts_with = "json")]
        verbose: bool,
        /// Print aggregate and per-candidate diagnostics as JSON
        #[arg(long)]
        json: bool,
        /// Wait before re-observing unchanged candidates (for example: 2s, 500ms)
        #[arg(long, value_parser = parse_settle_duration)]
        settle: Option<Duration>,
    },
    /// Install the native nightly scheduler
    Install {
        /// Source platform to document in the scheduled job (default: all)
        #[arg(long, default_value = "all")]
        platform: String,
        /// Local time in HH:MM format
        #[arg(long, default_value = "23:00")]
        time: String,
        /// Show scheduler changes without applying them
        #[arg(long)]
        dry_run: bool,
    },
    /// Show capture state and scheduler status
    Status {
        /// Print machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    /// Remove the native nightly scheduler (archive data is retained)
    Uninstall {
        /// Show scheduler changes without applying them
        #[arg(long)]
        dry_run: bool,
    },
    /// Record a lightweight end-of-session hint for the next collector run
    #[command(hide = true)]
    Hint {
        #[arg(long)]
        platform: String,
        #[arg(long)]
        session_id: Option<String>,
        #[arg(long)]
        path: Option<PathBuf>,
        /// Read session_id/transcript_path from the lifecycle hook's JSON stdin.
        #[arg(long)]
        stdin: bool,
    },
}

/// Parse date string (YYYY-MM-DD) or Unix timestamp
fn parse_date_or_timestamp(input: &str) -> Result<u64> {
    // Try parsing as Unix timestamp first
    if let Ok(ts) = input.parse::<u64>() {
        return Ok(ts);
    }

    // Try parsing as YYYY-MM-DD
    if let Ok(date) = NaiveDate::parse_from_str(input, "%Y-%m-%d") {
        let datetime = date
            .and_hms_opt(0, 0, 0)
            .ok_or_else(|| anyhow::anyhow!("Invalid date"))?;
        return Ok(datetime.and_utc().timestamp() as u64);
    }

    Err(anyhow::anyhow!(
        "Invalid date format. Use YYYY-MM-DD or Unix timestamp"
    ))
}

fn parse_settle_duration(input: &str) -> Result<Duration, String> {
    capture::parse_settle_duration(input).map_err(|error| error.to_string())
}

fn capture_rerun_command(platform: Platform) -> String {
    if platform == Platform::All {
        "cryo capture run".to_string()
    } else {
        format!("cryo capture run --platform {platform}")
    }
}

/// Helper function to print a list of sessions with previews
fn print_session_list(sessions: &[ChatSessionV1]) {
    for session in sessions {
        println!(
            "[{}] {}",
            session.id,
            session.title.as_deref().unwrap_or("Untitled")
        );
        println!(
            "  Source: {}",
            session.source.as_deref().unwrap_or("unknown")
        );
        println!("  Model: {}", session.model.as_deref().unwrap_or("unknown"));
        println!(
            "  Created: {}",
            session
                .created_at
                .map(|created| created.to_string())
                .unwrap_or_else(|| "unknown".into())
        );
        println!("  Messages: {}", session.messages.len());
        println!();
    }
}

/// Helper function to display first or last N sessions
fn display_sessions(
    storage: Storage,
    count: usize,
    first: bool,
    source: Option<String>,
) -> Result<()> {
    let mut sessions = storage.scan_all()?;
    if let Some(source) = source {
        let platform: Platform = source.parse()?;
        let slug = platform.slug();
        sessions.retain(|session| session.source.as_deref() == Some(slug));
    }
    // Stable sorting preserves archive insertion order when timestamps are
    // absent, while timestamped captured sessions are shown chronologically.
    sessions.sort_by_key(|session| session.created_at);
    let len = sessions.len();

    let (start, end, desc) = if first {
        let end = std::cmp::min(len, count);
        (0, end, "first")
    } else {
        let start = len.saturating_sub(count);
        (start, len, "last")
    };

    println!("Showing {} {} of {} sessions:\n", desc, end - start, len);
    print_session_list(&sessions[start..end]);
    Ok(())
}

/// Handles the 'add' command
///
/// This function locks the database and dispatches to either streaming or file mode.
fn handle_add(db_path: PathBuf, file: String, stream: bool) -> Result<()> {
    let _lock = CryoLock::acquire(&db_path, 5000)?;
    let storage = Storage::new(db_path.clone());

    if stream {
        handle_add_stream(storage, file)
    } else {
        handle_add_file(storage, db_path, file)
    }
}

/// Handles streaming input for 'add' command
///
/// Reads events line-by-line from stdin or a file and appends them to the WAL.
///
/// Use efficient buffering for writes.
/// Supports both stdin ("-") and file inputs.
fn handle_add_stream(storage: Storage, file: String) -> Result<()> {
    let stdin = io::stdin();
    let handle = stdin.lock();
    let mut wal_writer = storage.get_wal_writer()?;

    let reader: Box<dyn BufRead> = if file == "-" {
        Box::new(handle)
    } else {
        Box::new(io::BufReader::new(std::fs::File::open(file)?))
    };

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let event: StreamEvent = serde_json::from_str(&line)?;
        wal_writer.append(event)?;
    }
    wal_writer.flush()?;

    let archived = storage.flush_pending()?;
    if archived > 0 {
        println!("Archived {} sessions.", archived);
    }
    Ok(())
}

/// Handles file input for 'add' command
///
/// Tries multiple formats in order:
/// 1. Single Session (ChatSessionInput)
/// 2. ChatGPT Export (Vec<ChatGptConversation>)
/// 3. Array of Sessions (Vec<ChatSessionInput>)
fn handle_add_file(storage: Storage, db_path: PathBuf, file: String) -> Result<()> {
    let content = if file == "-" {
        let mut buf = String::new();
        io::stdin().read_to_string(&mut buf)?;
        buf
    } else {
        std::fs::read_to_string(&file)?
    };

    if let Ok(input) = serde_json::from_str::<ChatSessionInput>(&content) {
        let session = with_ingestion_metadata(input.into(), "add", &file);
        storage.append_pending(session)?;
        println!("Session saved to {}", db_path.display());
        return Ok(());
    }

    if let Ok(conversations) = serde_json::from_str::<Vec<ChatGptConversation>>(&content) {
        let count = conversations.len();
        println!(
            "Detected ChatGPT export format. Importing {} conversations...",
            count
        );
        let pb = ProgressBar::new(count as u64);
        pb.set_style(
            ProgressStyle::default_bar()
                .template(
                    "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta})",
                )
                .unwrap()
                .progress_chars("#>-"),
        );

        let mut sessions_to_import = Vec::with_capacity(count);
        for conv in conversations {
            match conv.try_into() {
                Ok(session) => {
                    sessions_to_import.push(with_ingestion_metadata(session, "add", &file));
                    pb.inc(1);
                }
                Err(e) => {
                    eprintln!("Warning: Failed to convert conversation: {}", e);
                    pb.inc(1);
                }
            }
        }

        let actual_count = sessions_to_import.len();
        if actual_count > 0 {
            storage.append_bulk(sessions_to_import)?;
        }

        pb.finish_with_message("Import complete");
        println!(
            "Imported {} ChatGPT conversations to {}",
            actual_count,
            db_path.display()
        );
        return Ok(());
    }

    match serde_json::from_str::<Vec<ChatSessionInput>>(&content) {
        Ok(sessions) => {
            let count = sessions.len();
            println!("Importing {} sessions...", count);
            let pb = ProgressBar::new(count as u64);
            pb.set_style(ProgressStyle::default_bar()
                .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta})")
                .unwrap()
                .progress_chars("#>-"));

            let mut sessions_to_import = Vec::with_capacity(count);
            for input in sessions {
                sessions_to_import.push(with_ingestion_metadata(input.into(), "add", &file));
                pb.inc(1);
            }

            if count > 0 {
                storage.append_bulk(sessions_to_import)?;
            }

            pb.finish_with_message("Import complete");
            println!("Imported {} sessions to {}", count, db_path.display());
            Ok(())
        }
        Err(e) => Err(anyhow::anyhow!(
            "Failed to parse input. Tried formats: Single Session, ChatGPT Export, Array of Sessions.\nLast error: {}",
            e
        )),
    }
}

/// Handles the 'search' command
fn handle_search(
    db_path: PathBuf,
    query: String,
    after: Option<String>,
    before: Option<String>,
    json: bool,
) -> Result<()> {
    let storage = Storage::new(db_path);

    // Parse date/timestamp arguments
    let after_ts = after
        .as_ref()
        .map(|s| parse_date_or_timestamp(s))
        .transpose()?;
    let before_ts = before
        .as_ref()
        .map(|s| parse_date_or_timestamp(s))
        .transpose()?;

    // Use the optimized Index Search with time range filtering
    let sessions = storage.search(&query, after_ts, before_ts)?;

    if sessions.is_empty() && !json {
        println!("No matches found.");
    }

    for session in sessions {
        if json {
            println!("{}", serde_json::to_string(&session)?);
        } else {
            println!(
                "[{}] {}",
                session.id,
                session.title.as_deref().unwrap_or("Untitled")
            );
        }
    }
    Ok(())
}

/// Handles the 'stats' command
fn handle_stats(db_path: PathBuf) -> Result<()> {
    let storage = Storage::new(db_path);
    match storage.get_stats() {
        Ok(stats) => {
            println!("Database Statistics");
            println!("===================");
            println!("Active File:      {}", stats.file_name);
            println!("Total Sessions:   {}", stats.session_count);
            println!("Total Messages:   {}", stats.message_count);
            println!(
                "Disk Usage:       {:.2} MB",
                stats.total_size_bytes as f64 / 1024.0 / 1024.0
            );

            if stats.min_time > 0 {
                use chrono::DateTime;
                let start = DateTime::from_timestamp(stats.min_time as i64, 0)
                    .map(|dt| dt.to_string())
                    .unwrap_or_else(|| stats.min_time.to_string());
                let end = DateTime::from_timestamp(stats.max_time as i64, 0)
                    .map(|dt| dt.to_string())
                    .unwrap_or_else(|| stats.max_time.to_string());
                println!("Time Range:       {} to {}", start, end);
            }
        }
        Err(e) => eprintln!("Error calculating stats: {}", e),
    }
    Ok(())
}

/// Handles the 'first' command
fn handle_first(db_path: PathBuf, count: usize, source: Option<String>) -> Result<()> {
    let storage = Storage::new(db_path);
    display_sessions(storage, count, true, source)
}

/// Handles the 'last' command
fn handle_last(db_path: PathBuf, count: usize, source: Option<String>) -> Result<()> {
    let storage = Storage::new(db_path);
    display_sessions(storage, count, false, source)
}

/// Handles the 'show' command
fn handle_show(db_path: PathBuf, session_id: Option<String>, diagnostics: bool) -> Result<()> {
    let storage = Storage::new(db_path);

    if diagnostics {
        let sessions = storage.scan_all()?;
        let selected: Vec<&ChatSessionV1> = session_id
            .as_deref()
            .map(|id| sessions.iter().filter(|session| session.id == id).collect())
            .unwrap_or_else(|| sessions.iter().collect());
        for session in selected {
            println!("Session: {}", session.id);
            print_session_diagnostics(session);
        }
        return Ok(());
    }

    let Some(session_id) = session_id else {
        return Err(anyhow::anyhow!(
            "show requires a session ID unless --diagnostics is used"
        ));
    };

    match storage.get_session_by_id(&session_id)? {
        Some(session) => {
            println!("Session: {}", session.id);
            println!("Title: {}", session.title.as_deref().unwrap_or("Untitled"));
            if let Some(source) = &session.source {
                println!("Source: {}", source);
            }
            if let Some(model) = &session.model {
                println!("Model: {}", model);
            }
            if let Some(created) = session.created_at {
                use chrono::DateTime;
                if let Some(dt) = DateTime::from_timestamp(created as i64, 0) {
                    let formatted = dt.format("%Y-%m-%d %H:%M:%S UTC");
                    println!("Created: {} ({})", created, formatted);
                } else {
                    println!("Created: {}", created);
                }
            }
            println!("\nMessages ({}):\n", session.messages.len());

            for (i, msg) in session.messages.iter().enumerate() {
                println!("--- Message {} ({:?}) ---", i + 1, msg.role);
                println!("{}", msg.content);
                println!();
            }
        }
        None => {
            println!("Session not found: {}", session_id);
        }
    }
    Ok(())
}

fn print_session_diagnostics(session: &ChatSessionV1) {
    let metadata = serde_json::from_str::<serde_json::Value>(&session.metadata_json)
        .unwrap_or_else(|_| serde_json::json!({}));
    println!(
        "  Source: {}",
        session.source.as_deref().unwrap_or("unknown")
    );
    println!("  Model: {}", session.model.as_deref().unwrap_or("unknown"));
    println!(
        "  Created: {}",
        session
            .created_at
            .map(|v| v.to_string())
            .unwrap_or_else(|| "unknown".into())
    );
    println!("  Messages: {}", session.messages.len());
    for key in [
        "importer",
        "parser_version",
        "ingest_time",
        "capture_revision",
        "duplicate_key",
        "source_platform",
        "source_path",
        "source_session_id",
        "records_read",
        "visible_messages_extracted",
        "records_skipped_by_reason",
        "malformed_records",
    ] {
        if let Some(value) = metadata.get(key) {
            println!("  {key}: {value}");
        }
    }
}

fn with_ingestion_metadata(
    mut session: ChatSessionV1,
    importer: &str,
    source_path: &str,
) -> ChatSessionV1 {
    let mut metadata = serde_json::from_str::<serde_json::Value>(&session.metadata_json)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    metadata.insert(
        "importer".into(),
        serde_json::Value::String(importer.into()),
    );
    metadata.insert(
        "parser_version".into(),
        serde_json::Value::String("ingest-v1".into()),
    );
    metadata.insert(
        "ingest_time".into(),
        serde_json::Value::Number(chrono::Utc::now().timestamp().max(0).into()),
    );
    metadata.insert(
        "source_platform".into(),
        serde_json::Value::String(session.source.clone().unwrap_or_else(|| "unknown".into())),
    );
    metadata.insert(
        "source_path".into(),
        serde_json::Value::String(
            if source_path == "-" {
                "unknown"
            } else {
                source_path
            }
            .into(),
        ),
    );
    metadata
        .entry("source_session_id")
        .or_insert_with(|| serde_json::Value::String(session.id.clone()));
    metadata.entry("records_read").or_insert_with(|| {
        serde_json::Value::Number(serde_json::Number::from(session.messages.len() as u64))
    });
    metadata.insert(
        "visible_messages_extracted".into(),
        serde_json::Value::Number(serde_json::Number::from(session.messages.len() as u64)),
    );
    metadata
        .entry("records_skipped_by_reason")
        .or_insert_with(|| serde_json::json!({}));
    metadata
        .entry("malformed_records")
        .or_insert_with(|| serde_json::Value::Number(0.into()));
    session.metadata_json = serde_json::Value::Object(metadata).to_string();
    session
}

fn handle_audit_provenance(db_path: PathBuf) -> Result<()> {
    let storage = Storage::new(db_path);
    let sessions = storage.scan_all()?;
    let mut missing = 0usize;
    for session in &sessions {
        let metadata = serde_json::from_str::<serde_json::Value>(&session.metadata_json)
            .unwrap_or_else(|_| serde_json::json!({}));
        let provenance_path = metadata
            .get("source_path")
            .and_then(|value| value.as_str())
            .filter(|path| *path != "unknown");
        if provenance_path.is_none() {
            missing += 1;
            println!(
                "{}: original source cannot be reconstructed from vault data",
                session.id
            );
        }
    }
    println!("Untraceable sessions: {}", missing);
    Ok(())
}

/// Handles the 'reindex' command
fn handle_reindex(db_path: PathBuf, yes: bool) -> Result<()> {
    // `reindex` now calls `flush_pending`, which writes to the archive data
    // file. We must hold the same CryoLock as `add`/`flush`/`optimise` to
    // avoid corrupting files when another process is appending concurrently.
    let _lock = CryoLock::acquire(&db_path, 5000)?;
    let storage = Storage::new(db_path);

    if !yes {
        println!("This will rebuild the index from existing data.");
        println!("Your data will not be lost, but the old index will be replaced.");
        print!("Continue? (y/N): ");
        io::Write::flush(&mut io::stdout())?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;

        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Cancelled.");
            return Ok(());
        }
    }

    println!("Reindexing...");
    storage.flush_pending()?;

    // Block-count denominator: the existing index may be truncated /
    // out-of-sync, so `stats` would lie. A header-only scan of the data
    // file gives the real number cheaply (no decompression).
    // No data yet — fall through to reindex, which will short-circuit.
    let total_blocks = storage.count_archive_blocks().unwrap_or_default();
    let pb = make_progress_bar(total_blocks);

    let result = storage.reindex_with_progress(|| {
        pb.inc(1);
    });
    let count = match result {
        Ok(c) => c,
        Err(e)
            if e.downcast_ref::<cryo_vault::storage::StorageError>()
                .is_some_and(|err| {
                    matches!(err, cryo_vault::storage::StorageError::DataFileNotFound)
                }) =>
        {
            0
        }
        Err(e) => {
            pb.abandon();
            return Err(e);
        }
    };
    pb.finish_and_clear();
    println!("✓ Reindexed {} sessions", count);
    Ok(())
}

fn make_progress_bar(len: u64) -> ProgressBar {
    let pb = ProgressBar::new(len.max(1));
    pb.set_style(
        ProgressStyle::default_bar()
            .template(
                "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta})",
            )
            .unwrap()
            .progress_chars("#>-"),
    );
    pb
}

/// Handles the 'optimise' command
fn handle_optimise(db_path: PathBuf, chunk_kb: usize, yes: bool) -> Result<()> {
    let _lock = CryoLock::acquire(&db_path, 5000)?;

    if chunk_kb == 0 {
        return Err(anyhow::anyhow!("chunk_kb must be greater than 0"));
    }

    if !yes {
        println!("This will rewrite your data and index files.");
        println!("A new block size of ~{} KB will be used.", chunk_kb);
        print!("Continue? (y/N): ");
        io::Write::flush(&mut io::stdout())?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;

        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Cancelled.");
            return Ok(());
        }
    }

    let storage = Storage::new(db_path.clone());
    let target_bytes = chunk_kb * 1024;

    // Drain pending WAL into archive before optimising, otherwise recently
    // ingested sessions are silently excluded from the rewrite AND the
    // progress bar (which uses stats including pending) never reaches 100%.
    storage.flush_pending()?;

    let stats = storage.get_stats()?;
    let total_sessions = stats.session_count;

    let pb = make_progress_bar(total_sessions);

    let (blocks, sessions) = storage.optimise_with_progress(target_bytes, |inc| {
        pb.inc(inc as u64);
    })?;

    pb.finish_with_message("Optimise complete");

    println!(
        "Optimised {} sessions into {} blocks (~{} KB target).",
        sessions, blocks, chunk_kb
    );
    Ok(())
}

/// Handles the 'flush' command
fn handle_flush(db_path: PathBuf) -> Result<()> {
    let _lock = CryoLock::acquire(&db_path, 5000)?;
    let storage = Storage::new(db_path);
    let count = storage.flush_pending()?;
    println!("Flushed {} sessions to database.", count);
    Ok(())
}

fn handle_capture(db_path: PathBuf, command: CaptureCommands) -> Result<()> {
    match command {
        CaptureCommands::Run {
            platform,
            time,
            dry_run,
            verbose,
            json: as_json,
            settle,
        } => {
            let platform: Platform = platform.parse()?;
            let options = CaptureOptions {
                platform,
                time,
                dry_run,
                settle,
                ..Default::default()
            };
            let _lock = if dry_run {
                None
            } else {
                Some(CryoLock::acquire(&db_path, 5000)?)
            };
            let report = if verbose {
                let mut observer = |event: capture::CaptureEvent| {
                    match event {
                        capture::CaptureEvent::DiscoveryStarted { pass, candidates } => {
                            eprintln!("Capture pass {pass}: scanning {candidates} candidate(s)");
                        }
                        capture::CaptureEvent::Candidate {
                            platform,
                            path,
                            outcome,
                            reason,
                            ..
                        } => {
                            eprintln!(
                                "  [{platform}] {path} — {outcome}{}",
                                reason
                                    .as_deref()
                                    .map(|reason| format!(" ({reason})"))
                                    .unwrap_or_default()
                            );
                        }
                        capture::CaptureEvent::PassCompleted { pass } => {
                            eprintln!("Capture pass {pass} complete");
                        }
                    }
                    let _ = io::stderr().flush();
                };
                capture::run_with_observer(&db_path, &options, &mut observer)?
            } else {
                capture::run(&db_path, &options)?
            };
            if as_json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                let platform_counts = report
                    .discovered_by_platform
                    .iter()
                    .map(|(platform, count)| format!("{platform} {count}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                println!(
                    "Capture: discovered {}{}, stable {}, imported {}, unchanged {}, empty {}, unsupported {}, unstable {}, failed {}{}",
                    report.discovered_files,
                    if platform_counts.is_empty() {
                        String::new()
                    } else {
                        format!(" ({platform_counts})")
                    },
                    report.stable_files,
                    report.imported_sessions,
                    report.skipped_unchanged,
                    report.skipped_empty,
                    report.skipped_unsupported,
                    report.skipped_unstable,
                    report.failed_files,
                    if dry_run { " (dry-run)" } else { "" }
                );
                if report.skipped_unstable > 0 {
                    println!(
                        "{} file(s) await a second unchanged observation; the next unchanged run will import them.",
                        report.skipped_unstable
                    );
                    println!("Re-run: {}", capture_rerun_command(platform));
                    println!("Use --verbose to see candidate paths and reasons.");
                }
                println!(
                    "Capture wrote {} logical session(s). Inspect with: cryo last --source {}",
                    report.imported_sessions,
                    if platform == Platform::All {
                        "<platform>"
                    } else {
                        platform.slug()
                    }
                );
            }
        }
        CaptureCommands::Install {
            platform,
            time,
            dry_run,
        } => {
            let platform: Platform = platform.parse()?;
            let _lock = if dry_run {
                None
            } else {
                Some(CryoLock::acquire(&db_path, 5000)?)
            };
            let mut paths = capture::install_scheduler(&db_path, platform, &time, dry_run)?;
            paths.extend(capture::install_hooks(platform, dry_run)?);
            if dry_run {
                println!("Would install nightly capture at {}", time);
            } else {
                println!("Nightly capture installed for {}", time);
            }
            for path in paths {
                println!("  {}", path.display());
            }
        }
        CaptureCommands::Status { json: as_json } => {
            let state = capture::load_state(&db_path)?;
            let installed = capture::scheduler_installed()?;
            let hooks = capture::hook_statuses()?;
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "installed": installed,
                        "last_run_at": state.last_run_at,
                        "tracked_files": state.files.len(),
                        "pending_hook_hints": state.pending_hooks.len(),
                        "schedule": state.schedule.as_deref().unwrap_or("23:00"),
                        "hooks": hooks
                    }))?
                );
            } else {
                println!(
                    "Nightly capture: {}",
                    if installed {
                        "installed"
                    } else {
                        "not installed"
                    }
                );
                println!(
                    "Schedule:        {} local time",
                    state.schedule.as_deref().unwrap_or("23:00")
                );
                println!("Tracked files:   {}", state.files.len());
                println!("Pending hints:   {}", state.pending_hooks.len());
                println!("Hooks:");
                for hook in hooks {
                    println!(
                        "  {:<13} {} ({}){}",
                        hook.platform,
                        if hook.installed {
                            "installed"
                        } else {
                            "missing"
                        },
                        hook.configuration_path,
                        hook.reason
                            .as_deref()
                            .map(|reason| format!(" — {reason}"))
                            .unwrap_or_default()
                    );
                }
                if let Some(last_run) = state.last_run_at {
                    println!("Last run:        {}", last_run);
                }
            }
        }
        CaptureCommands::Uninstall { dry_run } => {
            let _lock = if dry_run {
                None
            } else {
                Some(CryoLock::acquire(&db_path, 5000)?)
            };
            let mut paths = capture::uninstall_scheduler(dry_run)?;
            paths.extend(capture::uninstall_hooks(Platform::All, dry_run)?);
            if !dry_run {
                capture::clear_schedule(&db_path)?;
            }
            println!(
                "{} nightly capture scheduler{}.",
                if dry_run { "Would remove" } else { "Removed" },
                if paths.is_empty() {
                    String::new()
                } else {
                    format!(
                        " ({})",
                        paths
                            .iter()
                            .map(|p| p.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                }
            );
        }
        CaptureCommands::Hint {
            platform,
            session_id,
            path,
            stdin,
        } => {
            let platform: Platform = platform.parse()?;
            let (session_id, path) = if stdin {
                let mut payload = String::new();
                io::stdin().read_to_string(&mut payload)?;
                let value = serde_json::from_str::<serde_json::Value>(&payload).unwrap_or_default();
                let session_id = session_id.or_else(|| {
                    value
                        .get("session_id")
                        .or_else(|| value.get("sessionId"))
                        .or_else(|| value.get("conversation_id"))
                        .or_else(|| value.get("conversationId"))
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string)
                });
                let path = path.or_else(|| {
                    value
                        .get("transcript_path")
                        .or_else(|| value.get("transcriptPath"))
                        .or_else(|| value.get("transcript_pathname"))
                        .or_else(|| value.get("path"))
                        .and_then(serde_json::Value::as_str)
                        .map(PathBuf::from)
                });
                (session_id, path)
            } else {
                (session_id, path)
            };
            capture::record_hook_hint(
                &db_path,
                HookHint {
                    platform,
                    session_id,
                    path: path.map(|p| p.to_string_lossy().to_string()),
                    seen_at: chrono::Utc::now().timestamp().max(0) as u64,
                },
            )?;
            // Hook clients receive exactly one JSON result on stdout. Tracing
            // is configured for stderr, so no transcript content is emitted.
            println!("{{\"queued\":true}}");
        }
    }
    Ok(())
}

/// Main entry point for the Cryo CLI.
/// Handles command parsing and dispatching to appropriate storage operations.
fn main() -> Result<()> {
    // Initialize tracing subscriber (default: warn level)
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    // 1. Resolve DB Path
    let db_path = cli
        .db
        .unwrap_or_else(|| dirs::home_dir().unwrap().join(".cryo"));

    // 2. Dispatch
    match cli.command {
        Commands::Capture { command } => handle_capture(db_path, command)?,
        Commands::Add { file, stream } => handle_add(db_path, file, stream)?,
        Commands::Flush => handle_flush(db_path)?,
        Commands::Search {
            query,
            after,
            before,
            json,
        } => handle_search(db_path, query, after, before, json)?,
        Commands::Stats => handle_stats(db_path)?,
        Commands::First { count, source } => handle_first(db_path, count, source)?,
        Commands::Last { count, source } => handle_last(db_path, count, source)?,
        Commands::Show {
            session_id,
            diagnostics,
        } => handle_show(db_path, session_id, diagnostics)?,
        Commands::Audit { command } => match command {
            AuditCommands::Provenance => handle_audit_provenance(db_path)?,
        },
        Commands::Reindex { yes } => handle_reindex(db_path, yes)?,
        Commands::Optimise { chunk_kb, yes } => handle_optimise(db_path, chunk_kb, yes)?,
    }

    Ok(())
}
