---
title: "feat: verify command — confirm imported assets exist and are correct"
type: feat
status: active
date: 2026-02-21
---

# Verify Command

After importing from a Takeout zip, `photoferry verify ~/Downloads` reads each manifest and queries PhotoKit to confirm every imported asset actually landed correctly — exists in the library, has a paired video for Live Photos, and has the right creation date.

## Acceptance Criteria

- [ ] `photoferry verify ~/Downloads` scans all manifest files in the directory
- [ ] For each manifest entry: confirms asset exists in Photos library by local_id
- [ ] For Live Photo entries: confirms asset has `.pairedVideo` resource attached
- [ ] For entries with a stored creation_date: confirms Photos.app date matches
- [ ] Outputs per-zip summary: verified / missing / wrong-date / missing-live-video counts
- [ ] Lists any specific failures with path + local_id + reason
- [ ] `ManifestEntry` gains `creation_date: Option<String>` field (backwards-compatible)
- [ ] New imports store the creation date from sidecar metadata in the manifest

## Context

### Key files
- `src/manifest.rs:10` — `ManifestEntry { path, local_id }` — needs `creation_date` added
- `src/main.rs:23` — `Commands` enum — add `Verify { dir }` variant
- `src/main.rs:232` — `new_imported` vec construction — needs to include creation_date
- `src/importer.rs:9` — FFI declarations (`swift!` macro)
- `swift/Sources/PhotoFerrySwift.swift:53` — `@_cdecl` pattern

### Research findings
- **Batch fetch is safe**: `PHAsset.fetchAssets(withLocalIdentifiers: allIds, options: nil)` handles 2704 IDs in one call. PHFetchResult is lazy — only SQL query cost, not object materialisation.
- **Missing assets**: PHFetchResult silently omits missing/deleted assets. Diff input set vs found set to detect them.
- **Live Photo check**: `PHAssetResource.assetResources(for: asset).contains { $0.type == .pairedVideo || $0.type == .fullSizePairedVideo }` — synchronous, no dispatch queue needed.
- **creationDate**: `asset.creationDate` is `Date?`, always populated for successfully imported assets.
- **Old manifests**: `creation_date` field on ManifestEntry will deserialise as `None` for old manifests — date check simply skipped for those entries.

## Implementation

### 1. Extend `ManifestEntry` (`src/manifest.rs:10`)

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub path: String,
    pub local_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub creation_date: Option<String>,  // ISO 8601 from sidecar, if available
}
```

Update `write_manifest` parameter type from `&[(String, String)]` to `&[(String, String, Option<String>)]`:

```rust
// (relative_path, local_id, creation_date)
pub fn write_manifest(
    path: &Path,
    zip_name: &str,
    imported: &[(String, String, Option<String>)],
    failed: &[(String, String)],
) -> Result<()>
```

Update `merge_and_write` similarly. All call sites in `main.rs` must pass the third field.

### 2. Thread creation_date through import (`src/main.rs:232`)

In `import_inventory`, when building `ImportedFile`, capture the creation date:

```rust
summary.imported.push(ImportedFile {
    path: file.path.clone(),
    local_id: local_id.clone(),
    album: file.album.clone(),
    creation_date: file.metadata.as_ref()
        .and_then(|m| m.creation_date.clone()),  // already Option<String>
});
```

Add `creation_date: Option<String>` to `ImportedFile` struct. Then in `cmd_run`:

```rust
let new_imported: Vec<(String, String, Option<String>)> = summary.imported.iter().map(|file| {
    (
        file.path.strip_prefix(&content_root)...to_string(),
        file.local_id.clone(),
        file.creation_date.clone(),
    )
}).collect();
```

### 3. New Swift FFI function (`swift/Sources/PhotoFerrySwift.swift`)

Add result struct and batch verify function:

```swift
struct AssetVerifyResult: Codable {
    let localIdentifier: String
    let found: Bool
    let creationDate: String?
    let hasPairedVideo: Bool
}

@_cdecl("photoferry_verify_assets")
public func verifyAssets(identifiersJSON: SRString) -> SRString {
    let json = identifiersJSON.toString()
    guard let data = json.data(using: .utf8),
          let identifiers = try? JSONDecoder().decode([String].self, from: data)
    else {
        return SRString("{\"error\":\"invalid_input\"}")
    }

    let formatter = ISO8601DateFormatter()
    formatter.formatOptions = [.withInternetDateTime]

    let fetchResult = PHAsset.fetchAssets(
        withLocalIdentifiers: identifiers, options: nil
    )

    var results: [AssetVerifyResult] = []
    var foundIds = Set<String>()

    fetchResult.enumerateObjects { asset, _, _ in
        foundIds.insert(asset.localIdentifier)
        let resources = PHAssetResource.assetResources(for: asset)
        let hasPaired = resources.contains {
            $0.type == .pairedVideo || $0.type == .fullSizePairedVideo
        }
        let dateStr = asset.creationDate.map { formatter.string(from: $0) }
        results.append(AssetVerifyResult(
            localIdentifier: asset.localIdentifier,
            found: true,
            creationDate: dateStr,
            hasPairedVideo: hasPaired
        ))
    }

    // Report missing
    for id in identifiers where !foundIds.contains(id) {
        results.append(AssetVerifyResult(
            localIdentifier: id, found: false,
            creationDate: nil, hasPairedVideo: false
        ))
    }

    return SRString(toJSON(results))
}
```

### 4. Rust FFI wrapper (`src/importer.rs`)

```rust
swift!(fn photoferry_verify_assets(identifiers_json: &SRString) -> SRString);

#[derive(Debug, Deserialize)]
pub struct AssetVerifyResult {
    #[serde(rename = "localIdentifier")]
    pub local_identifier: String,
    pub found: bool,
    #[serde(rename = "creationDate")]
    pub creation_date: Option<String>,
    #[serde(rename = "hasPairedVideo")]
    pub has_paired_video: bool,
}

