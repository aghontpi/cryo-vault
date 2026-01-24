use fastbloom::BloomFilter;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Parameters for creating a `BlockIndex`.
pub struct BlockIndexParams<'a> {
    /// Text content to be tokenized and indexed.
    pub content: &'a str,
    /// Minimum timestamp (Unix epoch seconds).
    pub min_time: u64,
    /// Maximum timestamp (Unix epoch seconds).
    pub max_time: u64,
    /// Byte offset in the data file.
    pub data_offset: u64,
    /// Size of the compressed block in bytes.
    pub compressed_size: u32,
    /// Size of the uncompressed data in bytes.
    pub uncompressed_size: u32,
    /// Number of messages in this block.
    pub message_count: u32,
}

impl<'a> BlockIndexParams<'a> {
    /// Validates invariants: time range, compression ratio, and message count.
    pub fn validate(&self) -> Result<(), String> {
        if self.min_time > self.max_time {
            return Err(format!(
                "Invalid time range: min_time ({}) > max_time ({})",
                self.min_time, self.max_time
            ));
        }

        if self.compressed_size > self.uncompressed_size {
            return Err(format!(
                "Invalid compression: compressed_size ({}) > uncompressed_size ({})",
                self.compressed_size, self.uncompressed_size
            ));
        }

        if self.message_count == 0 {
            return Err("Block must contain at least one message (message_count == 0)".to_string());
        }

        Ok(())
    }
}

/// Represents the index for a single compressed block (session)
#[derive(Serialize, Deserialize, Debug)]
pub struct BlockIndex {
    pub session_id: String,
    /// Byte offset in data file
    pub data_offset: u64,
    /// Size of compressed block
    pub compressed_size: u32,
    /// Size of uncompressed data
    pub uncompressed_size: u32,
    /// Number of messages in the session
    pub message_count: u32,
    pub min_time: u64,
    pub max_time: u64,
    pub bloom: BloomFilter,
}

impl BlockIndex {
    /// Creates a new `BlockIndex`.
    ///
    /// Tokenizes content (alphanumeric, lowercase) into a Bloom filter (5% FP rate).
    ///
    /// # Parameters
    ///
    /// * `session_id` - Unique identifier for this session/block
    /// * `params` - Block parameters
    pub fn new(session_id: String, params: BlockIndexParams) -> Self {
        let mut tokens: HashSet<String> = params
            .content
            .split_whitespace()
            .map(|s| s.trim_matches(|c: char| !c.is_alphanumeric()))
            .filter(|s| !s.is_empty())
            .map(|s| s.to_lowercase())
            .collect();

        tokens.insert(session_id.clone());

        let filter = BloomFilter::with_false_pos(0.05)
            .seed(&0x517CC1B727220A95_u128)
            .items(tokens.iter());

        Self {
            session_id,
            data_offset: params.data_offset,
            compressed_size: params.compressed_size,
            uncompressed_size: params.uncompressed_size,
            message_count: params.message_count,
            min_time: params.min_time,
            max_time: params.max_time,
            bloom: filter,
        }
    }

    pub fn matches(&self, query: &str) -> bool {
        let tokens: Vec<&str> = query
            .split_whitespace()
            .map(|s| s.trim_matches(|c: char| !c.is_alphanumeric()))
            .filter(|s| !s.is_empty())
            .collect();

        for token in tokens {
            if !self.bloom.contains(&token.to_lowercase()) {
                return false;
            }
        }
        true
    }
}
