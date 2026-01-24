/// Indexing and search logic (Bloom filters, BlockIndex).
pub mod index;
/// File locking mechanism for concurrency safety.
pub mod lock;
/// Data schemas and serialization (DTOs, Events).
pub mod schema;
/// Storage engine core (WAL, Append/Rotate, Compaction).
pub mod storage;
