---
title: "feat: Basic import loop — wire Takeout inventory to PhotoKit"
type: feat
status: active
date: 2026-02-21
---

# Basic Import Loop (Phase 3a)

Wire `TakeoutInventory` scanning to `import_photo()` FFI calls with progress reporting, error handling, and a results manifest.

## Overview

Phase 2 scans Takeout ZIPs and produces `Vec<MediaFile>` with pre-parsed `PhotoMetadata`. Phase 1's Swift FFI already accepts a file path + metadata JSON and returns `ImportResult`. Phase 3a connects these two: iterate files, call importer, report progress, handle errors gracefully.

## Problem Statement

The `cmd_run` flow currently prints `"Import not yet implemented (Phase 3)"` at `src/main.rs:122`. The inventory is scanned, stats printed, then extraction cleaned up — but nothing is imported.

## Proposed Solution

### Import Loop (`src/main.rs`)

Replace the Phase 3 placeholder in `cmd_run`. For each `MediaFile` in `inventory.files`:

1. Call `importer::import_photo(path.to_str(), metadata.as_ref())`
2. On success: record `local_identifier`, print progress
3. On failure: log error, continue to next file
4. After all files: print summary (imported / failed / skipped)

```rust
// src/main.rs — inside cmd_run, replacing the placeholder
if !dry_run {
    let results = import_inventory(&inventory)?;
    print_import_summary(&results);
}
```

### Progress Reporting

Add `indicatif` for a progress bar. Single bar showing `[###-------] 142/500 sunset.jpg`. Falls back to simple line-by-line output if `--verbose` flag is passed (useful for piping/logging).

No need for multi-bar or spinner complexity — one bar, one job.

### Error Handling Strategy

**Continue on error.** A single failed photo should not abort 10,000 others. Collect errors and report at end.

```rust
struct ImportSummary {
    imported: usize,
    failed: Vec<(PathBuf, String)>,  // (path, error message)
    skipped: usize,                   // dry_run or already-imported
}
```

Print failed files at the end with paths and error messages so the user can retry or investigate.

### Results Manifest (Resume Support)

Write a JSON manifest alongside each ZIP after processing:

```
.photoferry-manifest-takeout-20240101.json
```

Contains:
```json
{
  "zip": "takeout-20240101.zip",
  "processed_at": "2026-02-21T15:30:00Z",
  "imported": [
    { "path": "Photos from 2024/sunset.jpg", "local_id": "ABC123" }
  ],
  "failed": [
    { "path": "Photos from 2024/corrupt.jpg", "error": "Invalid image data" }
  ]
}
```

On subsequent runs, check manifest before importing — skip files already successfully imported. This makes interrupted runs resumable without re-importing everything.

### Photos Access Pre-Check

Before starting the import loop, call `importer::check_access()`. If not authorized, print the System Settings path and bail early instead of failing on every file.

## Technical Considerations

**Extraction timing:** The current flow extracts → scans → cleans up, all within the same loop body. Import must happen between scan and cleanup. This is already the case structurally — the placeholder is in the right spot.

**Memory:** `inventory.files` holds all `MediaFile` structs in memory. For a typical Takeout (5-50K photos), this is ~1-5MB — fine. No streaming needed.

**Import speed:** Each `import_photo()` call blocks on a semaphore waiting for PhotoKit. Expect ~100-500ms per photo. 10K photos ≈ 15-80 minutes. Progress bar is essential.

**File paths:** `MediaFile.path` is absolute (set during `walkdir` traversal of the extraction dir). Must remain valid through the import — cleanup happens after.

## Acceptance Criteria

- [ ] `photoferry run ~/Downloads` imports all photos from Takeout ZIPs into Photos.app
- [ ] `photoferry run --dry-run` continues to work (scan only, no import)
- [ ] `photoferry run --once` imports from a single ZIP and stops
- [ ] Progress bar shows current file count and filename during import
- [ ] Failed imports are logged and reported in summary, don't abort the run
- [ ] Photos access is checked before starting import; clear error message if denied
- [ ] JSON manifest is written per-ZIP; re-running skips already-imported files
- [ ] `--verbose` flag shows per-file import results instead of progress bar

