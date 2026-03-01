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
use std::io::{self, Read, Seek, Write};
use std::path::PathBuf;
use tracing::{debug, trace};

use crate::schema::{ChatSessionInput, ChatSessionV1, StoredSession, StreamEvent};
use constants::{DATA_MAGIC, MAX_FILE_SIZE};
pub use types::{DbStats, StorageError};
pub use writer::{SessionWriter, WalWriter};

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
        let mut wal = self.get_wal_writer()?;

        let mut metadata: HashMap<String, serde_json::Value> =
            serde_json::from_str(&session.metadata_json).unwrap_or_default();

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
        let path = self.get_active_file();
        let index_path = self.get_index_file();

        let mut results = Vec::new();
        let mut seen_ids = std::collections::HashSet::new();

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
                    if msg.content.to_lowercase().contains(&query.to_lowercase()) {
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

        if !path.exists() {
            return Ok(results);
        }

        if !index_path.exists() {
            return Err(anyhow::anyhow!(
                "Index file not found. Please run: cryo reindex"
            ));
        }

        let mut file = File::open(&path)?;
        let mut idx_file = File::open(&index_path)?;

        let mut magic_buf = [0u8; 8];
        file.read_exact(&mut magic_buf)?;
        if magic_buf != DATA_MAGIC {
            return Err(anyhow::anyhow!("Invalid file format"));
        }

        loop {
            let mut idx_size_buf = [0u8; 4];
            let n = idx_file.read(&mut idx_size_buf)?;
            if n == 0 {
                break;
            } // Clean EOF
            if n < 4 {
                idx_file.read_exact(&mut idx_size_buf[n..])?;
            }
            let idx_size = u32::from_le_bytes(idx_size_buf) as usize;

            let mut compressed_idx_buf = vec![0u8; idx_size];
            idx_file.read_exact(&mut compressed_idx_buf)?;

            let idx_buf = zstd::decode_all(&compressed_idx_buf[..])?;
            let index: crate::index::BlockIndex = bincode::deserialize(&idx_buf)?;

            let mut size_buf = [0u8; 4];
            file.read_exact(&mut size_buf)?;
            let size = u32::from_le_bytes(size_buf) as usize;

            if let Some(after_ts) = after
                && index.max_time < after_ts
            {
                file.seek(io::SeekFrom::Current(size as i64))?;
                continue;
            }
            if let Some(before_ts) = before
                && index.min_time > before_ts
            {
                file.seek(io::SeekFrom::Current(size as i64))?;
                continue;
            }

            if index.matches(query) {
                let mut compressed_buf = vec![0u8; size];
                file.read_exact(&mut compressed_buf)?;

                let raw_bytes = zstd::decode_all(&compressed_buf[..])?;
                let wrapper: StoredSession = bincode::deserialize(&raw_bytes)?;

                match wrapper {
                    StoredSession::V1(s) => {
                        let mut found = false;
                        for msg in &s.messages {
                            if msg.content.to_lowercase().contains(&query.to_lowercase()) {
                                found = true;
                                break;
                            }
                        }
                        if found && !seen_ids.contains(&s.id) {
                            seen_ids.insert(s.id.clone());
                            results.push(s);
                        }
                    }
                    StoredSession::Block(sessions) => {
                        for s in sessions {
                            let mut found = false;
                            for msg in &s.messages {
                                if msg.content.to_lowercase().contains(&query.to_lowercase()) {
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
                }
            } else {
                file.seek(std::io::SeekFrom::Current(size as i64))?;
            }
        }

        debug!(results_count = results.len(), "Search completed");
        Ok(results)
    }

    /// Reads all sessions from the file (Scan).
    ///
    /// Checks magic bytes and iterates through all blocks in the file.
    pub fn scan_all(&self) -> Result<Vec<ChatSessionV1>> {
        let path = self.get_active_file();
        let mut sessions = Vec::new();
        let mut seen_ids = std::collections::HashSet::new();

        if let Ok(pending) = self.get_pending_sessions() {
            for s in pending {
                seen_ids.insert(s.id.clone());
                sessions.push(s);
            }
        }

        if !path.exists() {
            return Ok(sessions);
        }

        let mut file = File::open(&path)?;

        let mut magic_buf = [0u8; 8];
        file.read_exact(&mut magic_buf)?;
        if magic_buf != DATA_MAGIC {
            return Err(anyhow::anyhow!("Invalid file format: Wrong Magic Bytes"));
        }

        loop {
            // Read Size (u32)
            let mut size_buf = [0u8; 4];
            let n = file.read(&mut size_buf)?;
            if n == 0 {
                break;
            }
            if n < 4 {
                file.read_exact(&mut size_buf[n..])?;
            }
            let size = u32::from_le_bytes(size_buf) as usize;

            // Read Compressed Body
            let mut compressed_buf = vec![0u8; size];
            file.read_exact(&mut compressed_buf)?;

            // Decompress
            let raw_bytes = zstd::decode_all(&compressed_buf[..])?;

            // Deserialize Bincode -> StoredSession
            let wrapper: StoredSession = bincode::deserialize(&raw_bytes)?;

            match wrapper {
                StoredSession::V1(s) => {
                    if !seen_ids.contains(&s.id) {
                        seen_ids.insert(s.id.clone());
                        sessions.push(s);
                    }
                }
                StoredSession::Block(block_sessions) => {
                    for s in block_sessions {
                        if !seen_ids.contains(&s.id) {
                            seen_ids.insert(s.id.clone());
                            sessions.push(s);
                        }
                    }
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

    /// Internal helper that checks only the persistent archive files, skipping pending.bin
    fn get_archived_session_by_id(&self, session_id: &str) -> Result<Option<ChatSessionV1>> {
        let path = self.get_active_file();
        let index_path = self.get_index_file();

        if !path.exists() {
            return Ok(None);
        }

        if !index_path.exists() {
            return Err(anyhow::anyhow!(
                "Index file not found. Please run: cryo reindex"
            ));
        }

        // Read all index entries and build ID map
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

            // Decompress index
            let idx_buf = zstd::decode_all(&compressed_idx_buf[..])?;
            let index: crate::index::BlockIndex = bincode::deserialize(&idx_buf)?;

            if index.session_id.split(',').any(|id| id == session_id) {
                // Found it! Now read the specific block
                let mut data_file = File::open(&path)?;

                // Seek to the block
                data_file.seek(io::SeekFrom::Start(index.data_offset))?;

                // Read size header
                let mut size_buf = [0u8; 4];
                data_file.read_exact(&mut size_buf)?;
                let size = u32::from_le_bytes(size_buf) as usize;

                // Read compressed block
                let mut compressed_buf = vec![0u8; size];
                data_file.read_exact(&mut compressed_buf)?;

                // Decompress and deserialize
                let raw_bytes = zstd::decode_all(&compressed_buf[..])?;
                let wrapper: StoredSession = bincode::deserialize(&raw_bytes)?;

                return match wrapper {
                    StoredSession::V1(s) => {
                        if s.id == session_id {
                            Ok(Some(s))
                        } else {
                            Ok(None)
                        }
                    }
                    StoredSession::Block(sessions) => {
                        Ok(sessions.into_iter().find(|s| s.id == session_id))
                    }
                };
            }
        }

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
        debug!("Starting reindex operation");
        let path = self.get_active_file();
        let index_path = self.get_index_file();
        let temp_index_path = index_path.with_extension("cryo.tmp");

        if !path.exists() {
            return Err(StorageError::DataFileNotFound.into());
        }

        let mut file = File::open(&path)?;

        let mut idx_file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&temp_index_path)?;

        // Check Magic
        let mut magic_buf = [0u8; 8];
        file.read_exact(&mut magic_buf)?;
        if magic_buf != DATA_MAGIC {
            return Err(anyhow::anyhow!("Invalid file format"));
        }

        let mut count = 0;

        loop {
            // Track offset BEFORE reading size (current position)
            let data_offset = file.stream_position()?;

            // Read Size
            let mut size_buf = [0u8; 4];
            let n = file.read(&mut size_buf)?;
            if n == 0 {
                break;
            }
            if n < 4 {
                file.read_exact(&mut size_buf[n..])?;
            }
            let compressed_size = u32::from_le_bytes(size_buf);

            // Read Compressed Block
            let mut compressed_buf = vec![0u8; compressed_size as usize];
            file.read_exact(&mut compressed_buf)?;

            // Decompress and extract metadata
            let raw_bytes = zstd::decode_all(&compressed_buf[..])?;
            let wrapper: StoredSession = bincode::deserialize(&raw_bytes)?;

            match wrapper {
                StoredSession::V1(session) => {
                    // Extract content
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

                    // Stream to temp file
                    let index_bytes = bincode::serialize(&index_entry)?;
                    let compressed_index = zstd::encode_all(&index_bytes[..], 19)?;
                    let idx_size = compressed_index.len() as u32;
                    idx_file.write_all(&idx_size.to_le_bytes())?;
                    idx_file.write_all(&compressed_index)?;

                    count += 1;
                }
                StoredSession::Block(sessions) => {
                    let mut full_text = String::new();
                    let mut min_time = u64::MAX;
                    let mut max_time = 0;
                    let mut message_count = 0;
                    let mut session_ids = Vec::new();

                    for session in &sessions {
                        session_ids.push(session.id.clone());
                        full_text.push_str(&session.extract_full_text());

                        let created = session.created_at.unwrap_or(0);
                        if created < min_time {
                            min_time = created;
                        }
                        if created > max_time {
                            max_time = created;
                        }
                        message_count += session.messages.len() as u32;
                    }
                    if min_time == u64::MAX {
                        min_time = 0;
                    }

                    let index_entry = crate::index::BlockIndex::new(
                        session_ids.join(","),
                        crate::index::BlockIndexParams {
                            content: &full_text,
                            min_time,
                            max_time,
                            data_offset,
                            compressed_size,
                            uncompressed_size: raw_bytes.len() as u32,
                            message_count,
                        },
                    );

                    let index_bytes = bincode::serialize(&index_entry)?;
                    let compressed_index = zstd::encode_all(&index_bytes[..], 19)?;
                    let idx_size = compressed_index.len() as u32;
                    idx_file.write_all(&idx_size.to_le_bytes())?;
                    idx_file.write_all(&compressed_index)?;

                    count += sessions.len();
                }
            }
        }

        // Finalize
        idx_file.sync_all()?;
        drop(idx_file); // Ensure closed before rename

        fs::rename(temp_index_path, index_path)?;

        debug!(sessions_indexed = count, "Reindex completed");
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

        let path = self.get_active_file();
        if !path.exists() {
            return Ok(stats);
        }
        stats.total_size_bytes += fs::metadata(&path)?.len();

        let index_path = self.get_index_file();
        if index_path.exists() {
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

                stats.session_count += 1;
                stats.message_count += index.message_count as u64;
                stats.data_compressed_bytes += index.compressed_size as u64;
                stats.data_uncompressed_bytes += index.uncompressed_size as u64;

                if index.min_time > 0 && (stats.min_time == 0 || index.min_time < stats.min_time) {
                    stats.min_time = index.min_time;
                }
                if index.max_time != u64::MAX && index.max_time > stats.max_time {
                    stats.max_time = index.max_time;
                }
            }
            return Ok(stats);
        }

        let mut file = File::open(&path)?;

        // Check Magic
        let mut magic_buf = [0u8; 8];
        file.read_exact(&mut magic_buf)?;
        if magic_buf != DATA_MAGIC {
            return Err(anyhow::anyhow!("Invalid file format"));
        }

        loop {
            // Read Size (u32)
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

            // Read Compressed Body
            let mut compressed_buf = vec![0u8; compressed_size];
            file.read_exact(&mut compressed_buf)?;

            // Decompress
            let raw_bytes = zstd::decode_all(&compressed_buf[..])?;
            stats.data_uncompressed_bytes += raw_bytes.len() as u64;

            // Deserialize
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
            }
        }

        Ok(stats)
    }
}
