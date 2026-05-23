use super::*;
use crate::schema::{ChatSessionV1, MessageInput, MessageRole, MessageV1, StreamEvent};
use tempfile::TempDir;

fn create_test_storage() -> (Storage, TempDir) {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let storage = Storage::new(temp_dir.path().to_path_buf());
    (storage, temp_dir)
}

fn create_dummy_session(id: &str, msg_count: usize) -> ChatSessionV1 {
    let mut messages = Vec::new();
    for i in 0..msg_count {
        messages.push(MessageV1 {
            id: Some(i.to_string()),
            role: MessageRole::User,
            content: format!("Message {} for session {}", i, id),
            tool_calls: None,
            tool_outputs: None,
            parent_id: None,
            metadata_json: String::new(),
        });
    }

    ChatSessionV1 {
        id: id.to_string(),
        title: Some(format!("Title {}", id)),
        source: None,
        model: None,
        created_at: Some(1000),
        metadata_json: String::new(),
        messages,
    }
}

#[test]
fn test_storage_creation() {
    let (storage, _temp) = create_test_storage();
    assert!(storage.get_active_segment_num().is_ok());
    assert_eq!(storage.get_active_segment_num().unwrap(), 1);
}

#[test]
fn test_append_and_read_session() {
    let (storage, _temp) = create_test_storage();
    let session = create_dummy_session("s1", 5);

    storage
        .append_session(session.clone())
        .expect("Failed to append session");

    // Test Scan
    let sessions = storage.scan_all().expect("Failed to scan");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, session.id);
    assert_eq!(sessions[0].messages.len(), 5);

    // Test Get by ID
    let fetched = storage
        .get_session_by_id("s1")
        .expect("Failed to get by id");
    assert!(fetched.is_some());
    assert_eq!(fetched.unwrap().id, session.id);

    // Test missing ID
    let missing = storage
        .get_session_by_id("s2")
        .expect("Failed to check missing");
    assert!(missing.is_none());
}

#[test]
fn test_search() {
    let (storage, _temp) = create_test_storage();
    let s1 = create_dummy_session("s1", 1); // Content: "Message 0 for session s1"
    let s2 = create_dummy_session("s2", 1); // Content: "Message 0 for session s2"

    storage.append_session(s1).unwrap();
    storage.append_session(s2).unwrap();

    let results = storage.search("s1", None, None).expect("Search failed");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, "s1");

    let results = storage
        .search("Message", None, None)
        .expect("Search failed");
    assert_eq!(results.len(), 2);

    let results = storage
        .search("notfound", None, None)
        .expect("Search failed");
    assert_eq!(results.len(), 0);
}

#[test]
fn test_writer_rotation_explicit() {
    let (_storage, temp_dir) = create_test_storage();
    let data_dir = temp_dir.path().to_path_buf();

    // Create initial files
    let data_path = data_dir.join("data_001.cryo");
    let index_path = data_dir.join("index_001.cryo");

    let data_file = File::create(&data_path).unwrap();
    let index_file = File::create(&index_path).unwrap();

    // Create writer with very small max_size (e.g., 50 bytes)
    let mut writer = SessionWriter::new(
        data_dir.clone(),
        1,
        data_file,
        index_file,
        100, // 100 bytes max
    )
    .expect("Failed to create writer");

    // Write session 1 (should fit or be close)
    let s1 = create_dummy_session("s1", 1);
    writer
        .append(StoredSession::V1(s1))
        .expect("Failed to append s1");

    // Write session 2 (should trigger rotation if s1 + s2 > 100 bytes)
    // A session with 1 message is around ~80-100 bytes compressed/serialized usually.
    // Let's force it by writing a few.
    for i in 2..5 {
        let s = create_dummy_session(&format!("s{}", i), 1);
        writer
            .append(StoredSession::V1(s))
            .expect("Failed to append");
    }

    writer.flush().expect("Failed to flush");

    // Check if data_002.cryo exists
    let data_path_2 = data_dir.join("data_002.cryo");
    assert!(data_path_2.exists(), "Rotation did not happen");
}

