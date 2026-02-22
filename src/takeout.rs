use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use walkdir::WalkDir;

use crate::importer::PhotoMetadata;
use crate::metadata;
use crate::sidecar;

// MARK: - Types

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaType {
    Photo,
    Video,
}

#[derive(Debug, Clone)]
pub struct MediaFile {
    pub path: PathBuf,
    #[allow(dead_code)]
    pub media_type: MediaType,
    #[allow(dead_code)]
    pub sidecar: Option<PathBuf>,
    pub metadata: Option<PhotoMetadata>,
    pub album: Option<String>,
    pub live_photo_pair: Option<PathBuf>,
}

#[derive(Debug)]
pub struct TakeoutInventory {
    pub files: Vec<MediaFile>,
    pub albums: Vec<String>,
    pub stats: InventoryStats,
}

#[derive(Debug, Default)]
pub struct InventoryStats {
    pub photos: usize,
    pub videos: usize,
    pub with_sidecar: usize,
    pub without_sidecar: usize,
    pub trashed_skipped: usize,
    pub live_photo_pairs: usize,
}

// MARK: - Extension sets

const PHOTO_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "heic", "heif", "webp", "tiff", "tif", "bmp", "raw", "cr2", "nef",
    "arw", "dng",
];

const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "mov", "avi", "m4v", "3gp", "mkv", "mpg", "mpeg", "wmv", "flv", "webm",
];

fn classify_extension(ext: &str) -> Option<MediaType> {
    let ext_lower = ext.to_ascii_lowercase();
    if PHOTO_EXTENSIONS.contains(&ext_lower.as_str()) {
        Some(MediaType::Photo)
    } else if VIDEO_EXTENSIONS.contains(&ext_lower.as_str()) {
        Some(MediaType::Video)
    } else {
        None
    }
}

// MARK: - ZIP discovery

/// Find Takeout ZIP files in a directory.
pub fn find_takeout_zips(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut zips = Vec::new();

    let entries =
        fs::read_dir(dir).with_context(|| format!("Cannot read directory: {}", dir.display()))?;

    for entry in entries {
        let entry = entry?;
        let path = entry.path();

        if !path.is_file() {
            continue;
        }

        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };

        // Skip in-progress Chrome downloads
        if name.ends_with(".crdownload") {
            continue;
        }

        let name_lower = name.to_ascii_lowercase();
        if name_lower.ends_with(".zip")
            && (name_lower.starts_with("takeout-")
                || name_lower.starts_with("takeout ")
                || name_lower.contains("-takeout-"))
        {
            zips.push(path);
        }
    }

    zips.sort();
    Ok(zips)
}

// MARK: - ZIP extraction

/// Extract a Takeout ZIP to a destination directory. Returns the content root
/// (handles the `Takeout/` wrapper subfolder Google adds).
pub fn extract_zip(zip_path: &Path, dest: &Path) -> Result<PathBuf> {
    let file = fs::File::open(zip_path)
        .with_context(|| format!("Cannot open ZIP: {}", zip_path.display()))?;
    let reader = BufReader::new(file);
    let mut archive = zip::ZipArchive::new(reader)
        .with_context(|| format!("Invalid ZIP: {}", zip_path.display()))?;

    archive
        .extract(dest)
        .with_context(|| format!("Failed to extract ZIP: {}", zip_path.display()))?;

    // Google Takeout wraps everything in a `Takeout/` subfolder
    let takeout_dir = dest.join("Takeout");
    if takeout_dir.is_dir() {
        Ok(takeout_dir)
    } else {
        Ok(dest.to_path_buf())
    }
}

// MARK: - Directory scanning

