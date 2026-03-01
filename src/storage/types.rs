#[derive(thiserror::Error, Debug)]
pub enum StorageError {
    #[error("Data file not found")]
    DataFileNotFound,
}

/// Statistics Data Transfer Object
#[derive(Debug, Default, serde::Serialize)]
pub struct DbStats {
    pub file_name: String,
    pub session_count: u64,
    pub message_count: u64,
    /// File size on disk
    pub total_size_bytes: u64,
    /// Sum of compressed blocks
    pub data_compressed_bytes: u64,
    /// Sum of raw data
    pub data_uncompressed_bytes: u64,
    pub min_time: u64,
    pub max_time: u64,
}

impl DbStats {
    pub fn accumulate_session(&mut self, session: &crate::schema::ChatSessionV1) {
        self.session_count += 1;
        self.message_count += session.messages.len() as u64;
        if let Some(ts) = session.created_at {
            if self.min_time == 0 || ts < self.min_time {
                self.min_time = ts;
            }
            if ts > self.max_time {
                self.max_time = ts;
            }
        }
    }
}
