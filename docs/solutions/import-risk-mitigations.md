# photoferry Import Risk Mitigations

*Council: 2026-02-22 — 6 models, $0.88*
*Full transcript: ~/notes/Councils/LLM Council - Photoferry Import Risk - 2026-02-22.md*

## Already Implemented

- **Metadata (date, GPS):** JSON sidecar → `PHAssetChangeRequest.creationDate/location`
- **Live Photo pairing:** base filename match → `addResource(with: .pairedVideo)` in same request
- **Per-zip dedup:** manifest skips already-imported paths
- **Verify command:** batch PhotoKit query by `local_id` + `creation_date`
- **Pipeline resume:** manifest survives restarts; ZIP not deleted on failure

## High Priority Gaps

### 1. Verify before delete (Priority 1)
`cmd_download` currently deletes ZIP after import, but before verify. The PhotoKit success callback can fire before the asset is truly committed. Fix: run verify on the batch *before* deleting the ZIP.

### 2. Cross-zip duplicate detection (Priority 2)
Google Takeout exports the same photo in year folders AND album folders across different ZIPs. Current manifest is per-zip only. Need a global hash DB (SHA-256 content hash) across all processed zips. The 5,403 duplicates in Photos.app are exactly this.

Options:
- Add global `.photoferry-global-hashes.db` (SQLite, hash → local_id)
- Or rely on Photos.app Utilities → Duplicates after import

### 3. Disk space monitoring (Priority 3)
PhotoKit copies files into `~/Pictures/Photos Library.photoslibrary/originals` before iCloud uploads. 99 zips could fill local disk before iCloud catches up. Pause pipeline when free space < 20GB.

```rust
fn check_disk_space(dir: &Path, min_free_gb: u64) -> bool {
    use std::os::unix::fs::MetadataExt;
    // statvfs via nix or sys call
}
```

### 4. iCloud sync confirmation (Priority 4)
`local_id` only proves local existence. Before cancelling Google One, confirm iCloud sync is complete (Photos.app shows "0 items to upload").

## Deduplication Order of Operations

1. Import all 99 zips with photoferry (cross-zip hashing handles duplicates at import time)
2. OR import all, then use Photos.app Utilities → Duplicates
3. Run `photoferry verify ~/Downloads` across all manifests
4. Delete duplicates only after verify passes
5. Cancel Google One only after verify passes

## Notes on local_id Stability

`local_id` is device-local (`XXXX/L0/001` format). If user deletes and re-creates the local Photos Library, all local_ids become invalid. Mitigations:
- Store `creation_date` alongside `local_id` (already done)
- Store content hash for re-verification (future)
- Never rely solely on `local_id` for long-term deduplication