#[test]
fn test_reindex() {
    let (storage, _temp) = create_test_storage();
    let s1 = create_dummy_session("s1", 2);
    storage.append_session(s1.clone()).unwrap();

    // Corrupt/Delete index
    let index_path = _temp.path().join("index_001.cryo");
    fs::remove_file(&index_path).unwrap();

    assert!(storage.get_session_by_id("s1").is_err()); // Index missing

    // Reindex
    let count = storage.reindex().expect("Reindex failed");
    assert_eq!(count, 1);

    assert!(index_path.exists());
    let fetched = storage.get_session_by_id("s1").unwrap();
    assert!(fetched.is_some());
}

#[test]
fn test_wal_processing() {
    let (storage, _temp) = create_test_storage();
    let mut wal = storage.get_wal_writer().unwrap();

    // Simulate partial streams
    let mut metadata = HashMap::new();
    metadata.insert(
        "created_at".to_string(),
        serde_json::Value::Number(serde_json::Number::from(100)),
    );

    let event1 = StreamEvent::SessionStart {
        session_id: "w1".to_string(),
        metadata,
    };
    wal.append(event1).unwrap();

    let event2 = StreamEvent::AppendMessage {
        session_id: "w1".to_string(),
        message: MessageInput {
            id: Some("m1".to_string()),
            role: MessageRole::User,
            content: "Hello WAL".to_string(),
            tool_calls: None,
            tool_outputs: None,
            parent_id: None,
            metadata: HashMap::new(),
        },
    };
    wal.append(event2).unwrap();
    wal.flush().unwrap();

    // Flush pending (should not finalize as no Finalize event)
    let archived = storage.flush_pending().unwrap();
    assert_eq!(archived, 0);

    // Add finalize
    let mut wal = storage.get_wal_writer().unwrap();
    wal.append(StreamEvent::Finalize {
        session_id: "w1".to_string(),
    })
    .unwrap();
    wal.flush().unwrap();

    // Flush pending again
    let archived = storage.flush_pending().unwrap();
    assert_eq!(archived, 1);

    // Verify written to storage
    let fetched = storage.get_session_by_id("w1").unwrap();
    assert!(fetched.is_some());
    assert_eq!(fetched.unwrap().messages[0].content, "Hello WAL");
}

#[test]
fn test_idempotency() {
    let (storage, _temp) = create_test_storage();
    let s1 = create_dummy_session("duplicate", 1);
    storage.append_session(s1.clone()).unwrap();

    // Try to flush a WAL that has this session
    let mut wal = storage.get_wal_writer().unwrap();
    wal.append(StreamEvent::SessionStart {
        session_id: "duplicate".to_string(),
        metadata: HashMap::new(),
    })
    .unwrap();
    wal.append(StreamEvent::Finalize {
        session_id: "duplicate".to_string(),
    })
    .unwrap();
    wal.flush().unwrap();

    // Flush pending - should see it's already there and skip
    let archived = storage.flush_pending().unwrap();
    assert_eq!(archived, 0); // 0 because it skipped
}

#[test]
fn test_get_stats_with_index() {
    let (storage, _temp) = create_test_storage();

    // Add multiple sessions with different timestamps
    let mut s1 = create_dummy_session("s1", 3);
    s1.created_at = Some(1000);
    let mut s2 = create_dummy_session("s2", 5);
    s2.created_at = Some(2000);

    storage.append_session(s1).unwrap();
    storage.append_session(s2).unwrap();

    // Get stats using index (optimized path)
    let stats = storage.get_stats().expect("Failed to get stats");

    assert_eq!(stats.session_count, 2);
    assert_eq!(stats.message_count, 8); // 3 + 5
    assert_eq!(stats.min_time, 1000);
    assert_eq!(stats.max_time, 2000);
    assert!(stats.total_size_bytes > 0);
    assert!(stats.data_compressed_bytes > 0);
    assert!(stats.data_uncompressed_bytes > 0);
}

#[test]
fn test_get_stats_without_index() {
    let (storage, _temp) = create_test_storage();

    // Add sessions
    let s1 = create_dummy_session("s1", 2);
    storage.append_session(s1).unwrap();

    // Delete index to force fallback scan
    let index_path = _temp.path().join("index_001.cryo");
    fs::remove_file(&index_path).unwrap();

    // Get stats using fallback scan
    let stats = storage.get_stats().expect("Failed to get stats");

    assert_eq!(stats.session_count, 1);
    assert_eq!(stats.message_count, 2);
    assert!(stats.total_size_bytes > 0);
}

