use anyhow::Result;
use chrono::NaiveDate;
use clap::{Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};
use std::io::{self, BufRead, Read};
use std::path::PathBuf;

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
    },

    /// Show last N sessions (newest)
    Last {
        /// Number of sessions to show
        #[arg(default_value = "10")]
        count: usize,
    },

    /// Show full session details
    Show {
        /// Session ID
        session_id: String,
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

/// Helper function to print a list of sessions with previews
fn print_session_list(sessions: &[ChatSessionV1]) {
    for session in sessions {
        println!(
            "[{}] {}",
            session.id,
            session.title.as_deref().unwrap_or("Untitled")
        );
        println!("  Messages: {}", session.messages.len());
        if !session.messages.is_empty() {
            let preview = &session.messages[0].content;
            let preview_text = if preview.len() > 60 {
                format!("{}...", &preview[..60])
            } else {
                preview.clone()
            };
            println!("  Preview: {}", preview_text);
        }
        println!();
    }
}

/// Helper function to display first or last N sessions
fn display_sessions(storage: Storage, count: usize, first: bool) -> Result<()> {
    let sessions = storage.scan_all()?;
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
        let session: ChatSessionV1 = input.into();
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
                    sessions_to_import.push(session);
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
                sessions_to_import.push(input.into());
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
fn handle_first(db_path: PathBuf, count: usize) -> Result<()> {
    let storage = Storage::new(db_path);
    display_sessions(storage, count, true)
}

/// Handles the 'last' command
fn handle_last(db_path: PathBuf, count: usize) -> Result<()> {
    let storage = Storage::new(db_path);
    display_sessions(storage, count, false)
}

/// Handles the 'show' command
fn handle_show(db_path: PathBuf, session_id: String) -> Result<()> {
    let storage = Storage::new(db_path);

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

/// Handles the 'reindex' command
fn handle_reindex(db_path: PathBuf, yes: bool) -> Result<()> {
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
    let count = match storage.reindex() {
        Ok(c) => c,
        Err(e)
            if e.downcast_ref::<cryo_vault::storage::StorageError>()
                .is_some_and(|err| {
                    matches!(err, cryo_vault::storage::StorageError::DataFileNotFound)
                }) =>
        {
            0
        }
        Err(e) => return Err(e),
    };
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
    let stats = storage.get_stats()?;
    let total_sessions = stats.session_count as u64;

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
        Commands::Add { file, stream } => handle_add(db_path, file, stream)?,
        Commands::Flush => handle_flush(db_path)?,
        Commands::Search {
            query,
            after,
            before,
            json,
        } => handle_search(db_path, query, after, before, json)?,
        Commands::Stats => handle_stats(db_path)?,
        Commands::First { count } => handle_first(db_path, count)?,
        Commands::Last { count } => handle_last(db_path, count)?,
        Commands::Show { session_id } => handle_show(db_path, session_id)?,
        Commands::Reindex { yes } => handle_reindex(db_path, yes)?,
        Commands::Optimise { chunk_kb, yes } => handle_optimise(db_path, chunk_kb, yes)?,
    }

    Ok(())
}