## MVP

### New dependency — `Cargo.toml`

```toml
indicatif = "0.17"
```

### Import logic — `src/main.rs`

```rust
fn import_inventory(inventory: &TakeoutInventory, verbose: bool) -> Result<ImportSummary> {
    let total = inventory.files.len();
    let pb = if verbose {
        ProgressBar::hidden()
    } else {
        ProgressBar::new(total as u64)
    };
    pb.set_style(/* "[{bar:40}] {pos}/{len} {msg}" */);

    let mut summary = ImportSummary::default();

    for (i, file) in inventory.files.iter().enumerate() {
        let path_str = file.path.to_str().context("Invalid path")?;
        let filename = file.path.file_name().unwrap_or_default().to_string_lossy();
        pb.set_message(filename.to_string());

        match importer::import_photo(path_str, file.metadata.as_ref()) {
            Ok(result) if result.success => {
                summary.imported += 1;
                if verbose {
                    display::print_success(&format!("[{}/{}] {}", i + 1, total, filename));
                }
            }
            Ok(result) => {
                let err = result.error.unwrap_or_else(|| "unknown error".into());
                summary.failed.push((file.path.clone(), err.clone()));
                if verbose {
                    display::print_error(&format!("[{}/{}] {} — {}", i + 1, total, filename, err));
                }
            }
            Err(e) => {
                summary.failed.push((file.path.clone(), e.to_string()));
                if verbose {
                    display::print_error(&format!("[{}/{}] {} — {}", i + 1, total, filename, e));
                }
            }
        }

        pb.inc(1);
    }

    pb.finish_and_clear();
    Ok(summary)
}
```

### Manifest — `src/manifest.rs`

```rust
#[derive(Serialize, Deserialize)]
struct ImportManifest {
    zip: String,
    processed_at: String,
    imported: Vec<ManifestEntry>,
    failed: Vec<ManifestFailure>,
}

#[derive(Serialize, Deserialize)]
struct ManifestEntry {
    path: String,
    local_id: String,
}

#[derive(Serialize, Deserialize)]
struct ManifestFailure {
    path: String,
    error: String,
}
```

Read before import, filter out already-imported paths, write after import completes.

## Files to Modify

- `Cargo.toml` — add `indicatif = "0.17"`
- `src/main.rs` — replace placeholder with import loop, add `--verbose` flag, add access pre-check
- `src/display.rs` — add `print_warning()` for skipped/resumed files (optional)

## Files to Create

- `src/manifest.rs` — manifest read/write + resume filtering

## Success Metrics

- Successful import of a real Takeout ZIP end-to-end (even a small test one)
- Re-run after completion skips all files (manifest resume works)
- Interrupted run + re-run picks up where it left off

## Dependencies & Risks

- **PhotoKit authorization:** User must grant Full Access in System Settings. The pre-check catches this, but first-time users may not realize.
- **PhotoKit rate limits:** Unknown if Photos.app throttles rapid `performChanges` calls. If imports start failing mid-run, may need a small delay between calls.
- **Disk space:** ZIP extraction doubles storage temporarily. For large Takeouts (50GB+), this matters. Already handled by per-ZIP extract+cleanup cycle.

## Sources

- `src/importer.rs:62-72` — `import_photo()` public API
- `src/takeout.rs:132-228` — `scan_directory()` and `TakeoutInventory`
- `src/main.rs:86-131` — current `cmd_run` with Phase 3 placeholder at line 122
- `swift/Sources/PhotoFerrySwift.swift:87-186` — Swift import implementation
- MEMORY.md — swift-rs gotchas (Bool type, SwiftPM cache)