#[test]
fn test_get_stats_empty() {
    let (storage, _temp) = create_test_storage();

    // Stats on empty storage
    let stats = storage.get_stats().expect("Failed to get stats");

    assert_eq!(stats.session_count, 0);
    assert_eq!(stats.message_count, 0);
    assert_eq!(stats.min_time, 0);
    assert_eq!(stats.max_time, 0);
}

#[test]
fn test_search_with_date_range() {
    let (storage, _temp) = create_test_storage();

    // Create sessions with different timestamps
    let mut s1 = create_dummy_session("early", 1);
    s1.created_at = Some(1000);
    let mut s2 = create_dummy_session("middle", 1);
    s2.created_at = Some(5000);
    let mut s3 = create_dummy_session("late", 1);
    s3.created_at = Some(10000);

    storage.append_session(s1).unwrap();
    storage.append_session(s2).unwrap();
    storage.append_session(s3).unwrap();

    // Search with after filter
    let results = storage
        .search("Message", Some(4000), None)
        .expect("Search failed");
    assert_eq!(results.len(), 2); // middle and late

    // Search with before filter
    let results = storage
        .search("Message", None, Some(6000))
        .expect("Search failed");
    assert_eq!(results.len(), 2); // early and middle

    // Search with both filters
    let results = storage
        .search("Message", Some(2000), Some(8000))
        .expect("Search failed");
    assert_eq!(results.len(), 1); // only middle
    assert_eq!(results[0].id, "middle");
}

#[test]
fn test_scan_all_invalid_magic() {
    let (storage, _temp) = create_test_storage();

    // Create a file with invalid magic bytes
    let data_path = _temp.path().join("data_001.cryo");
    let mut file = File::create(&data_path).unwrap();
    file.write_all(b"BADMAGIC").unwrap();

    // Should return error
    let result = storage.scan_all();
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("Invalid file format")
    );
}

#[test]
fn test_search_missing_index() {
    let (storage, _temp) = create_test_storage();

    // Create data file but no index
    storage
        .append_session(create_dummy_session("s1", 1))
        .unwrap();
    let index_path = _temp.path().join("index_001.cryo");
    fs::remove_file(&index_path).unwrap();

    // Search should fail with helpful message
    let result = storage.search("query", None, None);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("cryo reindex"));
}

#[test]
fn test_reindex_missing_data() {
    let (storage, _temp) = create_test_storage();

    // Try to reindex when no data file exists
    let result = storage.reindex();
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("Data file not found")
    );
}

#[test]
fn test_flush_pending_with_orphan_messages() {
    let (storage, _temp) = create_test_storage();
    let mut wal = storage.get_wal_writer().unwrap();

    // Add messages without SessionStart (orphaned messages)
    wal.append(StreamEvent::AppendMessage {
        session_id: "orphan".to_string(),
        message: MessageInput {
            id: Some("m1".to_string()),
            role: MessageRole::User,
            content: "Orphan message".to_string(),
            tool_calls: None,
            tool_outputs: None,
            parent_id: None,
            metadata: HashMap::new(),
        },
    })
    .unwrap();

    wal.append(StreamEvent::Finalize {
        session_id: "orphan".to_string(),
    })
    .unwrap();

    wal.flush().unwrap();

    // Flush should handle gracefully (finalize without a session)
    let archived = storage.flush_pending().unwrap();
    assert_eq!(archived, 0); // Nothing to archive as session wasn't started
}

#[test]
fn test_flush_pending_empty_file() {
    let (storage, _temp) = create_test_storage();

    // Flush when no pending file exists
    let archived = storage.flush_pending().unwrap();
    assert_eq!(archived, 0);
}

