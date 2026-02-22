use std::collections::HashSet;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub path: String,
    pub local_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub creation_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_live_photo: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestFailure {
    pub path: String,
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportManifest {
    pub zip: String,
    pub processed_at: String,
    pub imported: Vec<ManifestEntry>,
    pub failed: Vec<ManifestFailure>,
}

/// Read an existing manifest file leniently. Returns None on any error.
#[cfg(test)]
pub fn read_manifest(path: &Path) -> Option<ImportManifest> {
    let contents = fs::read_to_string(path).ok()?;
    serde_json::from_str(&contents).ok()
}

/// Read an existing manifest file strictly.
/// Returns Ok(None) when missing, and Err when unreadable/corrupt.
pub fn read_manifest_strict(path: &Path) -> Result<Option<ImportManifest>> {
    let contents = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("Failed to read {}", path.display())),
    };
    let manifest = serde_json::from_str::<ImportManifest>(&contents)
        .with_context(|| format!("Corrupt manifest JSON at {}", path.display()))?;
    Ok(Some(manifest))
}

/// Build a set of already-imported paths from a manifest.
#[cfg(test)]
pub fn already_imported(manifest: &ImportManifest) -> HashSet<String> {
    manifest.imported.iter().map(|e| e.path.clone()).collect()
}

/// Write a manifest to disk. Uses write-to-tmp-then-rename for atomicity.
pub fn write_manifest(
    path: &Path,
    zip_name: &str,
    imported: &[(String, String, Option<String>, bool)], // (relative_path, local_id, creation_date, is_live_photo)
    failed: &[(String, String)],                         // (relative_path, error)
) -> Result<()> {
    let manifest = ImportManifest {
        zip: zip_name.to_string(),
        processed_at: Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        imported: imported
            .iter()
            .map(|(p, id, date, is_live_photo)| ManifestEntry {
                path: p.clone(),
                local_id: id.clone(),
                creation_date: date.clone(),
                is_live_photo: Some(*is_live_photo),
            })
            .collect(),
        failed: failed
            .iter()
            .map(|(p, e)| ManifestFailure {
                path: p.clone(),
                error: e.clone(),
            })
            .collect(),
    };

    let json = serde_json::to_string_pretty(&manifest)?;
    let tmp_path = path.with_extension("json.tmp");
    fs::write(&tmp_path, json)?;
    fs::rename(&tmp_path, path)?;
    Ok(())
}

/// Merge new results into an existing manifest (appends to imported/failed lists).
/// Previously-failed entries that succeeded this time are removed from failed.
pub fn merge_and_write(
    path: &Path,
    zip_name: &str,
    new_imported: &[(String, String, Option<String>, bool)],
    new_failed: &[(String, String)],
) -> Result<()> {
    let mut imported: Vec<(String, String, Option<String>, bool)> = Vec::new();
    let mut failed: Vec<(String, String)> = Vec::new();

    if let Some(existing) = read_manifest_strict(path)? {
        imported.extend(existing.imported.into_iter().map(|e| {
            (
                e.path,
                e.local_id,
                e.creation_date,
                e.is_live_photo.unwrap_or(false),
            )
        }));
        failed.extend(existing.failed.into_iter().map(|e| (e.path, e.error)));
    }

    // Remove old failures that succeeded on retry
    let newly_imported_paths: HashSet<&str> =
        new_imported.iter().map(|(p, _, _, _)| p.as_str()).collect();
    failed.retain(|(p, _)| !newly_imported_paths.contains(p.as_str()));

    imported.extend_from_slice(new_imported);
    let mut seen = std::collections::HashSet::new();
    let mut deduped = Vec::new();
    for entry in imported.into_iter().rev() {
        if seen.insert(entry.0.clone()) {
            deduped.push(entry);
        }
    }
    deduped.reverse();
    let imported = deduped;
    failed.extend_from_slice(new_failed);

    write_manifest(path, zip_name, &imported, &failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_nonexistent() {
        assert!(read_manifest(Path::new("/nonexistent/manifest.json")).is_none());
        assert!(read_manifest_strict(Path::new("/nonexistent/manifest.json"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_write_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.json");

        let imported = vec![
            ("photo.jpg".to_string(), "ABC123".to_string(), None, false),
            ("sunset.png".to_string(), "DEF456".to_string(), None, false),
        ];
        let failed = vec![("corrupt.jpg".to_string(), "bad data".to_string())];

        write_manifest(&path, "takeout-20240101.zip", &imported, &failed).unwrap();

        let manifest = read_manifest(&path).unwrap();
        assert_eq!(manifest.zip, "takeout-20240101.zip");
        assert_eq!(manifest.imported.len(), 2);
        assert_eq!(manifest.failed.len(), 1);
        assert_eq!(manifest.imported[0].path, "photo.jpg");
        assert_eq!(manifest.imported[0].local_id, "ABC123");
        assert_eq!(manifest.imported[0].is_live_photo, Some(false));
    }

    #[test]
    fn test_merge_removes_retried_failures() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.json");

        let failed = vec![("retry.jpg".to_string(), "timeout".to_string())];
        write_manifest(&path, "test.zip", &[], &failed).unwrap();

        let new_imported = vec![("retry.jpg".to_string(), "XYZ789".to_string(), None, false)];
        merge_and_write(&path, "test.zip", &new_imported, &[]).unwrap();

        let manifest = read_manifest(&path).unwrap();
        assert_eq!(manifest.imported.len(), 1);
        assert_eq!(manifest.failed.len(), 0);
    }

    #[test]
    fn test_read_manifest_strict_errors_on_corrupt_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.json");
        std::fs::write(&path, "{not-json").unwrap();

        assert!(read_manifest(&path).is_none());
        assert!(read_manifest_strict(&path).is_err());
    }

    #[test]
    fn test_already_imported_set() {
        let manifest = ImportManifest {
            zip: "test.zip".to_string(),
            processed_at: "2026-01-01T00:00:00Z".to_string(),
            imported: vec![
                ManifestEntry {
                    path: "a.jpg".to_string(),
                    local_id: "1".to_string(),
                    creation_date: None,
                    is_live_photo: None,
                },
                ManifestEntry {
                    path: "b.jpg".to_string(),
                    local_id: "2".to_string(),
                    creation_date: None,
                    is_live_photo: None,
                },
                ManifestEntry {
                    path: "c.jpg".to_string(),
                    local_id: "3".to_string(),
                    creation_date: None,
                    is_live_photo: Some(false),
                },
            ],
            failed: vec![],
        };

        let set = already_imported(&manifest);
        assert!(set.contains("a.jpg"));
        assert!(set.contains("b.jpg"));
        assert!(set.contains("c.jpg"));
        assert!(!set.contains("d.jpg"));
    }
}
