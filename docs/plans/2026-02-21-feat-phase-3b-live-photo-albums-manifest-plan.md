---
title: "feat: Phase 3b — Live Photo import, album assignment, resume manifest"
type: feat
status: active
date: 2026-02-21
---

# Phase 3b — Live Photo Import, Album Assignment, Resume Manifest

Three independent features that complete the import pipeline. Can be implemented in any order; all require a shared prerequisite: tracking `local_identifier` per imported file.

## Overview

Phase 3a wires inventory → import calls with progress and error handling. Phase 3b adds:

1. **Live Photo paired import** — new Swift FFI using `PHAssetCreationRequest` to import HEIC+MOV as a single Live Photo asset
2. **Album creation + assignment** — wire existing `create_album`/`add_to_album` FFI into the import loop
3. **Resume manifest** — JSON file per ZIP to skip already-imported files on re-runs

## Prerequisite: Track local_identifier per imported file

Currently `ImportSummary` only counts successes. Both album assignment and manifest writing need the `local_identifier` returned by `import_photo()`.

### Changes to `src/main.rs`

```rust
struct ImportedFile {
    path: PathBuf,
    local_id: String,
    album: Option<String>,
}

struct ImportSummary {
    imported: Vec<ImportedFile>,  // was: usize count
    failed: Vec<ImportFailure>,
    elapsed: std::time::Duration,
}
```

Update `import_inventory()` to push `ImportedFile` entries on success, capturing `result.local_identifier`. Progress count becomes `summary.imported.len()`.

## Feature 1: Live Photo Paired Import

### Problem

`MediaFile.live_photo_pair` is populated during scanning (paired videos excluded from file list), but the import loop ignores it. Currently Live Photos import as standalone stills — the video component is lost.

### Swift-side: new FFI function

**File:** `swift/Sources/PhotoFerrySwift.swift`

Add `photoferry_import_live_photo(photoPath, videoPath, metadataJSON) -> SRString`:

```swift
@_cdecl("photoferry_import_live_photo")
public func importLivePhoto(photoPath: SRString, videoPath: SRString, metadataJSON: SRString) -> SRString {
    // ... validation, metadata parsing same as importPhoto ...

    PHPhotoLibrary.shared().performChanges({
        let req = PHAssetCreationRequest.forAsset()
        req.addResource(with: .photo, fileURL: photoURL, options: nil)
        req.addResource(with: .pairedVideo, fileURL: videoURL, options: nil)

        // Metadata — same API, PHAssetCreationRequest inherits from PHAssetChangeRequest
        req.creationDate = date
        req.location = location
        req.isFavorite = favorite

        localIdentifier = req.placeholderForCreatedAsset?.localIdentifier
    })

    // Return same ImportResult JSON shape
}
```

No changes to existing `importPhoto` — it continues handling standalone photos/videos.

### Rust-side: new FFI wrapper

**File:** `src/importer.rs`

```rust
swift!(fn photoferry_import_live_photo(
    photo_path: &SRString, video_path: &SRString, metadata_json: &SRString
) -> SRString);

pub fn import_live_photo(
    photo_path: &str, video_path: &str, metadata: Option<&PhotoMetadata>
) -> Result<ImportResult> { ... }
```

### Import loop change

**File:** `src/main.rs` — inside `import_inventory()`:

```rust
let result = if let Some(ref video_path) = file.live_photo_pair {
    let video_str = video_path.to_str().context("Invalid video path")?;
    importer::import_live_photo(path_str, video_str, file.metadata.as_ref())
} else {
    importer::import_photo(path_str, file.metadata.as_ref())
};
```

### Content identifier gotcha

Live Photos require matching content identifiers in both files (Apple MakerNote key 17 in HEIC, `com.apple.quicktime.content.identifier` in MOV). **Google Takeout preserves original file bytes**, so iPhone-shot Live Photos should already have these embedded.

If the identifiers are missing (non-Apple cameras, edited files), the import succeeds but Photos won't recognize the asset as a Live Photo — it appears as a regular photo with an unused video resource. This is acceptable degradation for Phase 3b. A future phase could optionally embed identifiers using CGImageDestination + AVAssetWriter.

### Acceptance criteria

- [ ] `IMG_1234.HEIC` + `IMG_1234.MOV` imports as a single Live Photo asset in Photos.app
- [ ] Metadata (date, GPS, favorite) applied to the Live Photo
- [ ] Standalone photos/videos continue importing via the existing path
- [ ] Files without matching content identifiers still import (graceful degradation)

## Feature 2: Album Creation + Assignment

### Problem

`MediaFile.album` is populated during scanning (e.g., `Some("Vacation 2024")`), `TakeoutInventory.albums` lists all detected albums, but the import loop ignores both. All photos land in the default library with no album structure.

### Implementation

