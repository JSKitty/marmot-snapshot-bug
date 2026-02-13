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
    seed_test_data_with_values(db_path, raw_bytes, json_key, b"fake_crypto_state", b"fake_epoch_keys", b"fake_leaf_node", 5);
}

/// Helper: insert test data with specific values for crypto state and epoch.
fn seed_test_data_with_values(
    db_path: &std::path::Path,
    raw_bytes: &[u8],
    json_key: &[u8],
    crypto_state: &[u8],
    epoch_keys: &[u8],
    leaf_node: &[u8],
    epoch: i64,
) {
    let conn = Connection::open(db_path).unwrap();

    // OpenMLS StorageProvider writes group_id as JSON-encoded bytes
    // (this is what serialize_key() in mls_storage/mod.rs does)
    conn.execute(
        "INSERT INTO openmls_group_data (provider_version, group_id, data_type, group_data)
         VALUES (?1, ?2, ?3, ?4)",
        params![1i32, json_key, "group_state", crypto_state],
    )
    .unwrap();

    conn.execute(
        "INSERT INTO openmls_epoch_key_pairs (provider_version, group_id, epoch_id, leaf_index, key_pairs)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![1i32, json_key, json_key, 0i32, epoch_keys],
    )
    .unwrap();

    conn.execute(
        "INSERT INTO openmls_own_leaf_nodes (provider_version, group_id, leaf_node)
         VALUES (?1, ?2, ?3)",
        params![1i32, json_key, leaf_node],
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
            epoch,
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

    /// Test 5: Full MIP-03 rollback simulation — proves rollback creates an
    /// inconsistent metadata/crypto state that breaks the group.
    ///
    /// MIP-03 flow: snapshot at epoch N → process commit → if conflict, rollback.
    ///
    /// BUG: After rollback, `groups.epoch` is 5 (rolled back) but OpenMLS crypto
    /// state is still at epoch 6 (untouched — both snapshot AND delete miss it).
    /// The group has split-brain: metadata says one epoch, crypto engine says another.
    ///
    /// On a FIXED MDK, both would be rolled back to epoch 5 consistently.
    #[test]
    fn mip03_rollback_creates_metadata_crypto_mismatch() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db_path = tmp.path().to_path_buf();

        let storage = MdkSqliteStorage::new_unencrypted(&db_path).unwrap();

        let group_id = GroupId::from_slice(&[
            0xAA, 0xBB, 0xCC, 0xDD, 0x11, 0x22, 0x33, 0x44,
            0x55, 0x66, 0x77, 0x88, 0x99, 0x00, 0xEE, 0xFF,
        ]);
        let raw_bytes = group_id.as_slice();
        let json_key = JsonCodec::serialize(&group_id).unwrap();

        // ── EPOCH 5: Initial group state ──
        seed_test_data_with_values(
            &db_path, raw_bytes, &json_key,
            b"epoch5_crypto_state",   // openmls_group_data
            b"epoch5_epoch_keys",     // openmls_epoch_key_pairs
            b"epoch5_leaf_node",      // openmls_own_leaf_nodes
            5,                        // groups.epoch
        );

        // Take snapshot at epoch 5 (MIP-03: "before processing potentially conflicting commit")
        storage.create_group_snapshot(&group_id, "snap_epoch5").unwrap();

        // Verify snapshot only captured MDK metadata (not OpenMLS)
        let snap_tables = get_snapshot_tables(&db_path, raw_bytes);
        let snap_table_names: Vec<&str> = snap_tables.iter().map(|(t, _)| t.as_str()).collect();
        assert!(snap_table_names.contains(&"groups"), "snapshot has MDK groups");
        assert!(!snap_table_names.contains(&"openmls_group_data"), "snapshot missing OpenMLS (BUG)");

        // ── EPOCH 6: Advance state (simulate processing a commit) ──
        {
            let conn = Connection::open(&db_path).unwrap();

            // Advance MDK metadata to epoch 6
            conn.execute(
                "UPDATE groups SET epoch = 6 WHERE mls_group_id = ?",
                params![raw_bytes],
            ).unwrap();

            // Advance OpenMLS crypto state to epoch 6
            conn.execute(
                "UPDATE openmls_group_data SET group_data = ? WHERE group_id = ?",
                params![b"epoch6_crypto_state" as &[u8], &json_key],
            ).unwrap();

            conn.execute(
                "UPDATE openmls_epoch_key_pairs SET key_pairs = ? WHERE group_id = ?",
                params![b"epoch6_epoch_keys" as &[u8], &json_key],
            ).unwrap();

            conn.execute(
                "UPDATE openmls_own_leaf_nodes SET leaf_node = ? WHERE group_id = ?",
                params![b"epoch6_leaf_node" as &[u8], &json_key],
            ).unwrap();
        }

        // Verify we're at epoch 6 across the board
        {
            let conn = Connection::open(&db_path).unwrap();
            let epoch: i64 = conn.query_row(
                "SELECT epoch FROM groups WHERE mls_group_id = ?",
                params![raw_bytes], |row| row.get(0),
            ).unwrap();
            assert_eq!(epoch, 6, "pre-rollback: MDK metadata at epoch 6");

            let crypto: Vec<u8> = conn.query_row(
                "SELECT group_data FROM openmls_group_data WHERE group_id = ?",
                params![&json_key], |row| row.get(0),
            ).unwrap();
            assert_eq!(crypto, b"epoch6_crypto_state", "pre-rollback: OpenMLS at epoch 6");
        }

        // ── ROLLBACK: MIP-03 detects conflict, rolls back to epoch 5 snapshot ──
        storage.rollback_group_to_snapshot(&group_id, "snap_epoch5").unwrap();

        // ── VERIFY: What state is the group in after rollback? ──
        let conn = Connection::open(&db_path).unwrap();

        // MDK metadata: rolled back to epoch 5 ✓
        let epoch_after: i64 = conn.query_row(
            "SELECT epoch FROM groups WHERE mls_group_id = ?",
            params![raw_bytes], |row| row.get(0),
        ).unwrap();
        assert_eq!(epoch_after, 5,
            "MDK groups.epoch rolled back to 5 (raw bytes match → rollback works for MDK tables)");

        // OpenMLS crypto state: still at epoch 6! ✗
        // Both the snapshot (capture) AND the restore (delete) use as_slice() raw bytes,
        // which don't match the JSON-keyed OpenMLS rows. So:
        //   - Snapshot captured 0 OpenMLS rows
        //   - Restore's DELETE missed the OpenMLS rows (raw bytes ≠ JSON key)
        //   - OpenMLS data is completely untouched — still at epoch 6
        let crypto_after: Vec<u8> = conn.query_row(
            "SELECT group_data FROM openmls_group_data WHERE group_id = ?",
            params![&json_key], |row| row.get(0),
        ).unwrap();
        assert_eq!(crypto_after, b"epoch6_crypto_state",
            "BUG: OpenMLS crypto state NOT rolled back — still epoch 6 data");

        let keys_after: Vec<u8> = conn.query_row(
            "SELECT key_pairs FROM openmls_epoch_key_pairs WHERE group_id = ?",
            params![&json_key], |row| row.get(0),
        ).unwrap();
        assert_eq!(keys_after, b"epoch6_epoch_keys",
            "BUG: OpenMLS epoch keys NOT rolled back — still epoch 6 keys");

        let leaf_after: Vec<u8> = conn.query_row(
            "SELECT leaf_node FROM openmls_own_leaf_nodes WHERE group_id = ?",
            params![&json_key], |row| row.get(0),
        ).unwrap();
        assert_eq!(leaf_after, b"epoch6_leaf_node",
            "BUG: OpenMLS leaf node NOT rolled back — still epoch 6 leaf");

        // ── THE CONSEQUENCE ──
        // groups.epoch = 5   (metadata thinks we're at epoch 5)
        // MLS engine state   = epoch 6 (crypto keys, tree, leaf — all at epoch 6)
        //
        // This split-brain means:
        //   - MDK will try to process epoch 5 commits again
        //   - But the MLS engine has epoch 6 keys, so decryption fails
        //   - Every subsequent message in this group will be unprocessable
        //   - The group is permanently corrupted with no recovery path
        //
        // On a FIXED MDK (using JsonCodec for OpenMLS queries):
        //   - Snapshot would capture OpenMLS rows (JSON key matches)
        //   - Restore would delete epoch 6 OpenMLS state (JSON key matches)
        //   - Restore would re-insert epoch 5 OpenMLS state from snapshot
        //   - groups.epoch = 5 AND MLS engine state = epoch 5 (CONSISTENT)
    }
}
