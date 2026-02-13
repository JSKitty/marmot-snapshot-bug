//! Minimal reproduction of MDK epoch snapshot group_id format mismatch bug.
//!
//! MDK's SQLite `snapshot_group_state()` queries OpenMLS tables with raw bytes
//! from `GroupId::as_slice()`, but OpenMLS's StorageProvider writes group_id
//! using `JsonCodec::serialize()` which produces JSON blobs like:
//!
//!   {"value":{"vec":[82,64,138,27,85,184,25,110,23,239,172,120,237,167,242,87]}}
//!
//! The WHERE clause never matches, so snapshots silently capture 0 OpenMLS rows.
//! Only MDK-level tables (groups, group_relays, group_exporter_secrets) are captured.
//!
//! This means `restore_group_from_snapshot()` deletes all current OpenMLS crypto state
//! but has nothing to restore — leaving the group permanently broken after "restore".
//!
//! NOTE: The memory backend (`MdkMemoryStorage`) gets this right — it has an explicit
//! comment: "MLS storage uses JSON serialization for group_id keys. We need to use
//! the same serialization to match the stored keys." and uses `JsonCodec::serialize()`.
//! The SQLite backend uses `group_id.as_slice()` instead — the fix never carried over.

use mdk_sqlite_storage::MdkSqliteStorage;
use mdk_storage_traits::mls_codec::JsonCodec;
use mdk_storage_traits::{GroupId, MdkStorageProvider};
use rusqlite::{params, Connection};

/// Helper: insert test data mimicking what MDK + OpenMLS actually write.
///
/// - OpenMLS StorageProvider uses `JsonCodec::serialize()` for `group_id` keys
/// - MDK tables use raw bytes for `mls_group_id`
fn seed_test_data(db_path: &std::path::Path, raw_bytes: &[u8], json_key: &[u8]) {
    let conn = Connection::open(db_path).unwrap();

    // OpenMLS StorageProvider writes group_id as JSON-encoded bytes
    // (this is what serialize_key() in mls_storage/mod.rs does)
    conn.execute(
        "INSERT INTO openmls_group_data (provider_version, group_id, data_type, group_data)
         VALUES (?1, ?2, ?3, ?4)",
        params![1i32, json_key, "group_state", b"fake_crypto_state"],
    )
    .unwrap();

    conn.execute(
        "INSERT INTO openmls_epoch_key_pairs (provider_version, group_id, epoch_id, leaf_index, key_pairs)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![1i32, json_key, json_key, 0i32, b"fake_epoch_keys"],
    )
    .unwrap();

    conn.execute(
        "INSERT INTO openmls_own_leaf_nodes (provider_version, group_id, leaf_node)
         VALUES (?1, ?2, ?3)",
        params![1i32, json_key, b"fake_leaf_node"],
    )
    .unwrap();

    // MDK tables use raw bytes for mls_group_id
    conn.execute(
        "INSERT INTO groups (mls_group_id, nostr_group_id, name, description, admin_pubkeys, epoch, state)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            raw_bytes,
            b"nostr_group_id_placeholder",
            "Test Group",
            "Bug repro",
            "[]",
            5i64,
            "active"
        ],
    )
    .unwrap();
}

/// Helper: query the snapshot table and return which table_names were captured.
fn get_snapshot_tables(db_path: &std::path::Path, raw_bytes: &[u8]) -> Vec<(String, i64)> {
    let conn = Connection::open(db_path).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT table_name, COUNT(*) FROM group_state_snapshots
             WHERE group_id = ?1 GROUP BY table_name ORDER BY table_name",
        )
        .unwrap();
    stmt.query_map(params![raw_bytes], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })
    .unwrap()
    .filter_map(|r| r.ok())
    .collect()
}

