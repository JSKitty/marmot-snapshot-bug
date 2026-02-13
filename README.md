# MDK Snapshot Bug: `group_id` Format Mismatch (SQLite Backend)

Minimal reproduction of a bug in MDK's SQLite epoch snapshot system where **OpenMLS cryptographic state is silently missing from snapshots** due to a `group_id` encoding mismatch.

## The Bug

MDK's SQLite `snapshot_group_state()` queries OpenMLS tables using raw bytes from `GroupId::as_slice()`:

```rust
// mdk-sqlite-storage/src/lib.rs:465
let group_id_bytes = group_id.as_slice();  // 16 raw bytes

// Then used in queries like:
"SELECT * FROM openmls_group_data WHERE group_id = ?"  // binds raw bytes
```

But OpenMLS's `StorageProvider` implementation writes `group_id` using `JsonCodec::serialize()`, producing JSON blobs:

```
{"value":{"vec":[82,64,138,27,85,184,25,110,23,239,172,120,237,167,242,87]}}
```

**16 raw bytes ≠ 76-byte JSON blob** → the `WHERE` clause never matches → snapshots capture **zero** OpenMLS rows.

## The Memory Backend Already Has the Fix

The memory storage implementation (`MdkMemoryStorage`) gets this right, with an explicit comment explaining why:

```rust
// mdk-memory-storage/src/lib.rs:579-582
//
// MLS storage uses JSON serialization for group_id keys.
// We need to use the same serialization to match the stored keys.
let mls_group_id_bytes = mls_storage::JsonCodec::serialize(group_id.inner())
    .expect("Failed to serialize group_id for MLS lookup");
```

The SQLite backend uses `group_id.as_slice()` instead — the awareness and fix never carried over from memory to SQLite.

## Impact

- Snapshots only contain MDK metadata (`groups`, `group_relays`, `group_exporter_secrets`)
- All OpenMLS crypto state (`openmls_group_data`, `openmls_proposals`, `openmls_own_leaf_nodes`, `openmls_epoch_key_pairs`) is **silently missing**
- `restore_group_from_snapshot()` has **nothing to restore** for the cryptographic state
- Epoch rollback (MIP-03) is effectively non-functional for the SQLite backend
- This was introduced in PR #152 — the snapshot tests only exercise MDK-level data (`save_group`, `save_group_exporter_secret`), never writing through the OpenMLS `StorageProvider` trait

## Running the Reproduction

```bash
cargo run
```

Expected output:
```
=== MDK Snapshot group_id Format Mismatch Bug ===

1) The two encodings of the same GroupId:

   as_slice()          = 16 bytes: [82, 64, 138, ...]
   JsonCodec::serialize = 76 bytes: {"value":{"vec":[82,64,138,...]}}

2) Backend implementations:

   MdkMemoryStorage (CORRECT):
     // "MLS storage uses JSON serialization for group_id keys.
     //  We need to use the same serialization to match the stored keys."
     let mls_group_id_bytes = JsonCodec::serialize(group_id.inner())

   MdkSqliteStorage (BUG):
     let group_id_bytes = group_id.as_slice()

...

5) Snapshot contents:
   groups                                   1 row(s)

   [ OK ] groups                  (MDK table, raw bytes — matches)
   [BUG ] openmls_group_data      (OpenMLS, JSON key — MISSING)
   [BUG ] openmls_epoch_key_pairs (OpenMLS, JSON key — MISSING)
   [BUG ] openmls_own_leaf_nodes  (OpenMLS, JSON key — MISSING)

=== BUG CONFIRMED ===
```

Tests (4 tests, all pass — confirming the bug):
```bash
cargo test
```

| Test | What it proves |
|------|---------------|
| `format_mismatch_between_raw_bytes_and_json_codec` | `as_slice()` and `JsonCodec::serialize()` produce different bytes |
| `sqlite_snapshot_misses_openmls_tables` | `create_group_snapshot()` captures MDK tables but misses all OpenMLS tables |
| `json_codec_query_finds_openmls_data` | Using `JsonCodec` to query (like memory backend) finds the data |
| `restore_destroys_openmls_state_without_replacing_it` | Snapshot never contained OpenMLS rows — restore has nothing to work with |

## Suggested Fix

In `snapshot_group_state()` and the individual `snapshot_openmls_*` helpers, use `JsonCodec::serialize()` to encode the `group_id` before querying OpenMLS tables — the same approach the memory backend already uses:

```rust
// Instead of:
let group_id_bytes = group_id.as_slice();

// For OpenMLS table queries, use:
let openmls_key = JsonCodec::serialize(group_id)?;
```

The same fix is needed in `restore_group_from_snapshot()` for the delete queries against OpenMLS tables. The 3 MDK table queries (`groups`, `group_relays`, `group_exporter_secrets`) should continue using raw bytes since those tables store `mls_group_id` as raw bytes.

## MDK Version

Tested against: `fe69c3e50e4a66755ed4f04d34f5453c1ac6fd5f`

Bug introduced in: [PR #152](https://github.com/marmot-protocol/mdk/pull/152) (MIP-03 deterministic commit race resolution)
