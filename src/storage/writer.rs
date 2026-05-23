use crate::schema::{ChatSessionV1, StoredSession, StreamEvent};
use crate::storage::constants::DATA_MAGIC;
use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use tracing::{debug, trace};

pub struct SessionWriter {
    data_dir: PathBuf,
    current_segment: u32,
    data_writer: BufWriter<File>,
    index_writer: BufWriter<File>,
    current_size: u64,
    max_size: u64,
}

impl SessionWriter {
    pub fn new(
        data_dir: PathBuf,
        segment: u32,
        data_file: File,
        index_file: File,
        max_size: u64,
    ) -> Result<Self> {
        let current_size = data_file
            .metadata()
            .context("Failed to get file metadata")?
            .len();
        Ok(Self {
            data_dir,
            current_segment: segment,
            data_writer: BufWriter::new(data_file),
            index_writer: BufWriter::new(index_file),
            current_size,
            max_size,
        })
    }

    /// Appends a session to the storage.
    ///
    /// This method handles:
    /// - Data rotation if the file exceeds max size.
    /// - Serialization and compression (zstd level 19).
    /// - Writing to the data file.
    /// - Creating and writing the index entry (with Bloom filter data).
    ///
    /// The offset is tracked manually since we are using buffered writers.
    pub fn append(&mut self, wrapper: StoredSession) -> Result<()> {
        // Data Rotation Check
        if self.current_size >= self.max_size {
            debug!(
                current_size = self.current_size,
                max_size = self.max_size,
                "Rotating to new segment"
            );
            self.rotate()?;
        }

        let raw_bytes = bincode::serialize(&wrapper).context("Failed to serialize session")?;
        // Level 19 for maximum archival compression
        let compressed_bytes =
            zstd::encode_all(&raw_bytes[..], 19).context("Failed to compress")?;
        let compressed_size = compressed_bytes.len() as u32;

        let data_offset = self.current_size;

        self.data_writer.write_all(&compressed_size.to_le_bytes())?;
        self.data_writer.write_all(&compressed_bytes)?;

        let written_bytes = 4 + compressed_bytes.len() as u64;
        self.current_size += written_bytes;

        let build_block_index = |sessions: &[ChatSessionV1], offset: u64, comp_size: u32, uncomp_size: u32| {
            let mut full_text = String::new();
            let mut min_time = u64::MAX;
            let mut max_time = 0;
            let mut message_count = 0;
            let mut session_ids = Vec::new();

            for session in sessions {
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

            crate::index::BlockIndex::new(
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
            )
        };

        let index_entry = match &wrapper {
            StoredSession::V1(session) => {
                let full_text = session.extract_full_text();

                let min_time = session.created_at.unwrap_or(0);
                let max_time = session.created_at.unwrap_or(u64::MAX);

                crate::index::BlockIndex::new(
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
                )
            }
            StoredSession::Block(sessions) => {
                build_block_index(sessions, data_offset, compressed_size, raw_bytes.len() as u32)
            }
            StoredSession::V2(block) => {
                build_block_index(&block.sessions, data_offset, compressed_size, raw_bytes.len() as u32)
            }
        };

        let index_bytes = bincode::serialize(&index_entry)?;
        let compressed_index = zstd::encode_all(&index_bytes[..], 19)?;

        let idx_size = compressed_index.len() as u32;
        self.index_writer.write_all(&idx_size.to_le_bytes())?;
        self.index_writer.write_all(&compressed_index)?;
        let count = match &wrapper {
            StoredSession::V1(_) => 1,
            StoredSession::Block(sessions) => sessions.len(),
            StoredSession::V2(block) => block.sessions.len(),
        };
        trace!(
            compressed_size,
            session_count = count,
            "Session(s) written to storage"
        );
        Ok(())
    }

    /// Rotates the current segment to the next one.
    ///
    /// Flushes current buffers, increments the segment number, and opens new data/index files.
    fn rotate(&mut self) -> Result<()> {
        debug!(
            old_segment = self.current_segment,
            new_segment = self.current_segment + 1,
            "Rotating segment"
        );
        self.flush()?; // Ensure everything is written before closing/switching

        self.current_segment += 1;
        let data_path = self
            .data_dir
            .join(format!("data_{:03}.cryo", self.current_segment));
        let index_path = self
            .data_dir
            .join(format!("index_{:03}.cryo", self.current_segment));

        let mut data_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&data_path)?;
        let index_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&index_path)?;

        if data_file.metadata()?.len() == 0 {
            data_file.write_all(DATA_MAGIC)?;
        }

        self.current_size = data_file.metadata()?.len();
        self.data_writer = BufWriter::new(data_file);
        self.index_writer = BufWriter::new(index_file);

        Ok(())
    }

    /// Flush buffers and sync files to disk
    pub fn flush(&mut self) -> Result<()> {
        self.data_writer.flush()?;
        self.index_writer.flush()?;
        self.data_writer.get_mut().sync_all()?;
        self.index_writer.get_mut().sync_all()?;
        Ok(())
    }
}

pub struct WalWriter {
    writer: BufWriter<File>,
}

impl WalWriter {
    pub fn new(file: File) -> Self {
        Self {
            writer: BufWriter::new(file),
        }
    }

    pub fn append(&mut self, event: StreamEvent) -> Result<()> {
        trace!("Appending event to WAL");
        let bytes = serde_json::to_vec(&event)?;
        let size = bytes.len() as u32;
        self.writer.write_all(&size.to_le_bytes())?;
        self.writer.write_all(&bytes)?;
        Ok(())
    }

    pub fn flush(&mut self) -> Result<()> {
        self.writer.flush()?;
        self.writer.get_mut().sync_all()?;
        Ok(())
    }
}
