pub mod constants;
pub mod types;
pub mod writer;

#[cfg(test)]
pub mod tests;

#[cfg(test)]
pub mod property_tests;

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read, Seek, Write};
use std::path::PathBuf;
use tracing::{debug, trace};

use crate::schema::{ChatSessionInput, ChatSessionV1, StoredSession, StreamEvent};
use constants::{DATA_MAGIC, MAX_FILE_SIZE};
pub use types::{DbStats, StorageError};
pub use writer::{SessionWriter, WalWriter};

/// Computes the (min_time, max_time) range for a block of sessions.
///
/// Rule: if any session in the block has no `created_at`, we cannot safely
/// exclude the block from time-filtered queries, so we widen the range to
/// `[0, u64::MAX]` — matching the V1 single-session behaviour where
/// `unwrap_or(0)` / `unwrap_or(u64::MAX)` was used.
///
/// This must be kept in sync between `write_sessions_block`, the writer's
/// `append` path, and `reindex`. Otherwise sessions without a timestamp
/// silently become invisible to `cryo search --after T`.
pub(crate) fn compute_block_time_range(sessions: &[ChatSessionV1]) -> (u64, u64) {
    let mut min_time = u64::MAX;
    let mut max_time = 0u64;
    let mut has_missing = false;

    for session in sessions {
        match session.created_at {
            Some(ts) => {
                if ts < min_time {
                    min_time = ts;
                }
                if ts > max_time {
                    max_time = ts;
                }
            }
            None => has_missing = true,
        }
    }

    if has_missing {
        // At least one session has no timestamp — widen the range so block-level
        // time filtering can never exclude it. Per-session filtering inside
        // `search` decides what to actually return.
        (0, u64::MAX)
    } else if min_time == u64::MAX {
        // Empty list (defensive — callers guard against this).
        (0, 0)
    } else {
        (min_time, max_time)
    }
}

/// Number of session IDs encoded in a BlockIndex.session_id field.
/// Block indexes join IDs with ",", a single-session V1 entry has no separator.
pub(crate) fn count_sessions_in_index_id(joined: &str) -> u64 {
    if joined.is_empty() {
        0
    } else {
        joined.split(',').count() as u64
    }
}

fn write_sessions_block(
    data_writer: &mut BufWriter<File>,
    index_writer: &mut BufWriter<File>,
    data_offset: &mut u64,
    sessions: &[ChatSessionV1],
) -> Result<()> {
    if sessions.is_empty() {
        return Ok(());
    }

    let wrapper = if sessions.len() == 1 {
        StoredSession::V1(sessions[0].clone())
    } else {
        StoredSession::Block(sessions.to_vec())
    };

    let raw_bytes = bincode::serialize(&wrapper).context("Failed to serialize block")?;
    let compressed_bytes =
        zstd::encode_all(&raw_bytes[..], 19).context("Failed to compress block")?;
    let compressed_size = compressed_bytes.len() as u32;

    // Build the BlockIndex
    let mut full_text = String::new();
    let mut message_count = 0;
    let mut session_ids = Vec::new();

    for session in sessions {
        session_ids.push(session.id.clone());
        full_text.push_str(&session.extract_full_text());
        message_count += session.messages.len() as u32;
    }

    let (min_time, max_time) = compute_block_time_range(sessions);

    let index_entry = crate::index::BlockIndex::new(
        session_ids.join(","),
        crate::index::BlockIndexParams {
            content: &full_text,
            min_time,
            max_time,
            data_offset: *data_offset,
            compressed_size,
            uncompressed_size: raw_bytes.len() as u32,
            message_count,
        },
    );

    data_writer.write_all(&compressed_size.to_le_bytes())?;
    data_writer.write_all(&compressed_bytes)?;
    *data_offset += 4 + compressed_bytes.len() as u64;

    let index_bytes = bincode::serialize(&index_entry)?;
    let compressed_index = zstd::encode_all(&index_bytes[..], 19)?;
    let idx_size = compressed_index.len() as u32;
    index_writer.write_all(&idx_size.to_le_bytes())?;
    index_writer.write_all(&compressed_index)?;

    Ok(())
}

pub struct Storage {
    data_dir: PathBuf,
}

impl Storage {
    pub fn new(data_dir: PathBuf) -> Self {
        Storage { data_dir }
    }

    /// Find the active (latest) data file number
    fn get_active_segment_num(&self) -> Result<u32> {
        let mut max_num = 1;

        if let Ok(entries) = fs::read_dir(&self.data_dir) {
            for entry in entries.flatten() {
                if let Some(name) = entry.file_name().to_str()
                    && name.starts_with("data_")
                    && name.ends_with(".cryo")
                    && let Some(num_str) = name
                        .strip_prefix("data_")
                        .and_then(|s| s.strip_suffix(".cryo"))
                    && let Ok(num) = num_str.parse::<u32>()
                {
                    max_num = max_num.max(num);
                }
            }
        }

        Ok(max_num)
    }

    /// Get active data/index files, creating new segment if needed
    fn get_active_files(&self, max_size: u64) -> Result<(PathBuf, PathBuf, u32)> {
        let mut seg_num = self.get_active_segment_num()?;
        let mut data_path = self.data_dir.join(format!("data_{:03}.cryo", seg_num));

        // Check if current file exists and is too large
        if data_path.exists()
            && let Ok(metadata) = fs::metadata(&data_path)
            && metadata.len() >= max_size
        {
            seg_num += 1;
            data_path = self.data_dir.join(format!("data_{:03}.cryo", seg_num));
        }

        let index_path = self.data_dir.join(format!("index_{:03}.cryo", seg_num));
        Ok((data_path, index_path, seg_num))
    }