/// Scan an extracted Takeout directory and build an inventory of media files.
pub fn scan_directory(root: &Path) -> Result<TakeoutInventory> {
    let mut stats = InventoryStats::default();
    let mut files = Vec::new();
    let mut albums = Vec::new();
    let mut seen_albums = HashSet::new();

    // Group files by directory for efficient sidecar matching
    let dir_contents = collect_directory_contents(root)?;

    for (dir_path, entries) in &dir_contents {
        let album = detect_album(dir_path, &entries.json_files);
        let is_year_folder = is_year_folder(dir_path);

        if let Some(ref album_name) = album
            && !is_year_folder
            && seen_albums.insert(album_name.clone())
        {
            albums.push(album_name.clone());
        }

        // Build JSON candidates for this directory
        let json_candidates = sidecar::collect_json_candidates(&entries.all_files);

        // Detect Live Photo pairs in this directory
        let live_pairs = detect_live_photo_pairs(&entries.media_files);

        for media_path in &entries.media_files {
            let ext = media_path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("");
            let Some(media_type) = classify_extension(ext) else {
                continue;
            };

            // Skip videos that are Live Photo pairs (they'll be attached to the photo)
            if media_type == MediaType::Video && live_pairs.values().any(|v| v == media_path) {
                continue;
            }

            // Find sidecar and parse metadata
            let sidecar_path = sidecar::find_sidecar(media_path, &json_candidates);
            let takeout_meta = sidecar_path.as_ref().and_then(|sp| {
                let bytes = fs::read(sp).ok()?;
                metadata::parse_sidecar(&bytes).ok()
            });

            // Skip trashed files
            if takeout_meta.as_ref().is_some_and(|m| m.is_trashed()) {
                stats.trashed_skipped += 1;
                continue;
            }

            let photo_metadata = takeout_meta.as_ref().map(|m| m.to_photo_metadata());

            // Track stats
            match media_type {
                MediaType::Photo => stats.photos += 1,
                MediaType::Video => stats.videos += 1,
            }
            if sidecar_path.is_some() {
                stats.with_sidecar += 1;
            } else {
                stats.without_sidecar += 1;
            }

            let live_photo_pair = if media_type == MediaType::Photo {
                live_pairs.get(media_path).cloned()
            } else {
                None
            };
            if live_photo_pair.is_some() {
                stats.live_photo_pairs += 1;
            }

            let effective_album = if is_year_folder { None } else { album.clone() };

            files.push(MediaFile {
                path: media_path.clone(),
                media_type,
                sidecar: sidecar_path,
                metadata: photo_metadata,
                album: effective_album,
                live_photo_pair,
            });
        }
    }

    albums.sort();

    Ok(TakeoutInventory {
        files,
        albums,
        stats,
    })
}

// MARK: - Directory content collection

struct DirectoryEntries {
    all_files: Vec<PathBuf>,
    media_files: Vec<PathBuf>,
    json_files: Vec<PathBuf>,
}

/// Walk the directory tree and group files by their parent directory.
fn collect_directory_contents(root: &Path) -> Result<HashMap<PathBuf, DirectoryEntries>> {
    let mut dirs: HashMap<PathBuf, DirectoryEntries> = HashMap::new();

    for entry in WalkDir::new(root).follow_links(true) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }

        let path = entry.into_path();
        let parent = path.parent().unwrap_or(root).to_path_buf();
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();

        let dir_entry = dirs.entry(parent).or_insert_with(|| DirectoryEntries {
            all_files: Vec::new(),
            media_files: Vec::new(),
            json_files: Vec::new(),
        });

        dir_entry.all_files.push(path.clone());

        if ext == "json" {
            dir_entry.json_files.push(path);
        } else if classify_extension(&ext).is_some() {
            dir_entry.media_files.push(path);
        }
    }

    Ok(dirs)
}

// MARK: - Album detection

/// Check if a directory is an album folder by looking for a `metadata.json` with album data.
fn detect_album(_dir: &Path, json_files: &[PathBuf]) -> Option<String> {
    // First check: directory-level metadata.json
    let metadata_path = json_files
        .iter()
        .find(|p| p.file_name().and_then(|n| n.to_str()) == Some("metadata.json"))?;

    let bytes = fs::read(metadata_path).ok()?;
    let parsed: metadata::TakeoutJson = serde_json::from_slice(&bytes).ok()?;
    parsed.album_data.map(|a| a.title)
}

/// Check if directory name matches `Photos from YYYY` pattern — these aren't albums.
fn is_year_folder(dir: &Path) -> bool {
    let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if let Some(rest) = name.strip_prefix("Photos from ") {
        rest.len() == 4 && rest.chars().all(|c| c.is_ascii_digit())
    } else {
        false
    }
}

// MARK: - Live Photo pair detection

/// Match photo + video pairs in the same directory by base filename.
/// Returns a map from photo path → video path.
fn detect_live_photo_pairs(media_files: &[PathBuf]) -> HashMap<PathBuf, PathBuf> {
    let mut pairs = HashMap::new();
    let mut by_stem: HashMap<String, Vec<&PathBuf>> = HashMap::new();

    for path in media_files {
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            by_stem
                .entry(stem.to_ascii_uppercase())
                .or_default()
                .push(path);
        }
    }

    for files in by_stem.values() {
        if files.len() != 2 {
            continue;
        }

        let (mut photo, mut video) = (None, None);
        for f in files {
            let ext = f.extension().and_then(|e| e.to_str()).unwrap_or("");
            match classify_extension(ext) {
                Some(MediaType::Photo) => photo = Some((*f).clone()),
                Some(MediaType::Video) => video = Some((*f).clone()),
                None => {}
            }
        }

        if let (Some(p), Some(v)) = (photo, video) {
            pairs.insert(p, v);
        }
    }

    pairs
}

