---
title: "feat: CLI polish and pre-flight hardening"
type: feat
status: active
date: 2026-02-21
---

# CLI Polish and Pre-Flight Hardening

Harden photoferry before running against a real Takeout zip — better UX, edge-case resilience, and resume ergonomics.

## Acceptance Criteria

- [ ] **Dry-run shows skip count** from existing manifest (`N already imported, skipping`)
- [ ] **Verbose Live Photo label** — `[N/total] IMG_1234.HEIC+MOV -> local_id` instead of just the photo filename
- [ ] **Progress bar shows rate + ETA** — `[###---] 42/100 3.2 files/s ETA 18s`
- [ ] **Corrupt/partial zip** — graceful error with message, skip to next zip (not panic/abort)
- [ ] **Stale extract dir** — if `.photoferry-extract-*` already exists, wipe it cleanly before extracting
- [ ] **Duplicate local_ids in manifest** — deduplicate on write (same path re-imported gives same id)
- [ ] **`--retry-failed` flag** — re-attempt only files in the manifest's `failed` list
- [ ] **Warnings don't bleed into progress bar** — use `pb.println()` for mid-loop warnings

## Context

### Key files
- `src/main.rs:98` — `cmd_run` (dry_run, verbose, manifest logic)
- `src/main.rs:368` — `import_inventory` (progress bar, import dispatch)
- `src/main.rs:534` — `print_import_summary`
- `src/manifest.rs:75` — `merge_and_write` (currently bypassed in cmd_run)
- `Cargo.toml:20` — `indicatif = "0.17"` (already supports `{per_sec}` and `{eta}`)

### Research findings
- **`cmd_run` bypasses `merge_and_write`** (lines 173–221): ~40 lines manually duplicate what `merge_and_write` does, but without the "remove retried failures" cleanup. Replace with `merge_and_write`.
- **`pb.println()` needed**: `display::print_warning` writes to stdout and interleaves with the indicatif bar. Switch in-loop warnings to `pb.println()`.
- **indicatif ETA**: Change template from `"[{bar:40}] {pos}/{len} {msg}"` to `"[{bar:40}] {pos}/{len} {per_sec:.1} ETA {eta} {msg}"`.
- **Live Photo dispatch is done** (Phase 3b) but verbose output still shows only the photo filename.

## Implementation

### 1. Dry-run skip count (`src/main.rs:cmd_run`)

After reading existing manifest (line 148), when `dry_run`, print skips before the inventory summary:

```rust
// After already_imported HashSet is built:
if dry_run && !already_imported.is_empty() {
    display::print_info(&format!(
        "{} already imported (skipping)", already_imported.len()
    ));
}
```

### 2. Progress bar: rate + ETA (`src/main.rs:390`)

Change the template string:

```rust
// Before:
ProgressStyle::with_template("[{bar:40}] {pos}/{len} {msg}")

// After:
ProgressStyle::with_template("[{bar:40}] {pos}/{len} {per_sec:.1}/s ETA {eta} {msg}")
```

No dependency changes needed — indicatif 0.17 supports both placeholders.

### 3. Verbose Live Photo label (`src/main.rs:471`)

In the success branch of `import_inventory`, detect Live Photo:

```rust
if verbose {
    let label = if file.live_photo_pair.is_some() {
        let video_name = file.live_photo_pair.as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        format!("{}+{}", filename, video_name)
    } else {
        filename.clone()
    };
    display::print_success(&format!("[{}/{}] {} -> {}", index + 1, total, label, local_id));
}
```

### 4. Fix warning/progress bar interleaving (`src/main.rs:import_inventory`)

Replace `display::print_warning` calls inside the import loop with `pb.println()`:

```rust
// Before:
display::print_warning(&format!("Failed to add '{}' to album '{}'", ...));

// After:
pb.println(format!("  ! Failed to add '{}' to album '{}'", ...));
```

Apply to all `print_warning` and `print_error` calls inside the `for (index, file)` loop (lines ~449–470).

### 5. Stale extract dir cleanup (`src/main.rs:136`)

Before `create_dir_all`, check and wipe if exists:

```rust
if extract_dir.exists() {
    display::print_info(&format!(
        "Cleaning stale extract dir: {}",
        extract_dir.display()
    ));
    std::fs::remove_dir_all(&extract_dir)?;
}
std::fs::create_dir_all(&extract_dir)?;
```

### 6. Corrupt/partial zip — graceful skip (`src/main.rs:145`)

Wrap `extract_zip` in a match to skip rather than abort:

```rust
let content_root = match takeout::extract_zip(zip_path, &extract_dir) {
    Ok(root) => root,
    Err(e) => {
        display::print_error(&format!(
            "Skipping {} — failed to extract: {}",
            zip_path.file_name().unwrap_or_default().to_string_lossy(),
            e
        ));
        let _ = std::fs::remove_dir_all(&extract_dir);
        continue;
    }
};
```

### 7. Replace manual manifest merge with `merge_and_write` (`src/main.rs:172`)

Replace lines 172–221 with:

```rust
let new_imported: Vec<(String, String)> = summary.imported.iter().map(|file| {
    (
        file.path.strip_prefix(&content_root).unwrap_or(&file.path)
            .to_string_lossy().to_string(),
        file.local_id.clone(),
    )
}).collect();

let new_failed: Vec<(String, String)> = summary.failed.iter().map(|file| {
    let p = Path::new(&file.path);
    (
        p.strip_prefix(&content_root).unwrap_or(p)
            .to_string_lossy().to_string(),
        file.error.clone(),
    )
}).collect();

manifest::merge_and_write(
    &manifest_path,
    &zip_path.file_name().unwrap_or_default().to_string_lossy(),
    &new_imported,
    &new_failed,
)?;
```

### 8. Deduplicate manifest on write (`src/manifest.rs:write_manifest`)

Before building the final manifest struct, dedup the `imported` slice by path (last wins):

```rust
// Dedup: if same path appears twice, keep the last entry
use std::collections::HashMap;
let mut seen: HashMap<&str, &(String, String)> = HashMap::new();
for entry in imported.iter() {
    seen.insert(entry.0.as_str(), entry);
}
// Preserve insertion order by filtering original slice
let imported_deduped: Vec<&(String, String)> = imported.iter()
    .filter(|e| seen.get(e.0.as_str()).map(|v| *v as *const _) == Some(e as *const _))
    .collect();
```

Or simpler: deduplicate in `merge_and_write` before calling `write_manifest`:

```rust
// In merge_and_write, after extending:
let mut seen = HashSet::new();
imported.retain(|(p, _)| seen.insert(p.clone()));
```

### 9. `--retry-failed` flag (`src/main.rs:Commands::Run`)

Add to the `Run` subcommand:

```rust
/// Re-attempt only files that failed in a previous run
#[arg(long)]
retry_failed: bool,
```

In `cmd_run`, after reading the existing manifest, if `retry_failed`:
1. Build a `HashSet` of failed paths from manifest
2. Override `inventory.files.retain()` to keep only those paths
3. Clear the failed list in the manifest before writing (so a clean run replaces old failures)

```rust
if retry_failed {
    let failed_paths: HashSet<String> = existing_manifest.as_ref()
        .map(|m| m.failed.iter().map(|e| e.path.clone()).collect())
        .unwrap_or_default();
    if failed_paths.is_empty() {
        display::print_info("No previously-failed files to retry.");
        return Ok(());
    }
    display::print_info(&format!("Retrying {} previously-failed files", failed_paths.len()));
    inventory.files.retain(|file| {
        let relative = file.path.strip_prefix(&content_root)
            .unwrap_or(&file.path).to_string_lossy().to_string();
        failed_paths.contains(&relative)
    });
}
```

## Sources & References

- `src/main.rs:368` — `import_inventory` (progress bar + import loop)
- `src/main.rs:172` — manual manifest merge (replace with `merge_and_write`)
- `src/manifest.rs:75` — `merge_and_write`
- `indicatif 0.17` — supports `{per_sec}`, `{eta}`, `{elapsed_precise}`, `pb.println()`