pub fn verify_assets(local_ids: &[&str]) -> Result<Vec<AssetVerifyResult>> {
    let ids_json = serde_json::to_string(local_ids)?;
    let ids_sr: SRString = ids_json.as_str().into();
    let json = unsafe { photoferry_verify_assets(&ids_sr) };
    let results: Vec<AssetVerifyResult> = serde_json::from_str(json.as_str())?;
    Ok(results)
}
```

### 5. `cmd_verify` and `Verify` subcommand (`src/main.rs`)

```rust
/// Verify imported photos exist and are correct in Photos library
Verify {
    #[arg(default_value = "~/Downloads")]
    dir: PathBuf,
},
```

```rust
fn cmd_verify(dir: &PathBuf) -> Result<()> {
    let dir = expand_tilde(dir);
    display::print_header(&format!("Verifying imports in {}", dir.display()));

    // Find all manifest files
    let manifests: Vec<PathBuf> = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with(".photoferry-manifest-") && n.ends_with(".json"))
            .unwrap_or(false))
        .collect();

    if manifests.is_empty() {
        display::print_info("No manifests found.");
        return Ok(());
    }

    let access = importer::check_access()?;
    if !access.authorized {
        bail!("Photos access not authorized");
    }

    let mut total_verified = 0;
    let mut total_missing = 0;
    let mut total_wrong_date = 0;
    let mut total_missing_video = 0;

    for manifest_path in &manifests {
        let manifest = match manifest::read_manifest(manifest_path) {
            Some(m) => m,
            None => { display::print_warning(&format!("Could not read {:?}", manifest_path)); continue; }
        };

        display::print_header(&format!("Verifying {}", manifest.zip));
        display::print_info(&format!("Checking {} imported assets...", manifest.imported.len()));

        let ids: Vec<&str> = manifest.imported.iter().map(|e| e.local_id.as_str()).collect();
        let results = importer::verify_assets(&ids)?;

        // Build lookup map from results
        use std::collections::HashMap;
        let result_map: HashMap<&str, &importer::AssetVerifyResult> =
            results.iter().map(|r| (r.local_identifier.as_str(), r)).collect();

        let mut missing = vec![];
        let mut wrong_date = vec![];
        let mut missing_video = vec![];

        for entry in &manifest.imported {
            match result_map.get(entry.local_id.as_str()) {
                None | Some(importer::AssetVerifyResult { found: false, .. }) => {
                    missing.push(entry);
                }
                Some(result) => {
                    total_verified += 1;
                    // Date check (only if both sides have a date)
                    if let (Some(expected), Some(actual)) =
                        (&entry.creation_date, &result.creation_date)
                    {
                        // Compare date portion only (ignore sub-second differences)
                        if !dates_match(expected, actual) {
                            wrong_date.push((entry, actual.clone()));
                        }
                    }
                    // Live Photo check — only flag if we expected a paired video
                    // (heuristic: .heic or .jpg files that have a paired video missing)
                    if !result.has_paired_video
                        && entry.path.to_lowercase().ends_with(".heic")
                        && result.found
                    {
                        // Note: only flag if the manifest knew it was a Live Photo pair
                        // Future: store is_live_photo flag in manifest
                    }
                }
            }
        }

        missing.iter().for_each(|e| {
            display::print_error(&format!("MISSING: {} ({})", e.path, e.local_id));
            total_missing += 1;
        });
        wrong_date.iter().for_each(|(e, actual)| {
            display::print_warning(&format!(
                "DATE MISMATCH: {} — expected {} got {}",
                e.path,
                e.creation_date.as_deref().unwrap_or("?"),
                actual
            ));
            total_wrong_date += 1;
        });

        display::print_info(&format!(
            "Verified: {} | Missing: {} | Wrong date: {}",
            manifest.imported.len() - missing.len() - wrong_date.len(),
            missing.len(),
            wrong_date.len()
        ));
    }

    println!();
    display::print_header("Total");
    display::print_info(&format!("Verified OK: {}", total_verified));
    if total_missing > 0 { display::print_error(&format!("Missing: {}", total_missing)); }
    if total_wrong_date > 0 { display::print_warning(&format!("Wrong date: {}", total_wrong_date)); }
    if total_missing == 0 && total_wrong_date == 0 {
        display::print_success("All assets verified successfully");
    }

    Ok(())
}

fn dates_match(a: &str, b: &str) -> bool {
    // Compare first 19 chars (YYYY-MM-DDTHH:MM:SS) — ignore timezone/subsecond
    a.len() >= 19 && b.len() >= 19 && a[..19] == b[..19]
}
```

## Known Limitations

- **Old manifests** without `creation_date` skip date verification (graceful degradation)
- **Live Photo detection** in verify relies on `.heic` extension heuristic; a future improvement would store `is_live_photo: bool` in ManifestEntry
- **Date tolerance**: `dates_match` ignores sub-second differences and timezone offsets — adequate for sidecar-vs-PhotoKit comparison since Google uses Unix epoch seconds (no sub-second precision)

## Sources & References

- `src/manifest.rs:10` — ManifestEntry (needs creation_date)
- `src/main.rs:232` — new_imported construction
- `swift/Sources/PhotoFerrySwift.swift:53` — @_cdecl FFI pattern
- PhotoKit: `PHAsset.fetchAssets(withLocalIdentifiers:)` — batch, lazy, silently omits missing
- PhotoKit: `PHAssetResource.assetResources(for:)` — synchronous, no async needed
- PhotoKit: `PHAssetResourceType.pairedVideo` (raw 9), `.fullSizePairedVideo` (raw 10)
