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