// MARK: - Tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn setup_test_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn test_classify_extension() {
        assert_eq!(classify_extension("jpg"), Some(MediaType::Photo));
        assert_eq!(classify_extension("JPEG"), Some(MediaType::Photo));
        assert_eq!(classify_extension("mp4"), Some(MediaType::Video));
        assert_eq!(classify_extension("MOV"), Some(MediaType::Video));
        assert_eq!(classify_extension("json"), None);
        assert_eq!(classify_extension("txt"), None);
    }

    #[test]
    fn test_is_year_folder() {
        assert!(is_year_folder(Path::new("/tmp/Takeout/Photos from 2024")));
        assert!(is_year_folder(Path::new("Photos from 2020")));
        assert!(!is_year_folder(Path::new("Vacation 2024")));
        assert!(!is_year_folder(Path::new("Photos from January")));
    }

    #[test]
    fn test_find_takeout_zips() {
        let dir = setup_test_dir();
        let base = dir.path();

        // Create test files
        fs::write(base.join("takeout-20240101.zip"), b"PK\x03\x04").unwrap();
        fs::write(base.join("Takeout-20240102.zip"), b"PK\x03\x04").unwrap();
        fs::write(base.join("random.zip"), b"PK\x03\x04").unwrap();
        fs::write(base.join("takeout-partial.zip.crdownload"), b"").unwrap();

        let zips = find_takeout_zips(base).unwrap();
        assert_eq!(zips.len(), 2);
        let first_name = zips[0]
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_ascii_lowercase();
        assert!(first_name.contains("takeout"));
    }

    #[test]
    fn test_find_takeout_zips_empty_dir() {
        let dir = setup_test_dir();
        let zips = find_takeout_zips(dir.path()).unwrap();
        assert!(zips.is_empty());
    }

    #[test]
    fn test_live_photo_pair_detection() {
        let files = vec![
            PathBuf::from("/photos/IMG_1234.HEIC"),
            PathBuf::from("/photos/IMG_1234.MOV"),
            PathBuf::from("/photos/IMG_5678.JPG"),
        ];
        let pairs = detect_live_photo_pairs(&files);
        assert_eq!(pairs.len(), 1);
        assert_eq!(
            pairs.get(&PathBuf::from("/photos/IMG_1234.HEIC")),
            Some(&PathBuf::from("/photos/IMG_1234.MOV"))
        );
    }

    #[test]
    fn test_live_photo_no_pair() {
        let files = vec![
            PathBuf::from("/photos/IMG_1234.HEIC"),
            PathBuf::from("/photos/IMG_5678.MOV"),
        ];
        let pairs = detect_live_photo_pairs(&files);
        assert!(pairs.is_empty());
    }

    #[test]
    fn test_scan_mock_takeout() {
        let dir = setup_test_dir();
        let base = dir.path();

        // Create directory structure mimicking Google Takeout
        let year_dir = base.join("Photos from 2024");
        fs::create_dir_all(&year_dir).unwrap();

        // Media file
        fs::write(year_dir.join("sunset.jpg"), b"fake jpg data").unwrap();

        // Sidecar JSON
        let sidecar = r#"{
            "title": "sunset.jpg",
            "photoTakenTime": { "timestamp": "1700000000" },
            "geoDataExif": { "latitude": 22.3, "longitude": 114.2, "altitude": 50.0 },
            "favorited": true
        }"#;
        fs::write(year_dir.join("sunset.jpg.json"), sidecar).unwrap();

        // Album directory
        let album_dir = base.join("Vacation");
        fs::create_dir_all(&album_dir).unwrap();
        fs::write(album_dir.join("beach.png"), b"fake png data").unwrap();
        let album_meta = r#"{ "albumData": { "title": "Vacation" } }"#;
        fs::write(album_dir.join("metadata.json"), album_meta).unwrap();

        // Trashed file
        let trashed_sidecar = r#"{ "trashed": true }"#;
        fs::write(year_dir.join("deleted.jpg"), b"trash").unwrap();
        fs::write(year_dir.join("deleted.jpg.json"), trashed_sidecar).unwrap();

        let inventory = scan_directory(base).unwrap();

        assert_eq!(inventory.stats.photos, 2); // sunset + beach
        assert_eq!(inventory.stats.with_sidecar, 1); // sunset has sidecar
        assert_eq!(inventory.stats.without_sidecar, 1); // beach has no sidecar
        assert_eq!(inventory.stats.trashed_skipped, 1);
        assert_eq!(inventory.albums, vec!["Vacation"]);

        // Verify sunset metadata was parsed
        let sunset = inventory
            .files
            .iter()
            .find(|f| f.path.file_name().unwrap().to_str().unwrap() == "sunset.jpg")
            .unwrap();
        assert!(sunset.metadata.is_some());
        let meta = sunset.metadata.as_ref().unwrap();
        assert_eq!(meta.creation_date.as_deref(), Some("2023-11-14T22:13:20Z"));
        assert_eq!(meta.is_favorite, Some(true));
        assert!(sunset.album.is_none()); // year folder, not an album

        // Verify beach is in album
        let beach = inventory
            .files
            .iter()
            .find(|f| f.path.file_name().unwrap().to_str().unwrap() == "beach.png")
            .unwrap();
        assert_eq!(beach.album.as_deref(), Some("Vacation"));
    }
}
