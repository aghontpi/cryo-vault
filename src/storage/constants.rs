/// Magic bytes for file identification
pub const DATA_MAGIC: &[u8] = b"CRYODAT1";
/// Maximum file size before rotation (1GB)
pub const MAX_FILE_SIZE: u64 = 1024 * 1024 * 1024;