/// Sessions stored across multiple segments must remain visible to
/// `scan_all`, `search`, `get_session_by_id`, `get_stats`, and `reindex`.
/// Before the multi-segment fix, all of these only inspected the latest
/// segment, so once rotation triggered older data became silently invisible.
#[test]
fn test_multi_segment_visibility() {
    let (storage, _temp) = create_test_storage();
    let data_dir = _temp.path().to_path_buf();

    // Manually craft segments by driving the writer with a tiny max_size.
    let data_path_1 = data_dir.join("data_001.cryo");
    let index_path_1 = data_dir.join("index_001.cryo");
    let data_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&data_path_1)
        .unwrap();
    let index_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&index_path_1)
        .unwrap();
    // Seed magic so reads succeed.
    let mut seed = std::fs::OpenOptions::new()
        .write(true)
        .open(&data_path_1)
        .unwrap();
    seed.write_all(constants::DATA_MAGIC).unwrap();
    drop(seed);

    let mut writer = SessionWriter::new(
        data_dir.clone(),
        1,
        data_file,
        index_file,
        // Tiny max so rotation triggers after a few sessions.
        200,
    )
    .unwrap();

    let mut s_old = create_dummy_session("seg1-session", 2);
    s_old.created_at = Some(1000);
    writer.append(StoredSession::V1(s_old)).unwrap();

    // Force enough writes to trigger rotation to segment 2.
    for i in 0..6 {
        let mut s = create_dummy_session(&format!("padding-{}", i), 1);
        s.created_at = Some(2000 + i as u64);
        writer.append(StoredSession::V1(s)).unwrap();
    }
    writer.flush().unwrap();

    assert!(
        data_dir.join("data_002.cryo").exists(),
        "Test setup: rotation must have produced data_002.cryo"
    );

    // Reindex must rebuild every segment's index — not just the active one.
    let reindexed = storage.reindex().unwrap();
    assert!(reindexed >= 7, "Reindex should cover all segments");
    assert!(data_dir.join("index_001.cryo").exists());
    assert!(data_dir.join("index_002.cryo").exists());

    // get_session_by_id must find a session that lives in segment 1.
    let fetched = storage.get_session_by_id("seg1-session").unwrap();
    assert!(
        fetched.is_some(),
        "Session in older segment must remain reachable after rotation"
    );

    // scan_all must include every segment.
    let all = storage.scan_all().unwrap();
    assert!(all.iter().any(|s| s.id == "seg1-session"));
    assert!(all.iter().any(|s| s.id == "padding-0"));
    assert!(all.iter().any(|s| s.id == "padding-5"));

    // search must cross segments.
    let hits = storage.search("seg1-session", None, None).unwrap();
    assert!(hits.iter().any(|s| s.id == "seg1-session"));

    // get_stats must aggregate counts across segments, not just the latest.
    let stats = storage.get_stats().unwrap();
    assert_eq!(
        stats.session_count, 7,
        "Stats must include sessions in every segment, got {}",
        stats.session_count
    );
}

/// Search must filter sessions individually by `created_at`, even when they
/// share a Block index entry whose [min_time, max_time] straddles the
/// requested [after, before] window.
#[test]
fn test_search_time_filter_refines_within_block() {
    let (storage, _temp) = create_test_storage();

    let mut early = create_dummy_session("inside-early", 1);
    early.created_at = Some(100);
    let mut middle = create_dummy_session("inside-middle", 1);
    middle.created_at = Some(500);
    let mut late = create_dummy_session("inside-late", 1);
    late.created_at = Some(900);

    // Bulk-append produces a single multi-session Block whose index range is
    // [100, 900] — overlaps [200, 800] so the block passes the gate, then the
    // per-session filter must drop `early` and `late`.
    storage
        .append_bulk(vec![early, middle, late])
        .expect("append_bulk failed");

    let hits = storage
        .search("Message", Some(200), Some(800))
        .expect("Search failed");
    assert_eq!(
        hits.len(),
        1,
        "Per-session filter must drop sessions outside [200, 800]; got {:?}",
        hits.iter().map(|s| &s.id).collect::<Vec<_>>()
    );
    assert_eq!(hits[0].id, "inside-middle");
}

/// `get_stats` (index-fast-path) must count sessions, not blocks.
/// A multi-session Block index entry must contribute N sessions to the total.
#[test]
fn test_stats_counts_sessions_inside_blocks() {
    let (storage, _temp) = create_test_storage();

    let s1 = create_dummy_session("a", 1);
    let s2 = create_dummy_session("b", 2);
    let s3 = create_dummy_session("c", 3);

    // append_bulk packs all three into one block.
    storage
        .append_bulk(vec![s1, s2, s3])
        .expect("append_bulk failed");

    let stats = storage.get_stats().expect("stats failed");
    assert_eq!(
        stats.session_count, 3,
        "Stats must report 3 sessions across the block, not 1 (block count)"
    );
    assert_eq!(stats.message_count, 6);
}