**File:** `src/main.rs` — modify `import_inventory()`

**Pre-loop: create albums**

```rust
let mut album_ids: HashMap<String, String> = HashMap::new();
for album_name in &inventory.albums {
    match importer::create_album(album_name) {
        Ok(id) => { album_ids.insert(album_name.clone(), id); }
        Err(e) => { display::print_warning(&format!("Album '{}': {}", album_name, e)); }
    }
}
```

**Post-import: assign to album**

Inside the success branch of the import loop, after recording the `ImportedFile`:

```rust
if let Some(ref album_name) = file.album {
    if let Some(album_id) = album_ids.get(album_name) {
        if let Err(e) = importer::add_to_album(album_id, &local_id) {
            display::print_warning(&format!("Album assign failed: {}", e));
        }
    }
}
```

Album failures are warnings, not fatal errors — the photo is already imported.

### Edge case: duplicate album names

`create_album` always creates a new album. If the user runs photoferry twice, they get duplicate albums. This is fine for now — dedup is a future concern (could check existing albums via `PHAssetCollection.fetchAssetCollections(with:subtype:options:)` but that needs a new Swift FFI function).

### Acceptance criteria

- [ ] Albums detected in Takeout are created in Photos.app
- [ ] Photos in album directories are assigned to the correct album after import
- [ ] Album creation failure does not abort the run
- [ ] Photos in `Photos from YYYY/` directories are NOT assigned to any album

## Feature 3: Resume Manifest

### Problem

If a 10,000-photo import is interrupted at photo 5,000, re-running imports all 10,000 again — including 5,000 duplicates in Photos.app (PhotoKit doesn't dedup).

### Implementation

**New file:** `src/manifest.rs`

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
    path: String,       // relative path within extraction
    local_id: String,   // Photos.app localIdentifier
}

#[derive(Serialize, Deserialize)]
struct ManifestFailure {
    path: String,
    error: String,
}
```

**Manifest file location:** alongside the ZIP:

```
~/Downloads/.photoferry-manifest-takeout-20240101.json
```

### Read manifest before import

In `cmd_run`, after scanning but before `import_inventory()`:

```rust
let manifest = manifest::read_manifest(&manifest_path);
let already_imported: HashSet<String> = manifest
    .map(|m| m.imported.iter().map(|e| e.path.clone()).collect())
    .unwrap_or_default();
```

Filter `inventory.files` to exclude already-imported paths. Report skip count.

### Write manifest after import

After `import_inventory()` returns (before cleanup):

```rust
manifest::write_manifest(&manifest_path, &zip_name, &summary)?;
```

Appends to existing manifest if present (merges imported + failed lists).

### Acceptance criteria

- [ ] JSON manifest written per-ZIP after import completes
- [ ] Re-running skips files listed in manifest as successfully imported
- [ ] Skipped count shown in progress output
- [ ] Manifest survives between runs (not inside extraction dir)
- [ ] Failed files are re-attempted on next run (only imported files are skipped)

## Technical Considerations

**Import ordering:** Create albums first, import files, assign to albums. This is the natural order since `add_to_album` needs both the album ID and the asset's local identifier.

**Performance:** Album creation is fast (one API call per album, typically <20 albums). `add_to_album` adds ~50ms per call. For 10K photos across 15 albums, album assignment adds ~8 minutes — acceptable but worth noting.

**Manifest atomicity:** Write manifest to a `.tmp` file, then rename. Prevents corrupt manifests from interrupted writes.

## Files to Modify

- `src/main.rs` — `ImportSummary` struct, `import_inventory()` loop, `cmd_run()` for manifest read/write
- `src/importer.rs` — add `import_live_photo()` FFI wrapper
- `swift/Sources/PhotoFerrySwift.swift` — add `importLivePhoto` function

## Files to Create

- `src/manifest.rs` — manifest read/write/merge

## Verification

1. `cargo build` succeeds
2. Unit tests for manifest read/write/merge
3. Import a known Live Photo pair (HEIC + MOV) — verify it shows as Live Photo in Photos.app
4. Import files with album metadata — verify albums created and photos assigned
5. Interrupt a run (Ctrl-C mid-import), re-run — verify already-imported files skipped
6. All 39 existing tests still pass

## Sources

- `swift/Sources/PhotoFerrySwift.swift:87-186` — current import implementation
- `src/importer.rs:75-93` — album FFI (already implemented)
- `src/main.rs:283-389` — current import loop
- `src/takeout.rs:22-29` — MediaFile struct with album + live_photo_pair
- Apple PHAssetCreationRequest docs — `addResource(with: .photo)` + `.pairedVideo`
- Content identifier requirement: Apple MakerNote key 17 + QuickTime `content.identifier`
- LimitPoint/LivePhoto reference implementation for identifier embedding