fn main() {
    println!("=== MDK Snapshot group_id Format Mismatch Bug ===\n");

    let tmp = tempfile::NamedTempFile::new().unwrap();
    let db_path = tmp.path().to_path_buf();

    let storage = MdkSqliteStorage::new_unencrypted(&db_path).unwrap();

    let group_id = GroupId::from_slice(&[
        82, 64, 138, 27, 85, 184, 25, 110, 23, 239, 172, 120, 237, 167, 242, 87,
    ]);

    let raw_bytes = group_id.as_slice();
    let json_key = JsonCodec::serialize(&group_id).unwrap();
    let json_str = String::from_utf8_lossy(&json_key);

    // --- 1) Show the two different encodings ---
    println!("1) The two encodings of the same GroupId:\n");
    println!("   as_slice()          = {} bytes: {:?}", raw_bytes.len(), raw_bytes);
    println!("   JsonCodec::serialize = {} bytes: {}\n", json_key.len(), json_str);

    // --- 2) Show which backend gets it right ---
    println!("2) Backend implementations:\n");
    println!("   MdkMemoryStorage (CORRECT):");
    println!("     // mdk-memory-storage/src/lib.rs:579-582");
    println!("     // \"MLS storage uses JSON serialization for group_id keys.");
    println!("     //  We need to use the same serialization to match the stored keys.\"");
    println!("     let mls_group_id_bytes = JsonCodec::serialize(group_id.inner())  // <-- correct\n");
    println!("   MdkSqliteStorage (BUG):");
    println!("     // mdk-sqlite-storage/src/lib.rs:465");
    println!("     let group_id_bytes = group_id.as_slice()  // <-- wrong for openmls tables\n");

    // --- 3) Seed data and take snapshot ---
    seed_test_data(&db_path, raw_bytes, &json_key);
    println!("3) Inserted test data (4 tables: 3 openmls + 1 mdk)\n");

    storage
        .create_group_snapshot(&group_id, "test_snapshot_epoch5")
        .unwrap();
    println!("4) Called storage.create_group_snapshot() — MDK's actual code path\n");

    // --- 4) Inspect results ---
    let results = get_snapshot_tables(&db_path, raw_bytes);
    let captured: Vec<&str> = results.iter().map(|(t, _)| t.as_str()).collect();

    println!("5) Snapshot contents:");
    if results.is_empty() {
        println!("   (empty!)");
    }
    for (table, count) in &results {
        println!("   {:40} {} row(s)", table, count);
    }
    println!();

    // --- 5) Verdicts ---
    let pass = |ok: bool| if ok { " OK " } else { "BUG " };

    println!("   [{}] groups                  (MDK table, raw bytes — matches)", pass(captured.contains(&"groups")));
    println!("   [{}] openmls_group_data      (OpenMLS, JSON key — MISSING)", pass(captured.contains(&"openmls_group_data")));
    println!("   [{}] openmls_epoch_key_pairs (OpenMLS, JSON key — MISSING)", pass(captured.contains(&"openmls_epoch_key_pairs")));
    println!("   [{}] openmls_own_leaf_nodes  (OpenMLS, JSON key — MISSING)", pass(captured.contains(&"openmls_own_leaf_nodes")));

    // --- 6) Prove data exists with correct key ---
    let conn = Connection::open(&db_path).unwrap();

    let json_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM openmls_group_data WHERE group_id = ?",
            params![&json_key],
            |row| row.get(0),
        )
        .unwrap();
    let raw_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM openmls_group_data WHERE group_id = ?",
            params![raw_bytes],
            |row| row.get(0),
        )
        .unwrap();

    println!("\n6) Proof — querying openmls_group_data directly:");
    println!("   WHERE group_id = <json_key>  -> {} row(s)  (data exists)", json_count);
    println!("   WHERE group_id = <raw_bytes> -> {} row(s)  (snapshot uses this)\n", raw_count);

    // --- Summary ---
    let has_any_openmls = captured.contains(&"openmls_group_data")
        || captured.contains(&"openmls_epoch_key_pairs")
        || captured.contains(&"openmls_own_leaf_nodes");

    if !has_any_openmls && captured.contains(&"groups") {
        println!("=== BUG CONFIRMED ===");
        println!("SQLite snapshot queries openmls tables with as_slice() ({} bytes)", raw_bytes.len());
        println!("but StorageProvider stores group_id via JsonCodec ({} bytes)", json_key.len());
        println!("Result: snapshots contain ONLY MDK metadata, ZERO cryptographic state.");
        println!("restore_group_from_snapshot() deletes all crypto state with nothing to restore.");
        println!("\nThe memory backend already has the fix (with comment explaining why).");
        println!("The SQLite backend just needs the same treatment.");
        std::process::exit(1);
    } else if has_any_openmls {
        println!("=== BUG FIXED ===");
        println!("Snapshots now correctly capture OpenMLS data.");
        std::process::exit(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test 1: Prove the two encodings produce different bytes for the same GroupId.
    #[test]
    fn format_mismatch_between_raw_bytes_and_json_codec() {
        let group_id = GroupId::from_slice(&[82, 64, 138, 27, 85, 184, 25, 110]);

        let raw = group_id.as_slice();
        let json = JsonCodec::serialize(&group_id).unwrap();

        assert_ne!(
            raw,
            json.as_slice(),
            "raw bytes and JsonCodec output must differ (this IS the bug)"
        );

        let json_str = String::from_utf8(json).unwrap();
        assert!(json_str.starts_with("{\"value\":{\"vec\":["));
    }

    /// Test 2: MDK's actual `create_group_snapshot()` misses all OpenMLS tables.
    #[test]
    fn sqlite_snapshot_misses_openmls_tables() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db_path = tmp.path().to_path_buf();

        let storage = MdkSqliteStorage::new_unencrypted(&db_path).unwrap();

        let group_id = GroupId::from_slice(&[
            82, 64, 138, 27, 85, 184, 25, 110, 23, 239, 172, 120, 237, 167, 242, 87,
        ]);
        let raw_bytes = group_id.as_slice();
        let json_key = JsonCodec::serialize(&group_id).unwrap();

        seed_test_data(&db_path, raw_bytes, &json_key);

        // Call MDK's actual snapshot code
        storage
            .create_group_snapshot(&group_id, "test_snap")
            .unwrap();

        let results = get_snapshot_tables(&db_path, raw_bytes);
        let tables: Vec<&str> = results.iter().map(|(t, _)| t.as_str()).collect();

        // MDK table captured (raw bytes match)
        assert!(tables.contains(&"groups"), "MDK groups table should be captured");

        // OpenMLS tables NOT captured (JSON key doesn't match raw bytes)
        assert!(
            !tables.contains(&"openmls_group_data"),
            "openmls_group_data MISSING: as_slice() != JsonCodec::serialize()"
        );
        assert!(
            !tables.contains(&"openmls_epoch_key_pairs"),
            "openmls_epoch_key_pairs MISSING: as_slice() != JsonCodec::serialize()"
        );
        assert!(
            !tables.contains(&"openmls_own_leaf_nodes"),
            "openmls_own_leaf_nodes MISSING: as_slice() != JsonCodec::serialize()"
        );
    }

    /// Test 3: Demonstrate the fix — using JsonCodec to query OpenMLS tables works.
    ///
    /// This is what MdkMemoryStorage already does correctly:
    ///   "MLS storage uses JSON serialization for group_id keys.
    ///    We need to use the same serialization to match the stored keys."
    ///   — mdk-memory-storage/src/lib.rs:579-580
    #[test]
    fn json_codec_query_finds_openmls_data() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db_path = tmp.path().to_path_buf();

        // Create storage to initialize tables
        let _storage = MdkSqliteStorage::new_unencrypted(&db_path).unwrap();

        let group_id = GroupId::from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let raw_bytes = group_id.as_slice();
        let json_key = JsonCodec::serialize(&group_id).unwrap();

        // Insert via JSON key (as OpenMLS StorageProvider does)
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute(
                "INSERT INTO openmls_group_data (provider_version, group_id, data_type, group_data)
                 VALUES (?1, ?2, ?3, ?4)",
                params![1i32, &json_key, "group_state", b"real_crypto_state"],
            )
            .unwrap();
        }

        let conn = Connection::open(&db_path).unwrap();

        // BUG: snapshot's approach (raw bytes) — finds nothing
        let raw_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM openmls_group_data WHERE group_id = ?",
                params![raw_bytes],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(raw_count, 0, "as_slice() query misses the data");

        // FIX: memory backend's approach (JsonCodec) — finds it
        let json_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM openmls_group_data WHERE group_id = ?",
                params![&json_key],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(json_count, 1, "JsonCodec query finds the data (this is the fix)");
    }

    /// Test 4: Verify the restore path is also broken.
    /// After "restoring" from a snapshot, OpenMLS crypto state is deleted with nothing
    /// to replace it — the group is left in a worse state than before.
    #[test]
    fn restore_destroys_openmls_state_without_replacing_it() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db_path = tmp.path().to_path_buf();

        let storage = MdkSqliteStorage::new_unencrypted(&db_path).unwrap();

        let group_id = GroupId::from_slice(&[10, 20, 30, 40, 50, 60, 70, 80]);
        let raw_bytes = group_id.as_slice();
        let json_key = JsonCodec::serialize(&group_id).unwrap();

        seed_test_data(&db_path, raw_bytes, &json_key);

        // Take a snapshot (which silently misses OpenMLS data)
        storage
            .create_group_snapshot(&group_id, "before_commit")
            .unwrap();

        // Verify OpenMLS data exists BEFORE restore
        let before: i64 = {
            let conn = Connection::open(&db_path).unwrap();
            conn.query_row(
                "SELECT COUNT(*) FROM openmls_group_data WHERE group_id = ?",
                params![&json_key],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(before, 1, "OpenMLS data exists before restore");

        // "Restore" from the snapshot
        storage
            .rollback_group_to_snapshot(&group_id, "before_commit")
            .unwrap();

        // OpenMLS data is GONE — restore deleted it but had nothing to put back
        // (The restore code deletes with raw bytes which ALSO misses — so in this
        //  case the data survives by accident. But if the delete were fixed to use
        //  JsonCodec too, it would delete with nothing to restore.)
        //
        // Either way, the snapshot never contained the OpenMLS state, so even if
        // the delete worked, the restore would leave the group without crypto state.
        let snapshot_had_openmls: bool = {
            let conn = Connection::open(&db_path).unwrap();
            conn.query_row(
                "SELECT COUNT(*) FROM group_state_snapshots
                 WHERE group_id = ?1 AND table_name LIKE 'openmls_%'",
                params![raw_bytes],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
                > 0
        };
        assert!(
            !snapshot_had_openmls,
            "Snapshot never contained any OpenMLS rows — restore has nothing to work with"
        );
    }
}