    fn get_active_file(&self) -> PathBuf {
        self.data_dir.join(format!(
            "data_{:03}.cryo",
            self.get_active_segment_num().unwrap_or(1)
        ))
    }

    fn get_index_file(&self) -> PathBuf {
        self.data_dir.join(format!(
            "index_{:03}.cryo",
            self.get_active_segment_num().unwrap_or(1)
        ))
    }

    fn get_pending_file(&self) -> PathBuf {
        self.data_dir.join("pending.bin")
    }

    /// All `(data_path, index_path, seg_num)` triples on disk, sorted by
    /// segment number ascending.
    ///
    /// Before this helper, every read path (`scan_all`, `search`,
    /// `get_session_by_id`, `get_stats`, `reindex`) only ever inspected the
    /// single highest-numbered segment, so once rotation triggered (>1 GB
    /// per file) all older data became silently invisible.
    fn list_segments(&self) -> Vec<(PathBuf, PathBuf, u32)> {
        let mut segs: Vec<(PathBuf, PathBuf, u32)> = Vec::new();
        if let Ok(entries) = fs::read_dir(&self.data_dir) {
            for entry in entries.flatten() {
                if let Some(name) = entry.file_name().to_str()
                    && name.starts_with("data_")
                    && name.ends_with(".cryo")
                    && let Some(num_str) = name
                        .strip_prefix("data_")
                        .and_then(|s| s.strip_suffix(".cryo"))
                    && let Ok(num) = num_str.parse::<u32>()
                {
                    let data_path = self.data_dir.join(format!("data_{:03}.cryo", num));
                    let index_path = self.data_dir.join(format!("index_{:03}.cryo", num));
                    segs.push((data_path, index_path, num));
                }
            }
        }
        segs.sort_by_key(|(_, _, n)| *n);
        segs
    }

    pub fn get_writer(&self) -> Result<SessionWriter> {
        let (data_path, index_path, seg_num) = self.get_active_files(MAX_FILE_SIZE)?;
        if let Some(parent) = data_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut data_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&data_path)
            .context("Failed to open data file")?;

        if data_file.metadata()?.len() == 0 {
            data_file.write_all(DATA_MAGIC)?;
        }

