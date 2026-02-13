# MDK Snapshot Bug: `group_id` Format Mismatch

Minimal reproduction of a bug in MDK's epoch snapshot system where **OpenMLS cryptographic state is silently missing from snapshots** due to a `group_id` encoding mismatch.

## The Bug

MDK's `snapshot_group_state()` queries OpenMLS tables using raw bytes from `GroupId::as_slice()`:

```rust
let group_id_bytes = group_id.as_slice();  // 16 raw bytes
// ...
"SELECT * FROM openmls_group_data WHERE group_id = ?"  // binds raw bytes
```

But OpenMLS's `StorageProvider` implementation writes `group_id` using `JsonCodec::serialize()`, producing JSON blobs:

```
{"value":{"vec":[82,64,138,27,85,184,25,110,23,239,172,120,237,167,242,87]}}
```

**16 raw bytes ≠ 76-byte JSON blob** → the `WHERE` clause never matches → snapshots capture **zero** OpenMLS rows.

## Impact

- Snapshots only contain MDK metadata (`groups`, `group_relays`, `group_exporter_secrets`)
- All OpenMLS crypto state (`openmls_group_data`, `openmls_proposals`, `openmls_own_leaf_nodes`, `openmls_epoch_key_pairs`) is **silently missing**
- `restore_group_from_snapshot()` deletes the current OpenMLS state but has **nothing to restore**, leaving the group permanently broken
- Epoch rollback (MIP-03) is effectively non-functional

## Running the Reproduction

```bash
cargo run
```

Expected output:
```
=== MDK Snapshot group_id Format Mismatch Bug ===

GroupId raw bytes (16 bytes): [82, 64, 138, 27, ...]
JsonCodec output  (76 bytes): {"value":{"vec":[82,64,138,27,...]}}

...

Snapshot contents:
  groups                                   1 row(s)

[PASS] groups table captured (MDK, raw bytes)
[BUG ] openmls_group_data MISSING from snapshot (BUG: raw bytes != JSON blob)
[BUG ] openmls_epoch_key_pairs MISSING from snapshot (BUG: raw bytes != JSON blob)

=== BUG CONFIRMED ===
```

Tests:
```bash
cargo test
```

## Suggested Fix

In `snapshot_group_state()` (and the individual `snapshot_openmls_*` helpers), use `JsonCodec::serialize()` to encode the `group_id` before querying OpenMLS tables — the same serialization path that the `StorageProvider` uses when writing:

```rust
// Instead of:
let group_id_bytes = group_id.as_slice();

// Use:
let group_id_key = JsonCodec::serialize(group_id)?;
```

The same fix is needed in `restore_group_from_snapshot()` for the delete queries against OpenMLS tables.

## Affected Code

- `mdk-sqlite-storage/src/lib.rs` — `snapshot_group_state()`, `snapshot_openmls_*()` helpers, `restore_group_from_snapshot()`
- `mdk-sqlite-storage/src/mls_storage/mod.rs` — `serialize_key()` uses `JsonCodec` (the correct path)

## MDK Version

Tested against: `fe69c3e50e4a66755ed4f04d34f5453c1ac6fd5f`
