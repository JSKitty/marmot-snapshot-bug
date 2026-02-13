//! Minimal reproduction of MDK epoch snapshot group_id format mismatch bug.
//!
//! MDK's `snapshot_group_state()` queries OpenMLS tables with raw bytes from
//! `GroupId::as_slice()`, but OpenMLS's StorageProvider writes group_id using
//! `JsonCodec::serialize()` which produces JSON blobs like:
//!
//!   {"value":{"vec":[82,64,138,27,85,184,25,110,23,239,172,120,237,167,242,87]}}
//!
//! The WHERE clause never matches, so snapshots silently capture 0 OpenMLS rows.
//! Only MDK-level tables (groups, group_relays, group_exporter_secrets) are captured.
//!
//! This means `restore_group_from_snapshot()` deletes all current OpenMLS crypto state
//! but has nothing to restore — leaving the group permanently broken after "restore".

use mdk_sqlite_storage::MdkSqliteStorage;
use mdk_storage_traits::mls_codec::JsonCodec;
use mdk_storage_traits::{GroupId, MdkStorageProvider};
use rusqlite::{params, Connection};

fn main() {
    println!("=== MDK Snapshot group_id Format Mismatch Bug ===\n");

    let tmp = tempfile::NamedTempFile::new().unwrap();
    let db_path = tmp.path().to_path_buf();

    // 1) Create MdkSqliteStorage (initializes all tables via migrations)
    let storage = MdkSqliteStorage::new_unencrypted(&db_path).unwrap();

    let group_id = GroupId::from_slice(&[
        82, 64, 138, 27, 85, 184, 25, 110, 23, 239, 172, 120, 237, 167, 242, 87,
    ]);

    // 2) Show the format mismatch
    let raw_bytes = group_id.as_slice();
    let json_key = JsonCodec::serialize(&group_id).unwrap();
    let json_str = String::from_utf8_lossy(&json_key);

    println!("GroupId raw bytes ({} bytes): {:?}", raw_bytes.len(), raw_bytes);
    println!("JsonCodec output  ({} bytes): {}\n", json_key.len(), json_str);

    // 3) Insert test data via separate connection (mimicking normal MDK/OpenMLS writes)
    {
        let conn = Connection::open(&db_path).unwrap();

        // OpenMLS StorageProvider writes group_id as JSON-encoded bytes
        conn.execute(
            "INSERT INTO openmls_group_data (provider_version, group_id, data_type, group_data)
             VALUES (?1, ?2, ?3, ?4)",
            params![1i32, &json_key, "group_state", b"fake_crypto_state_data"],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO openmls_epoch_key_pairs (provider_version, group_id, epoch_id, leaf_index, key_pairs)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i32, &json_key, &json_key, 0i32, b"fake_epoch_key_pairs"],
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

        println!("Inserted test data:");
        println!("  - openmls_group_data: 1 row (group_id = JSON blob, {} bytes)", json_key.len());
        println!("  - openmls_epoch_key_pairs: 1 row (group_id = JSON blob, {} bytes)", json_key.len());
        println!("  - groups: 1 row (mls_group_id = raw bytes, {} bytes)\n", raw_bytes.len());
    }

    // 4) Call MDK's actual snapshot code path
    println!("Calling storage.create_group_snapshot()...");
    storage
        .create_group_snapshot(&group_id, "test_snapshot_epoch5")
        .unwrap();
    println!("Snapshot created successfully.\n");

    // 5) Inspect what the snapshot captured
    {
        let conn = Connection::open(&db_path).unwrap();

        // List all captured table_name entries
        let mut stmt = conn
            .prepare(
                "SELECT table_name, COUNT(*) FROM group_state_snapshots
                 WHERE group_id = ?1 GROUP BY table_name ORDER BY table_name",
            )
            .unwrap();

        let results: Vec<(String, i64)> = stmt
            .query_map(params![raw_bytes], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        println!("Snapshot contents:");
        if results.is_empty() {
            println!("  (empty - no rows captured at all!)");
        }
        for (table, count) in &results {
            println!("  {:40} {} row(s)", table, count);
        }

        let captured_tables: Vec<&str> = results.iter().map(|(t, _)| t.as_str()).collect();
        println!();

        // Check MDK tables (use raw bytes — should match)
        let has_groups = captured_tables.contains(&"groups");
        println!(
            "[{}] groups table captured (MDK, raw bytes)",
            if has_groups { "PASS" } else { "FAIL" }
        );

        // Check OpenMLS tables (use raw bytes — should NOT match due to bug)
        let has_openmls_group_data = captured_tables.contains(&"openmls_group_data");
        let has_openmls_epoch_keys = captured_tables.contains(&"openmls_epoch_key_pairs");

        println!(
            "[{}] openmls_group_data MISSING from snapshot (BUG: raw bytes != JSON blob)",
            if !has_openmls_group_data {
                "BUG "
            } else {
                "FIXED"
            }
        );
        println!(
            "[{}] openmls_epoch_key_pairs MISSING from snapshot (BUG: raw bytes != JSON blob)",
            if !has_openmls_epoch_keys {
                "BUG "
            } else {
                "FIXED"
            }
        );

        // Prove the data exists — query with the correct JSON key
        let real_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM openmls_group_data WHERE group_id = ?",
                params![&json_key],
                |row| row.get(0),
            )
            .unwrap();
        println!(
            "\nProof: openmls_group_data has {} row(s) when queried with JSON key",
            real_count
        );

        let bad_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM openmls_group_data WHERE group_id = ?",
                params![raw_bytes],
                |row| row.get(0),
            )
            .unwrap();
        println!(
            "Proof: openmls_group_data has {} row(s) when queried with raw bytes (snapshot uses this)\n",
            bad_count
        );

        // Summary
        if !has_openmls_group_data && !has_openmls_epoch_keys && has_groups {
            println!("=== BUG CONFIRMED ===");
            println!("snapshot_group_state() queries openmls tables with raw bytes ({} bytes)", raw_bytes.len());
            println!("but OpenMLS StorageProvider stores group_id as JSON blobs ({} bytes)", json_key.len());
            println!("Result: snapshots contain ONLY MDK metadata, ZERO cryptographic state.");
            println!("restore_group_from_snapshot() would delete all crypto state with nothing to restore.");
            std::process::exit(1);
        } else if has_openmls_group_data && has_openmls_epoch_keys {
            println!("=== BUG FIXED ===");
            println!("Snapshots now correctly capture OpenMLS data.");
            std::process::exit(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_mismatch_between_raw_bytes_and_json_codec() {
        let group_id = GroupId::from_slice(&[82, 64, 138, 27, 85, 184, 25, 110]);

        let raw_bytes = group_id.as_slice();
        let json_key = JsonCodec::serialize(&group_id).unwrap();

        // These MUST differ for the bug to exist
        assert_ne!(
            raw_bytes,
            json_key.as_slice(),
            "If these are equal, the bug is fixed in JsonCodec"
        );

        // Verify the JSON structure
        let json_str = String::from_utf8(json_key.clone()).unwrap();
        assert!(
            json_str.contains("\"value\""),
            "JsonCodec should produce a JSON object with 'value' key"
        );
        assert!(
            json_str.contains("\"vec\""),
            "JsonCodec should produce a JSON object with 'vec' key"
        );
    }

    #[test]
    fn snapshot_misses_openmls_data() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db_path = tmp.path().to_path_buf();

        let storage = MdkSqliteStorage::new_unencrypted(&db_path).unwrap();

        let group_id = GroupId::from_slice(&[
            82, 64, 138, 27, 85, 184, 25, 110, 23, 239, 172, 120, 237, 167, 242, 87,
        ]);

        let raw_bytes = group_id.as_slice();
        let json_key = JsonCodec::serialize(&group_id).unwrap();

        // Insert data the way MDK + OpenMLS actually writes it
        {
            let conn = Connection::open(&db_path).unwrap();

            // OpenMLS StorageProvider uses JsonCodec for group_id
            conn.execute(
                "INSERT INTO openmls_group_data (provider_version, group_id, data_type, group_data)
                 VALUES (?1, ?2, ?3, ?4)",
                params![1i32, &json_key, "group_state", b"crypto_state"],
            )
            .unwrap();

            // MDK tables use raw bytes
            conn.execute(
                "INSERT INTO groups (mls_group_id, nostr_group_id, name, description, admin_pubkeys, epoch, state)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    raw_bytes,
                    b"nostr_id",
                    "Test",
                    "",
                    "[]",
                    5i64,
                    "active"
                ],
            )
            .unwrap();
        }

        // Take snapshot via MDK's actual code path
        storage
            .create_group_snapshot(&group_id, "test_snap")
            .unwrap();

        // Verify the snapshot
        let conn = Connection::open(&db_path).unwrap();

        let captured_tables: Vec<String> = {
            let mut stmt = conn
                .prepare(
                    "SELECT DISTINCT table_name FROM group_state_snapshots WHERE group_id = ?",
                )
                .unwrap();
            stmt.query_map(params![raw_bytes], |row| row.get(0))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect()
        };

        // MDK table should be captured (raw bytes match raw bytes)
        assert!(
            captured_tables.contains(&"groups".to_string()),
            "groups table should be in snapshot (MDK uses raw bytes consistently)"
        );

        // BUG: OpenMLS table should be captured but ISN'T
        assert!(
            !captured_tables.contains(&"openmls_group_data".to_string()),
            "BUG: openmls_group_data is MISSING because snapshot queries with raw bytes \
             ({} bytes) but the table stores JSON-encoded keys ({} bytes)",
            raw_bytes.len(),
            json_key.len()
        );

        // Prove the data exists with the correct key
        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM openmls_group_data WHERE group_id = ?",
                params![&json_key],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(
            exists,
            "Data IS in the table — just invisible to snapshot queries"
        );
    }
}