        let index_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&index_path)
            .context("Failed to open index file")?;

        SessionWriter::new(
            self.data_dir.clone(),
            seg_num,
            data_file,
            index_file,
            MAX_FILE_SIZE,
        )
    }

    /// Appends a session to the active storage file AND writes the index.
    pub fn append_session(&self, session: ChatSessionV1) -> Result<()> {
        let session_id = session.id.clone();
        trace!(session_id = %session_id, "Appending session to storage");
        let mut writer = self.get_writer()?;
        writer.append(StoredSession::V1(session))?;
        writer.flush()?;
        debug!(session_id = %session_id, "Session appended successfully");
        Ok(())
    }

    pub fn get_wal_writer(&self) -> Result<WalWriter> {
        let path = self.get_pending_file();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .context("Failed to open pending file")?;

        Ok(WalWriter::new(file))
    }

    /// Appends a large batch of sessions directly to the main storage file,
    /// bypassing the WAL buffer entirely. This is highly optimized for bulk imports.
    pub fn append_bulk(&self, sessions: Vec<ChatSessionV1>) -> Result<usize> {
        if sessions.is_empty() {
            return Ok(0);
        }

        let mut writer = self.get_writer()?;
        let total_count = sessions.len();

        // We chunk the array to prevent memory spikes in ZSTD compression
        // 100 sessions per block is a reasonable tradeoff for search vs compression
        for chunk in sessions.chunks(100) {
            let block = StoredSession::Block(chunk.to_vec());
            writer.append(block)?;
        }

        writer.flush()?;
        debug!(
            count = total_count,
            "Successfully bulk-appended sessions to main storage"
        );

        Ok(total_count)
    }

    /// Appends a session directly to the WAL buffer and triggers a flush if > 256KB
    pub fn append_pending(&self, session: ChatSessionV1) -> Result<()> {
        // Scope the WAL writer so it is dropped (and its OS handle closed)
        // before we potentially re-open pending.bin with truncate=true in
        // `flush_pending`. Holding an append handle while another open call
        // truncates the same file is fragile on Windows.
        {
            let mut wal = self.get_wal_writer()?;

            let mut metadata: HashMap<String, serde_json::Value> =
                serde_json::from_str(&session.metadata_json).unwrap_or_default();

            // Strip any pre-existing keys that overlap with top-level fields so
            // we don't double-serialise them; the top-level field is the source
            // of truth on the round-trip in `get_pending_sessions`.
            metadata.remove("title");
            metadata.remove("source");
            metadata.remove("model");
            metadata.remove("created_at");

            if let Some(t) = &session.title {
                metadata.insert("title".to_string(), serde_json::Value::String(t.clone()));
            }
            if let Some(s) = &session.source {
                metadata.insert("source".to_string(), serde_json::Value::String(s.clone()));
            }
            if let Some(m) = &session.model {
                metadata.insert("model".to_string(), serde_json::Value::String(m.clone()));
            }
            if let Some(c) = session.created_at {
                metadata.insert(
                    "created_at".to_string(),
                    serde_json::Value::Number(c.into()),
                );
            }

            wal.append(StreamEvent::SessionStart {
                session_id: session.id.clone(),
                metadata,
            })?;

            for msg in session.messages {
                let msg_input = crate::schema::MessageInput {
                    role: msg.role,
                    content: msg.content,
                    tool_calls: msg.tool_calls,
                    tool_outputs: msg.tool_outputs,
                    id: msg.id,
                    parent_id: msg.parent_id,
                    metadata: serde_json::from_str(&msg.metadata_json).unwrap_or_default(),
                };
                wal.append(StreamEvent::AppendMessage {
                    session_id: session.id.clone(),
                    message: msg_input,
                })?;
            }

            wal.append(StreamEvent::Finalize {
                session_id: session.id,
            })?;

            wal.flush()?;
        } // wal dropped here, OS handle closed

        // Trigger auto-flush if > 256KB
        let len = fs::metadata(self.get_pending_file())?.len();
        if len > 256 * 1024 {
            let count = self.flush_pending()?;
            debug!(count, "Auto-flushed buffer to single block.");
        }

        Ok(())
    }

    /// Read pending.bin, aggregate complete sessions, write to .cryo, compact pending.bin
    ///
    /// 1. Reads all events from pending.bin (JSON deserialization).
    /// 2. Reconstructs sessions from events.
    /// 3. Archives finalized sessions ensuring idempotency.
    /// 4. Compacts pending.bin by rewriting only unfinished events.
    pub fn flush_pending(&self) -> Result<usize> {
        trace!("Flushing pending WAL events");
        let pending_path = self.get_pending_file();
        if !pending_path.exists() {
            trace!("No pending file found");
            return Ok(0);
        }

        let mut file = File::open(&pending_path)?;
        let mut events = Vec::new();
        loop {
            let mut size_buf = [0u8; 4];
            let n = file.read(&mut size_buf)?;
            if n == 0 {
                break;
            } // Clean EOF
            if n < 4 {
                file.read_exact(&mut size_buf[n..])?;
            }
            let size = u32::from_le_bytes(size_buf) as usize;
            let mut buf = vec![0u8; size];
            file.read_exact(&mut buf)?;

            events.push(serde_json::from_slice::<StreamEvent>(&buf)?);
        }

        let mut session_map: HashMap<String, ChatSessionInput> = HashMap::new();
        let mut finalized_ids = Vec::new();

        for event in &events {
            match event {
                StreamEvent::SessionStart {
                    session_id,
                    metadata,
                } => {
                    session_map
                        .entry(session_id.clone())
                        .or_insert_with(|| ChatSessionInput {
                            id: Some(session_id.clone()),
                            title: None,
                            source: None,
                            model: None,
                            created_at: None,
                            metadata: metadata.clone(),
                            messages: Vec::new(),
                        });
                }
                StreamEvent::AppendMessage {
                    session_id,
                    message,
                } => {
                    if let Some(session) = session_map.get_mut(session_id) {
                        session.messages.push(message.clone());
                    }
                }
                StreamEvent::Finalize { session_id } => {
                    if session_map.contains_key(session_id) {
                        finalized_ids.push(session_id.clone());
                    }
                }
            }
        }

        let mut archived_count = 0;
        let mut sessions_to_block = Vec::new();

        for id in &finalized_ids {
            // Only process if it was built from parts (exists in map)
            if let Some(input) = session_map.remove(id) {
                // IDEMPOTENCY CHECK: Skip if already exists in the archive
                if self.get_archived_session_by_id(id)?.is_some() {
                    debug!(session_id = %id, "Session already archived, skipping");
                    continue;
                }

                let session_v1: ChatSessionV1 = input.into();
                sessions_to_block.push(session_v1);
                archived_count += 1;
            }
        }

        if !sessions_to_block.is_empty() {
            let mut writer = self.get_writer()?;
            let wrapper = if sessions_to_block.len() == 1 {
                StoredSession::V1(sessions_to_block.pop().unwrap())
            } else {
                StoredSession::Block(sessions_to_block)
            };
            writer.append(wrapper)?;
            writer.flush()?;
        }

        let mut new_pending_events = Vec::new();
        for event in events {
            let sid = match &event {
                StreamEvent::SessionStart { session_id, .. } => session_id,
                StreamEvent::AppendMessage { session_id, .. } => session_id,
                StreamEvent::Finalize { session_id } => session_id,
            };

            if !finalized_ids.contains(sid) {
                new_pending_events.push(event);
            }
        }

        let mut new_file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&pending_path)?;

        for event in new_pending_events {
            // JSON for WAL
            let bytes = serde_json::to_vec(&event)?;
            let size = bytes.len() as u32;
            new_file.write_all(&size.to_le_bytes())?;
            new_file.write_all(&bytes)?;
        }
        new_file.sync_all()?;

        debug!(archived_count, "Pending WAL flush completed");
        Ok(archived_count)
    }

    /// Reads all completed sessions currently in the pending WAL buffer.
    pub fn get_pending_sessions(&self) -> Result<Vec<ChatSessionV1>> {
        let pending_path = self.get_pending_file();
        if !pending_path.exists() {
            return Ok(Vec::new());
        }

        let mut file = File::open(&pending_path)?;
        let mut events = Vec::new();
        loop {
            let mut size_buf = [0u8; 4];
            let n = file.read(&mut size_buf)?;
            if n == 0 {
                break;
            }
            if n < 4 {
                file.read_exact(&mut size_buf[n..])?;
            }
            let size = u32::from_le_bytes(size_buf) as usize;
            let mut buf = vec![0u8; size];
            file.read_exact(&mut buf)?;

            events.push(serde_json::from_slice::<StreamEvent>(&buf)?);
        }

        let mut session_map: HashMap<String, ChatSessionInput> = HashMap::new();
        let mut finalized_ids = Vec::new();

        for event in &events {
            match event {
                StreamEvent::SessionStart {
                    session_id,
                    metadata,
                } => {
                    let title = metadata
                        .get("title")
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    let source = metadata
                        .get("source")
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    let model = metadata
                        .get("model")
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    let created_at = metadata.get("created_at").and_then(|v| v.as_u64());

                    let mut clean_metadata = metadata.clone();
                    clean_metadata.remove("title");
                    clean_metadata.remove("source");
                    clean_metadata.remove("model");
                    clean_metadata.remove("created_at");

                    session_map
                        .entry(session_id.clone())
                        .or_insert_with(|| ChatSessionInput {
                            id: Some(session_id.clone()),
                            title,
                            source,
                            model,
                            created_at,
                            metadata: clean_metadata,
                            messages: Vec::new(),
                        });
                }
                StreamEvent::AppendMessage {
                    session_id,
                    message,
                } => {
                    if let Some(session) = session_map.get_mut(session_id) {
                        session.messages.push(message.clone());
                    }
                }
                StreamEvent::Finalize { session_id } => {
                    if session_map.contains_key(session_id) {
                        finalized_ids.push(session_id.clone());
                    }
                }
            }
        }

        let mut results = Vec::new();
        for id in &finalized_ids {
            if let Some(input) = session_map.remove(id) {
                results.push(input.into());
            }
        }

        Ok(results)
    }

    /// Searches the archive using the Index for filtering.
    ///
    /// Iterates through blocks and index entries in sync.
    /// Checks time range first (cheap check), then the Bloom filter.
    /// If a match is found, decompresses and verifies the content (False Positive Check).
    pub fn search(
        &self,
        query: &str,
        after: Option<u64>,
        before: Option<u64>,
    ) -> Result<Vec<ChatSessionV1>> {
        trace!(query, ?after, ?before, "Starting search");

        let mut results = Vec::new();
        let mut seen_ids = std::collections::HashSet::new();
        let query_lower = query.to_lowercase();

        // 1. Search Pending Buffer First
        if let Ok(pending_sessions) = self.get_pending_sessions() {
            for s in pending_sessions {
                if let Some(after_ts) = after {
                    if let Some(ct) = s.created_at {
                        if ct < after_ts {
                            continue;
                        }
                    }
                }
                if let Some(before_ts) = before {
                    if let Some(ct) = s.created_at {
                        if ct > before_ts {
                            continue;
                        }
                    }
                }

                let mut found = false;
                for msg in &s.messages {
                    if msg.content.to_lowercase().contains(&query_lower) {
                        found = true;
                        break;
                    }
                }
                if found {
                    seen_ids.insert(s.id.clone());
                    results.push(s);
                }
            }
        }

        // 2. Search every archived segment (oldest → newest). Previously this
        //    only looked at the latest segment, so after a 1 GB rotation older
        //    data became invisible to search.
        let segments = self.list_segments();
        if segments.is_empty() {
            return Ok(results);
        }

        for (path, index_path, _seg) in segments {
            if !path.exists() {
                continue;
            }
            if !index_path.exists() {
                return Err(anyhow::anyhow!(
                    "Index file not found for segment {}. Please run: cryo reindex",
                    path.display()
                ));
            }

            let mut file = File::open(&path)?;
            let mut idx_file = File::open(&index_path)?;

            let mut magic_buf = [0u8; 8];
            file.read_exact(&mut magic_buf)?;
            if magic_buf != DATA_MAGIC {
                return Err(anyhow::anyhow!(
                    "Invalid file format in {}",
                    path.display()
                ));
            }

            // Walk the data file sequentially. When an index entry is
            // available we use it for fast bloom-filter / time-range pruning.
            // When the index runs out before the data does — a known shape of
            // index corruption from older releases where MCP `add_log` and
            // CLI `add` raced without a shared lock — we fall back to
            // decompressing each unindexed block and content-checking it
            // directly. This keeps search correct (no missing matches) at a
            // proportional speed cost on the unindexed tail.
            let mut idx_exhausted = false;
            let mut unindexed_blocks_scanned: u64 = 0;

            loop {
                let index_opt: Option<crate::index::BlockIndex> = if idx_exhausted {
                    None
                } else {
                    let mut idx_size_buf = [0u8; 4];
                    let n = idx_file.read(&mut idx_size_buf)?;
                    if n == 0 {
                        idx_exhausted = true;
                        None
                    } else {
                        if n < 4 {
                            idx_file.read_exact(&mut idx_size_buf[n..])?;
                        }
                        let idx_size = u32::from_le_bytes(idx_size_buf) as usize;
                        let mut compressed_idx_buf = vec![0u8; idx_size];
                        idx_file.read_exact(&mut compressed_idx_buf)?;
                        let idx_buf = zstd::decode_all(&compressed_idx_buf[..])?;
                        Some(bincode::deserialize(&idx_buf)?)
                    }
                };

                let mut size_buf = [0u8; 4];
                let n = file.read(&mut size_buf)?;
                if n == 0 {
                    break;
                }
                if n < 4 {
                    file.read_exact(&mut size_buf[n..])?;
                }
                let size = u32::from_le_bytes(size_buf) as usize;

                if index_opt.is_none() {
                    unindexed_blocks_scanned += 1;
                }

                // Time-range prefilter via index (only when block-level
                // bounds are known — without an index we have to decompress
                // and do the per-session check below).
                if let Some(ref idx) = index_opt {
                    if let Some(after_ts) = after
                        && idx.max_time < after_ts
                    {
                        file.seek(io::SeekFrom::Current(size as i64))?;
                        continue;
                    }
                    if let Some(before_ts) = before
                        && idx.min_time > before_ts
                    {
                        file.seek(io::SeekFrom::Current(size as i64))?;
                        continue;
                    }
                }

                // Bloom-filter prefilter when an index entry is present.
                // No index entry → can't prune, fall through to decompress.
                let must_decompress = match index_opt.as_ref() {
                    Some(idx) => idx.matches(query),
                    None => true,
                };

                if !must_decompress {
                    file.seek(std::io::SeekFrom::Current(size as i64))?;
                    continue;
                }

                let mut compressed_buf = vec![0u8; size];
                file.read_exact(&mut compressed_buf)?;

                let raw_bytes = zstd::decode_all(&compressed_buf[..])?;
                let wrapper: StoredSession = bincode::deserialize(&raw_bytes)?;

                let sessions = match wrapper {
                    StoredSession::V1(s) => vec![s],
                    StoredSession::Block(sessions) => sessions,
                    StoredSession::V2(block) => block.sessions,
                };
                for s in sessions {
                    // Per-session time filter — block-level min/max can let
                    // sessions outside the requested range slip through when a
                    // block straddles the boundary or contains sessions with
                    // no `created_at`.
                    if let Some(after_ts) = after {
                        if let Some(ct) = s.created_at {
                            if ct < after_ts {
                                continue;
                            }
                        }
                    }
                    if let Some(before_ts) = before {
                        if let Some(ct) = s.created_at {
                            if ct > before_ts {
                                continue;
                            }
                        }
                    }

                    let mut found = false;
                    for msg in &s.messages {
                        if msg.content.to_lowercase().contains(&query_lower) {
                            found = true;
                            break;
                        }
                    }
                    if found && !seen_ids.contains(&s.id) {
                        seen_ids.insert(s.id.clone());
                        results.push(s);
                    }
                }
            }

            if unindexed_blocks_scanned > 0 {
                tracing::warn!(
                    segment = %path.display(),
                    unindexed_blocks = unindexed_blocks_scanned,
                    "Index is out of sync with data (missing entries for {} blocks). Search fell back to full scan for those blocks. Run `cryo reindex` to restore fast search.",
                    unindexed_blocks_scanned
                );
            }
        }

        debug!(results_count = results.len(), "Search completed");
        Ok(results)
    }

    /// Reads all sessions from every segment on disk (oldest → newest)
    /// plus the pending buffer.
    ///
    /// Pending sessions are prepended in their WAL order so callers that
    /// take "last N" still see the freshest activity.
    pub fn scan_all(&self) -> Result<Vec<ChatSessionV1>> {
        let mut sessions = Vec::new();
        let mut seen_ids = std::collections::HashSet::new();

        // Archive first (chronological), pending appended last so the freshest
        // entries land at the end of the vector — preserving the long-standing
        // "last N = most recent activity" assumption used by `cryo last` /
        // `get_recent_sessions`.
        for (data_path, _index_path, _seg) in self.list_segments() {
            if !data_path.exists() {
                continue;
            }

            let mut file = File::open(&data_path)?;

            let mut magic_buf = [0u8; 8];
            file.read_exact(&mut magic_buf)?;
            if magic_buf != DATA_MAGIC {
                return Err(anyhow::anyhow!(
                    "Invalid file format: Wrong Magic Bytes in {}",
                    data_path.display()
                ));
            }

            loop {
                let mut size_buf = [0u8; 4];
                let n = file.read(&mut size_buf)?;
                if n == 0 {
                    break;
                }
                if n < 4 {
                    file.read_exact(&mut size_buf[n..])?;
                }
                let size = u32::from_le_bytes(size_buf) as usize;

                let mut compressed_buf = vec![0u8; size];
                file.read_exact(&mut compressed_buf)?;

                let raw_bytes = zstd::decode_all(&compressed_buf[..])?;
                let wrapper: StoredSession = bincode::deserialize(&raw_bytes)?;

                let block_sessions = match wrapper {
                    StoredSession::V1(s) => vec![s],
                    StoredSession::Block(sessions) => sessions,
                    StoredSession::V2(block) => block.sessions,
                };
                for s in block_sessions {
                    if !seen_ids.contains(&s.id) {
                        seen_ids.insert(s.id.clone());
                        sessions.push(s);
                    }
                }
            }
        }

        if let Ok(pending) = self.get_pending_sessions() {
            for s in pending {
                if !seen_ids.contains(&s.id) {
                    seen_ids.insert(s.id.clone());
                    sessions.push(s);
                }
            }
        }

        Ok(sessions)
    }

    /// Get a single session by ID using the index (O(1) lookup)
    pub fn get_session_by_id(&self, session_id: &str) -> Result<Option<ChatSessionV1>> {
        // 1. Check pending first
        if let Ok(pending) = self.get_pending_sessions() {
            if let Some(s) = pending.into_iter().find(|s| s.id == session_id) {
                return Ok(Some(s));
            }
        }

        // 2. Check Archive
        self.get_archived_session_by_id(session_id)
    }

    /// Internal helper that checks only the persistent archive files, skipping pending.bin.
    /// Iterates every segment so sessions written before rotation remain reachable.
    fn get_archived_session_by_id(&self, session_id: &str) -> Result<Option<ChatSessionV1>> {
        for (path, index_path, _seg) in self.list_segments() {
            if !path.exists() {
                continue;
            }
            if !index_path.exists() {
                return Err(anyhow::anyhow!(
                    "Index file not found for segment {}. Please run: cryo reindex",
                    path.display()
                ));
            }

            let mut idx_file = File::open(&index_path)?;

            loop {
                let mut idx_size_buf = [0u8; 4];
                let n = idx_file.read(&mut idx_size_buf)?;
                if n == 0 {
                    break;
                }
                if n < 4 {
                    idx_file.read_exact(&mut idx_size_buf[n..])?;
                }
                let idx_size = u32::from_le_bytes(idx_size_buf) as usize;

                let mut compressed_idx_buf = vec![0u8; idx_size];
                idx_file.read_exact(&mut compressed_idx_buf)?;

                let idx_buf = zstd::decode_all(&compressed_idx_buf[..])?;
                let index: crate::index::BlockIndex = bincode::deserialize(&idx_buf)?;

                if index.session_id.split(',').any(|id| id == session_id) {
                    let mut data_file = File::open(&path)?;

                    data_file.seek(io::SeekFrom::Start(index.data_offset))?;

                    let mut size_buf = [0u8; 4];
                    data_file.read_exact(&mut size_buf)?;
                    let size = u32::from_le_bytes(size_buf) as usize;

                    let mut compressed_buf = vec![0u8; size];
                    data_file.read_exact(&mut compressed_buf)?;

                    let raw_bytes = zstd::decode_all(&compressed_buf[..])?;
                    let wrapper: StoredSession = bincode::deserialize(&raw_bytes)?;

                    return match wrapper {
                        StoredSession::V1(s) => {
                            if s.id == session_id {
                                Ok(Some(s))
                            } else {
                                // Bloom false positive on a single-session block — keep scanning.
                                continue;
                            }
                        }
                        StoredSession::Block(sessions) => {
                            if let Some(s) = sessions.into_iter().find(|s| s.id == session_id) {
                                Ok(Some(s))
                            } else {
                                continue;
                            }
                        }
                        StoredSession::V2(block) => {
                            if let Some(s) = block.sessions.into_iter().find(|s| s.id == session_id) {
                                Ok(Some(s))
                            } else {
                                continue;
                            }
                        }
                    };
                } // end `if index matches`
            } // end inner loop over index entries
        } // end for over segments

        Ok(None)
    }

    /// Rebuild index from existing data file - Streaming & Atomic
    ///
    /// 1. Opens the data file.
    /// 2. Creates a temporary index file.
    /// 3. Reads all blocks, creating index entries on the fly.
    /// 4. Writes index entries to the temp file.
    /// 5. Rename temp file to actual index file.
    pub fn reindex(&self) -> Result<usize> {
        self.reindex_with_progress(|| {})
    }

    /// Reindex with a per-block progress callback.
    ///
    /// `on_block` fires once per data block successfully indexed (paired
    /// with [`count_archive_blocks`] as the bar denominator). Callers that
    /// don't care can pass `|| {}`.
    pub fn reindex_with_progress<F>(&self, mut on_block: F) -> Result<usize>
    where
        F: FnMut(),
    {
        debug!("Starting reindex operation");

        let segments = self.list_segments();
        if segments.is_empty() {
            return Err(StorageError::DataFileNotFound.into());
        }

        let mut total_count = 0usize;

        for (path, index_path, _seg) in segments {
            if !path.exists() {
                continue;
            }
            total_count += self.reindex_segment(&path, &index_path, &mut on_block)?;
        }

        debug!(sessions_indexed = total_count, "Reindex completed");
        Ok(total_count)
    }

    /// Total number of compressed blocks across every archive segment.
    ///
    /// Reads only the 4-byte size headers and seeks past each block payload,
    /// so this is O(blocks) reads with no decompression. Useful as a
    /// progress-bar denominator for `reindex`, where the existing index is
    /// untrustworthy and `stats` would lie.
    pub fn count_archive_blocks(&self) -> Result<u64> {
        let mut total: u64 = 0;
        for (path, _index_path, _seg) in self.list_segments() {
            if !path.exists() {
                continue;
            }
            let mut file = File::open(&path)?;
            let mut magic = [0u8; 8];
            file.read_exact(&mut magic)?;
            if magic != DATA_MAGIC {
                return Err(anyhow::anyhow!(
                    "Invalid file format in {}",
                    path.display()
                ));
            }
            loop {
                let mut size_buf = [0u8; 4];
                let n = file.read(&mut size_buf)?;
                if n == 0 {
                    break;
                }
                if n < 4 {
                    file.read_exact(&mut size_buf[n..])?;
                }
                let size = u32::from_le_bytes(size_buf) as i64;
                file.seek(io::SeekFrom::Current(size))?;
                total += 1;
            }
        }
        Ok(total)
    }

    /// Rebuilds the index for a single segment atomically (temp file → rename).
    fn reindex_segment<F>(
        &self,
        path: &PathBuf,
        index_path: &PathBuf,
        on_block: &mut F,
    ) -> Result<usize>
    where
        F: FnMut(),
    {
        let temp_index_path = index_path.with_extension("cryo.tmp");

        let mut file = File::open(path)?;

        let mut idx_file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&temp_index_path)?;

        let mut magic_buf = [0u8; 8];
        file.read_exact(&mut magic_buf)?;
        if magic_buf != DATA_MAGIC {
            return Err(anyhow::anyhow!(
                "Invalid file format in {}",
                path.display()
            ));
        }

        let mut count = 0;

        loop {
            let data_offset = file.stream_position()?;

            let mut size_buf = [0u8; 4];
            let n = file.read(&mut size_buf)?;
            if n == 0 {
                break;
            }
            if n < 4 {
                file.read_exact(&mut size_buf[n..])?;
            }
            let compressed_size = u32::from_le_bytes(size_buf);

            let mut compressed_buf = vec![0u8; compressed_size as usize];
            file.read_exact(&mut compressed_buf)?;

            let raw_bytes = zstd::decode_all(&compressed_buf[..])?;
            let wrapper: StoredSession = bincode::deserialize(&raw_bytes)?;

            let build_block_index_and_len = |sessions: &[ChatSessionV1], offset: u64, comp_size: u32, uncomp_size: u32| {
                let mut full_text = String::new();
                let mut message_count = 0;
                let mut session_ids = Vec::new();

                for session in sessions {
                    session_ids.push(session.id.clone());
                    full_text.push_str(&session.extract_full_text());
                    message_count += session.messages.len() as u32;
                }

                let (min_time, max_time) = compute_block_time_range(sessions);

                let entry = crate::index::BlockIndex::new(
                    session_ids.join(","),
                    crate::index::BlockIndexParams {
                        content: &full_text,
                        min_time,
                        max_time,
                        data_offset: offset,
                        compressed_size: comp_size,
                        uncompressed_size: uncomp_size,
                        message_count,
                    },
                );
                (entry, sessions.len())
            };

            match wrapper {
                StoredSession::V1(session) => {
                    let full_text = session.extract_full_text();

                    let min_time = session.created_at.unwrap_or(0);
                    let max_time = session.created_at.unwrap_or(u64::MAX);

                    let index_entry = crate::index::BlockIndex::new(
                        session.id.clone(),
                        crate::index::BlockIndexParams {
                            content: &full_text,
                            min_time,
                            max_time,
                            data_offset,
                            compressed_size,
                            uncompressed_size: raw_bytes.len() as u32,
                            message_count: session.messages.len() as u32,
                        },
                    );

                    let index_bytes = bincode::serialize(&index_entry)?;
                    let compressed_index = zstd::encode_all(&index_bytes[..], 19)?;
                    let idx_size = compressed_index.len() as u32;
                    idx_file.write_all(&idx_size.to_le_bytes())?;
                    idx_file.write_all(&compressed_index)?;

                    count += 1;
                    on_block();
                }
                StoredSession::Block(sessions) => {
                    let (index_entry, sessions_len) = build_block_index_and_len(&sessions, data_offset, compressed_size, raw_bytes.len() as u32);
                    let index_bytes = bincode::serialize(&index_entry)?;
                    let compressed_index = zstd::encode_all(&index_bytes[..], 19)?;
                    let idx_size = compressed_index.len() as u32;
                    idx_file.write_all(&idx_size.to_le_bytes())?;
                    idx_file.write_all(&compressed_index)?;

                    count += sessions_len;
                    on_block();
                }
                StoredSession::V2(block) => {
                    let (index_entry, sessions_len) = build_block_index_and_len(&block.sessions, data_offset, compressed_size, raw_bytes.len() as u32);
                    let index_bytes = bincode::serialize(&index_entry)?;
                    let compressed_index = zstd::encode_all(&index_bytes[..], 19)?;
                    let idx_size = compressed_index.len() as u32;
                    idx_file.write_all(&idx_size.to_le_bytes())?;
                    idx_file.write_all(&compressed_index)?;

                    count += sessions_len;
                    on_block();
                }
            }
        }

        idx_file.sync_all()?;
        drop(idx_file);

        fs::rename(&temp_index_path, index_path)?;

        Ok(count)
    }

    /// Comprehensive Database Statistics
    ///
    /// Tries an optimized path using the Index if available.
    /// Fallback to full data scan (slow) if index is missing.
    pub fn get_stats(&self) -> Result<DbStats> {
        let mut stats = DbStats {
            file_name: self
                .get_active_file()
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string(),
            session_count: 0,
            message_count: 0,
            min_time: 0,
            max_time: 0,
            total_size_bytes: 0,
            data_compressed_bytes: 0,
            data_uncompressed_bytes: 0,
        };

        // 1. Count pending stats
        if let Ok(pending) = self.get_pending_sessions() {
            for s in pending {
                stats.accumulate_session(&s);
            }
        }

        // 2. Add pending file size to total disk usage
        let pending_path = self.get_pending_file();
        if pending_path.exists() {
            stats.total_size_bytes += fs::metadata(&pending_path)?.len();
        }

        // Iterate every segment — previously this only counted the latest
        // segment, so once rotation triggered all older data was missing
        // from `cryo stats`.
        let segments = self.list_segments();
        if segments.is_empty() {
            return Ok(stats);
        }

        for (path, index_path, _seg) in &segments {
            if !path.exists() {
                continue;
            }
            stats.total_size_bytes += fs::metadata(path)?.len();

            if index_path.exists() {
                let mut idx_file = File::open(index_path)?;
                loop {
                    let mut idx_size_buf = [0u8; 4];
                    let n = idx_file.read(&mut idx_size_buf)?;
                    if n == 0 {
                        break;
                    }
                    if n < 4 {
                        idx_file.read_exact(&mut idx_size_buf[n..])?;
                    }
                    let idx_size = u32::from_le_bytes(idx_size_buf) as usize;

                    let mut compressed_idx_buf = vec![0u8; idx_size];
                    idx_file.read_exact(&mut compressed_idx_buf)?;

                    let idx_buf = zstd::decode_all(&compressed_idx_buf[..])?;
                    let index: crate::index::BlockIndex = bincode::deserialize(&idx_buf)?;

                    // Each index entry can represent multiple sessions (Block / V2),
                    // joined by ',' in `session_id`. Counting +=1 here would report
                    // block count instead of session count.
                    stats.session_count += count_sessions_in_index_id(&index.session_id);
                    stats.message_count += index.message_count as u64;
                    stats.data_compressed_bytes += index.compressed_size as u64;
                    stats.data_uncompressed_bytes += index.uncompressed_size as u64;

                    if index.min_time > 0
                        && (stats.min_time == 0 || index.min_time < stats.min_time)
                    {
                        stats.min_time = index.min_time;
                    }
                    if index.max_time != u64::MAX && index.max_time > stats.max_time {
                        stats.max_time = index.max_time;
                    }
                }
                continue; // Move on to the next segment.
            }

            // Index missing for this segment — fall back to a slow data scan.
            let mut file = File::open(path)?;

            let mut magic_buf = [0u8; 8];
            file.read_exact(&mut magic_buf)?;
            if magic_buf != DATA_MAGIC {
                return Err(anyhow::anyhow!(
                    "Invalid file format in {}",
                    path.display()
                ));
            }

            loop {
                let mut size_buf = [0u8; 4];
                let n = file.read(&mut size_buf)?;
                if n == 0 {
                    break;
                }
                if n < 4 {
                    file.read_exact(&mut size_buf[n..])?;
                }
                let compressed_size = u32::from_le_bytes(size_buf) as usize;
                stats.data_compressed_bytes += compressed_size as u64;

                let mut compressed_buf = vec![0u8; compressed_size];
                file.read_exact(&mut compressed_buf)?;

                let raw_bytes = zstd::decode_all(&compressed_buf[..])?;
                stats.data_uncompressed_bytes += raw_bytes.len() as u64;

                let wrapper: StoredSession = bincode::deserialize(&raw_bytes)?;
                match wrapper {
                    StoredSession::V1(s) => {
                        stats.accumulate_session(&s);
                    }
                    StoredSession::Block(block_sessions) => {
                        for s in &block_sessions {
                            stats.accumulate_session(s);
                        }
                    }
                    StoredSession::V2(block) => {
                        for s in &block.sessions {
                            stats.accumulate_session(s);
                        }
                    }
                }
            }
        }

        Ok(stats)
    }

    /// Rewrites the active data/index files using chunked blocks.
    pub fn optimise(&self, target_bytes: usize) -> Result<(usize, usize)> {
        self.optimise_with_progress(target_bytes, |_| {})
    }

    /// Optimise data file into ~target_bytes compressed blocks and rebuild index.
    ///
    /// Calls `on_session` for each session processed.
    pub fn optimise_with_progress<F>(&self, target_bytes: usize, mut on_session: F) -> Result<(usize, usize)>
    where
        F: FnMut(usize),
    {
        let path = self.get_active_file();
        let index_path = self.get_index_file();

        if !path.exists() {
            return Ok((0, 0));
        }

        let temp_data_path = path.with_extension("cryo.tmp");
        let temp_index_path = index_path.with_extension("cryo.tmp");

        let mut input = File::open(&path)?;

        // Check Magic
        let mut magic_buf = [0u8; 8];
        input.read_exact(&mut magic_buf)?;
        if magic_buf != DATA_MAGIC {
            return Err(anyhow::anyhow!("Invalid file format"));
        }

        let data_file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&temp_data_path)?;
        let index_file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&temp_index_path)?;

        let mut data_writer = BufWriter::new(data_file);
        let mut index_writer = BufWriter::new(index_file);

        data_writer.write_all(DATA_MAGIC)?;
        let mut data_offset = DATA_MAGIC.len() as u64;

        let mut chunk_sessions: Vec<ChatSessionV1> = Vec::new();
        let mut block_count = 0usize;
        let mut session_count = 0usize;

        loop {
            let mut size_buf = [0u8; 4];
            let n = input.read(&mut size_buf)?;
            if n == 0 {
                break;
            }
            if n < 4 {
                input.read_exact(&mut size_buf[n..])?;
            }
            let compressed_size = u32::from_le_bytes(size_buf) as usize;
            let mut compressed_buf = vec![0u8; compressed_size];
            input.read_exact(&mut compressed_buf)?;

            let raw_bytes = zstd::decode_all(&compressed_buf[..])?;
            let wrapper: StoredSession = bincode::deserialize(&raw_bytes)?;

            let sessions = match wrapper {
                StoredSession::V1(session) => vec![session],
                StoredSession::Block(sessions) => sessions,
                StoredSession::V2(block) => block.sessions,
            };

            for session in sessions {
                on_session(1);
                chunk_sessions.push(session);

                // Cheap gate: skip trial-compression until the *uncompressed*
                // bincode size is plausibly above target. zstd on chat text
                // achieves ~3–5× compression at level 19, so we only start
                // trial-compressing once raw size is > 2× target. This keeps
                // optimise roughly O(N) instead of O(N²) of re-compressions
                // of the growing chunk.
                let raw = bincode::serialize(&if chunk_sessions.len() == 1 {
                    StoredSession::V1(chunk_sessions[0].clone())
                } else {
                    StoredSession::Block(chunk_sessions.clone())
                })?;

                if raw.len() < target_bytes * 2 {
                    continue;
                }

                // Use the SAME level we actually write at (19), otherwise the
                // size check is based on a much larger trial output and the
                // emitted blocks come out 2–3× smaller than `target_bytes`.
                let compressed = zstd::encode_all(&raw[..], 19)?;

                if compressed.len() > target_bytes && chunk_sessions.len() > 1 {
                    let last = chunk_sessions.pop().expect("chunk has at least one session");
                    write_sessions_block(
                        &mut data_writer,
                        &mut index_writer,
                        &mut data_offset,
                        &chunk_sessions,
                    )?;
                    block_count += 1;
                    session_count += chunk_sessions.len();
                    chunk_sessions.clear();
                    chunk_sessions.push(last);
                } else if compressed.len() >= target_bytes {
                    write_sessions_block(
                        &mut data_writer,
                        &mut index_writer,
                        &mut data_offset,
                        &chunk_sessions,
                    )?;
                    block_count += 1;
                    session_count += chunk_sessions.len();
                    chunk_sessions.clear();
                }
            }
        }

        if !chunk_sessions.is_empty() {
            write_sessions_block(
                &mut data_writer,
                &mut index_writer,
                &mut data_offset,
                &chunk_sessions,
            )?;
            block_count += 1;
            session_count += chunk_sessions.len();
            chunk_sessions.clear();
        }

        data_writer.flush()?;
        index_writer.flush()?;
        data_writer.get_mut().sync_all()?;
        index_writer.get_mut().sync_all()?;

        fs::rename(temp_data_path, path)?;
        fs::rename(temp_index_path, index_path)?;

        Ok((block_count, session_count))
    }
}