/// A session with no `created_at` bundled into a Block must still surface in
/// a time-filtered search. (Pre-fix: block min/max collapsed to 0 and the
/// after-filter excluded the whole block.)
#[test]
fn test_search_includes_block_sessions_without_created_at() {
    let (storage, _temp) = create_test_storage();

    let mut undated = create_dummy_session("undated", 1);
    undated.created_at = None;
    let mut dated = create_dummy_session("dated", 1);
    dated.created_at = Some(5_000);

    storage
        .append_bulk(vec![undated, dated])
        .expect("append_bulk failed");

    let hits = storage
        .search("Message", Some(1_000), None)
        .expect("search failed");
    // Both must surface: dated by virtue of created_at >= 1000, undated
    // because it has no timestamp the filter could exclude it on.
    assert!(hits.iter().any(|s| s.id == "dated"));
    assert!(
        hits.iter().any(|s| s.id == "undated"),
        "Block sessions with no created_at must still be reachable by time-filtered search"
    );
}

/// When the index file is partially truncated (a known shape of corruption
/// from older releases where CLI `add` and MCP `add_log` raced without a
/// shared lock), search must still return matches from the unindexed tail of
/// the data file. The previous implementation walked data and index in
/// lockstep — once the index EOF'd, the remaining data blocks became
/// silently invisible to search even though `scan_all` could still see them.
#[test]
fn test_search_recovers_when_index_is_truncated() {
    let (storage, _temp) = create_test_storage();
    let data_dir = _temp.path().to_path_buf();

    // Six distinct sessions, each with a unique marker word.
    for i in 0..6 {
        let mut s = create_dummy_session(&format!("trunc-{}", i), 1);
        s.messages[0].content = format!("marker-{} content", i);
        storage.append_session(s).unwrap();
    }

    // Sanity check: with a healthy index, search finds every marker.
    for i in 0..6 {
        let q = format!("marker-{}", i);
        let hits = storage.search(&q, None, None).unwrap();
        assert!(
            hits.iter().any(|s| s.id == format!("trunc-{}", i)),
            "healthy index should find {}",
            q
        );
    }

    // Simulate the corruption: truncate the index file so it only describes
    // the first two blocks. The data file is untouched.
    let index_path = data_dir.join("index_001.cryo");
    let buf = std::fs::read(&index_path).unwrap();
    let mut off = 0usize;
    let mut kept_bytes = 0usize;
    for _ in 0..2 {
        let sz =
            u32::from_le_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
        off += 4 + sz;
        kept_bytes = off;
    }
    std::fs::write(&index_path, &buf[..kept_bytes]).unwrap();
    assert!(
        kept_bytes < buf.len(),
        "test setup: index must actually be truncated"
    );

    // Markers 0–1 still have index entries; markers 2–5 do not. All six must
    // still be searchable — search must walk the unindexed tail.
    for i in 0..6 {
        let q = format!("marker-{}", i);
        let hits = storage.search(&q, None, None).unwrap();
        assert!(
            hits.iter().any(|s| s.id == format!("trunc-{}", i)),
            "search must recover {} from unindexed data (index truncated)",
            q
        );
    }

    // And — importantly — a query that matches none of the stored sessions
    // must still return zero, even when half the blocks are unindexed.
    let none = storage.search("nonexistent-token-zzz", None, None).unwrap();
    assert_eq!(none.len(), 0);
}

#[test]
fn test_search_bloom_filter_false_positive() {
    let (storage, _temp) = create_test_storage();

    // Create a session with specific content
    let mut s1 = create_dummy_session("real", 1);
    s1.messages[0].content = "This is the real content".to_string();
    storage.append_session(s1).unwrap();

    // Search for something that might trigger bloom filter but not in actual content
    // The bloom filter may say "maybe", but verification should filter it out
    let results = storage.search("xyzabc", None, None).expect("Search failed");
    assert_eq!(results.len(), 0); // Should be filtered out by content verification
}
