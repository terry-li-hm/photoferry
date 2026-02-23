mod display;
mod downloader;
mod importer;
mod manifest;
mod metadata;
mod sidecar;
mod takeout;

use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use clap::{Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};
use std::collections::{HashMap, HashSet};
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

const STRICT_EXTENSIONS_ABORT: &str = "STRICT_EXTENSIONS_ABORT";

#[derive(Parser)]
#[command(
    name = "photoferry",
    version,
    about = "Google Photos → iCloud migration"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Check Photos.app access permission
    Check,
    /// Process Takeout zips and import photos
    Run {
        /// Source directory containing Takeout zips
        #[arg(default_value = "~/Downloads")]
        dir: PathBuf,
        /// Process one zip and exit
        #[arg(long)]
        once: bool,
        /// Simulate without importing
        #[arg(long)]
        dry_run: bool,
        /// Print per-file import results instead of progress bar
        #[arg(long)]
        verbose: bool,
        /// Include trashed items from Takeout
        #[arg(long)]
        include_trashed: bool,
        /// Retry only files that previously failed in manifest
        #[arg(long)]
        retry_failed: bool,
        /// Abort if any unknown file extensions are detected
        #[arg(long)]
        strict_extensions: bool,
        /// Write CSV report of unknown files to PATH
        #[arg(long)]
        unknown_report: Option<PathBuf>,
    },
    /// Import a single file (for testing)
    Import {
        /// Path to photo/video file
        file: PathBuf,
        /// JSON metadata string
        #[arg(long)]
        metadata: Option<String>,
    },
    /// List albums detected in Takeout zips
    Albums {
        /// Source directory containing Takeout zips
        #[arg(default_value = "~/Downloads")]
        dir: PathBuf,
    },
    /// Verify imported photos exist and are correct in Photos library
    Verify {
        /// Directory containing manifest files
        #[arg(default_value = "~/Downloads")]
        dir: PathBuf,
    },
    /// Re-import assets that verify as missing from Photos library
    RetryMissing {
        /// Directory containing manifests and Takeout zips
        #[arg(default_value = "~/Downloads")]
        dir: PathBuf,
        /// Print per-file import results
        #[arg(long)]
        verbose: bool,
    },
    /// Re-import Live Photo fallbacks (still-only) as Live Photos
    RetryLivePhotoFallbacks {
        /// Directory containing manifests and Takeout zips
        #[arg(default_value = "~/Downloads")]
        dir: PathBuf,
        /// Print per-file import results
        #[arg(long)]
        verbose: bool,
    },
    /// Download Takeout zips from Google, import, and delete
    Download {
        /// Google Takeout job ID
        #[arg(long)]
        job: String,
        /// Google user ID
        #[arg(long)]
        user: String,
        /// Download directory
        #[arg(long, default_value = "~/Downloads")]
        dir: PathBuf,
        /// First part index (default: 0)
        #[arg(long, default_value_t = 0)]
        start: usize,
        /// Last part index inclusive (default: 98 for 99 parts)
        #[arg(long, default_value_t = 98)]
        end: usize,
        /// Download only, skip import
        #[arg(long)]
        download_only: bool,
        /// Use Chrome browser for downloading (handles passkey/re-auth)
        #[arg(long)]
        chrome: bool,
        /// Print per-file import results
        #[arg(long)]
        verbose: bool,
        /// Include trashed items from Takeout
        #[arg(long)]
        include_trashed: bool,
        /// Abort if any unknown file extensions are detected
        #[arg(long)]
        strict_extensions: bool,
        /// Write CSV report of unknown files to PATH
        #[arg(long)]
        unknown_report: Option<PathBuf>,
        /// Confirm Photos.app shows iCloud upload queue is complete (for safe zip deletion)
        #[arg(long)]
        icloud_confirmed: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        None => {
            display::print_header("photoferry");
            display::print_info(&format!("v{}", env!("CARGO_PKG_VERSION")));
            display::print_info("Google Photos → iCloud migration");
            println!();
            display::print_info("Run 'photoferry --help' for usage");
        }
        Some(Commands::Check) => cmd_check()?,
        Some(Commands::Run {
            dir,
            once,
            dry_run,
            verbose,
            include_trashed,
            retry_failed,
            strict_extensions,
            unknown_report,
        }) => cmd_run(
            &dir,
            once,
            dry_run,
            verbose,
            include_trashed,
            retry_failed,
            strict_extensions,
            unknown_report.as_deref(),
        )?,
        Some(Commands::Import { file, metadata }) => cmd_import(&file, metadata.as_deref())?,
        Some(Commands::Albums { dir }) => cmd_albums(&dir)?,
        Some(Commands::Verify { dir }) => cmd_verify(&dir)?,
        Some(Commands::RetryMissing { dir, verbose }) => cmd_retry_missing(&dir, verbose)?,
        Some(Commands::RetryLivePhotoFallbacks { dir, verbose }) => {
            cmd_retry_live_photo_fallbacks(&dir, verbose)?
        }
        Some(Commands::Download {
            job,
            user,
            dir,
            start,
            end,
            download_only,
            chrome,
            verbose,
            include_trashed,
            strict_extensions,
            unknown_report,
            icloud_confirmed,
        }) => cmd_download(
            &job,
            &user,
            &dir,
            start,
            end,
            download_only,
            chrome,
            verbose,
            include_trashed,
            strict_extensions,
            unknown_report.as_deref(),
            icloud_confirmed,
        )?,
    }

    Ok(())
}

fn cmd_check() -> Result<()> {
    display::print_header("Checking Photos.app access...");
    let result = importer::check_access()?;

    if !result.authorized {
        display::print_error(&format!(
            "Photos access: {} — grant in System Settings > Privacy & Security > Photos",
            result.status
        ));
    } else if result.status == "limited" {
        display::print_warning(
            "Photos access: limited — grant full library access for reliable verify/retry",
        );
    } else {
        display::print_success(&format!("Photos access: {} (authorized)", result.status));
    }

    Ok(())
}

fn cmd_run(
    dir: &Path,
    once: bool,
    dry_run: bool,
    verbose: bool,
    include_trashed: bool,
    retry_failed: bool,
    strict_extensions: bool,
    unknown_report: Option<&Path>,
) -> Result<()> {
    let dir = expand_tilde(dir);
    if dry_run {
        display::print_header(&format!("Dry run — scanning {}", dir.display()));
    } else {
        display::print_header(&format!("Processing Takeout zips from {}", dir.display()));
    }

    let zips = takeout::find_takeout_zips(&dir)?;
    if zips.is_empty() {
        display::print_info("No Takeout zips found.");
        return Ok(());
    }

    display::print_info(&format!("Found {} zip(s)", zips.len()));

    let zips_to_process = if once { &zips[..1] } else { &zips };

    if !dry_run {
        let access = importer::check_access()?;
        ensure_full_photos_access(&access, "import")?;
        display::print_success(&format!("Photos access: {} (authorized)", access.status));
    }

    let mut total_summary = ImportSummary::default();

    for zip_path in zips_to_process {
        display::print_header(&format!(
            "Processing {}",
            zip_path.file_name().unwrap_or_default().to_string_lossy()
        ));
        match process_one_zip(
            zip_path,
            &dir,
            dry_run,
            verbose,
            include_trashed,
            retry_failed,
            strict_extensions,
            unknown_report,
        ) {
            Ok(summary) => {
                print_import_summary(&summary);
                total_summary.merge(&summary);
            }
            Err(e) => {
                let msg = e.to_string();
                if msg.starts_with(STRICT_EXTENSIONS_ABORT) {
                    let cleaned = msg
                        .strip_prefix(STRICT_EXTENSIONS_ABORT)
                        .unwrap_or(&msg)
                        .trim_start_matches(':')
                        .trim();
                    return Err(anyhow::anyhow!(cleaned.to_string()));
                }
                display::print_error(&format!(
                    "Skipping {} — {}",
                    zip_path.file_name().unwrap_or_default().to_string_lossy(),
                    e
                ));
            }
        }
    }

    // Print totals if multiple zips processed
    if !dry_run && zips_to_process.len() > 1 {
        println!();
        display::print_header("Total across all zips");
        print_import_summary(&total_summary);
    }

    Ok(())
}

/// Process a single Takeout zip. Streams directory-by-directory from the ZIP
/// to avoid extracting the entire archive (peak disk: ~one directory vs full ZIP).
#[allow(clippy::too_many_arguments)]
fn process_one_zip(
    zip_path: &Path,
    manifest_dir: &Path,
    dry_run: bool,
    verbose: bool,
    include_trashed: bool,
    retry_failed: bool,
    strict_extensions: bool,
    unknown_report: Option<&Path>,
) -> Result<ImportSummary> {
    process_zip_streaming(
        zip_path,
        manifest_dir,
        dry_run,
        verbose,
        include_trashed,
        retry_failed,
        strict_extensions,
        unknown_report,
    )
}

// MARK: - Streaming ZIP processor

/// Entry metadata collected during Phase 1 (ZIP indexing).
struct ZipEntry {
    index: usize,
    relative_path: String,
    filename: String,
    /// false if filtered out by already_imported / retry_failed
    should_import: bool,
}

#[derive(Default)]
struct ZipDirGroup {
    media: Vec<ZipEntry>,
    json: Vec<ZipEntry>,
}

/// Stream-process a ZIP file one directory at a time.
///
/// Phase 1: Index all ZIP entries by parent directory (no disk I/O).
/// Phase 2: For each directory, extract its files to a temp dir, run sidecar
///           matching / live-photo detection / import, then delete the temp files.
/// Phase 3: Write merged manifest.
#[allow(clippy::too_many_arguments)]
fn process_zip_streaming(
    zip_path: &Path,
    manifest_dir: &Path,
    dry_run: bool,
    verbose: bool,
    include_trashed: bool,
    retry_failed: bool,
    strict_extensions: bool,
    unknown_report: Option<&Path>,
) -> Result<ImportSummary> {
    let zip_stem = zip_path.file_stem().unwrap_or_default().to_string_lossy();
    let zip_name = zip_path.file_name().unwrap_or_default().to_string_lossy();
    let manifest_path = manifest_dir.join(format!(".photoferry-manifest-{}.json", zip_stem));
    let tmp_dir = manifest_dir.join(".photoferry-stream-tmp");

    // Load existing manifest for resume / retry filtering
    let existing_manifest = manifest::read_manifest_strict(&manifest_path).with_context(|| {
        format!(
            "Refusing to continue with corrupt manifest {}",
            manifest_path.display()
        )
    })?;

    let already_imported: HashSet<String> = existing_manifest
        .as_ref()
        .map(|m| m.imported.iter().map(|e| e.path.clone()).collect())
        .unwrap_or_default();

    let failed_paths: HashSet<String> = if retry_failed {
        existing_manifest
            .as_ref()
            .map(|m| m.failed.iter().map(|e| e.path.clone()).collect())
            .unwrap_or_default()
    } else {
        HashSet::new()
    };

    if retry_failed && failed_paths.is_empty() {
        display::print_info("No previously-failed files to retry.");
        return Ok(ImportSummary::default());
    }
    if dry_run && !already_imported.is_empty() {
        display::print_info(&format!(
            "{} already imported (skipping)",
            already_imported.len()
        ));
    }

    // ── Phase 1: Index ZIP entries by directory ──────────────────────────

    let file = std::fs::File::open(zip_path)
        .with_context(|| format!("Cannot open ZIP: {}", zip_path.display()))?;
    let mut archive = zip::ZipArchive::new(BufReader::new(file))
        .with_context(|| format!("Invalid ZIP: {}", zip_path.display()))?;

    // Detect "Takeout/" wrapper prefix
    let content_prefix = {
        let mut prefix = String::new();
        for i in 0..archive.len().min(20) {
            if let Ok(entry) = archive.by_index_raw(i) {
                if !entry.is_dir() && entry.name().starts_with("Takeout/") {
                    prefix = "Takeout/".to_string();
                    break;
                }
            }
        }
        prefix
    };

    let mut dirs: HashMap<String, ZipDirGroup> = HashMap::new();
    let mut unknown_stats = takeout::InventoryStats::default();
    let mut total_photos = 0usize;
    let mut total_videos = 0usize;
    let mut total_to_process = 0usize;

    for i in 0..archive.len() {
        let entry = archive.by_index_raw(i)?;
        if entry.is_dir() {
            continue;
        }
        let entry_path = entry.name().to_string();
        let entry_size = entry.size();
        drop(entry); // release borrow

        let relative = entry_path
            .strip_prefix(&content_prefix)
            .unwrap_or(&entry_path)
            .to_string();

        let path = Path::new(&relative);
        let dir_key = path
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let Some(fname) = path.file_name() else {
            continue;
        };
        let filename = fname.to_string_lossy().to_string();
        let ext = Path::new(&filename)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();

        if ext == "json" {
            dirs.entry(dir_key).or_default().json.push(ZipEntry {
                index: i,
                relative_path: relative,
                filename,
                should_import: false, // JSON entries are never directly imported
            });
        } else if let Some(media_type) = takeout::classify_extension(&ext) {
            // Always count for summary stats
            match media_type {
                takeout::MediaType::Photo => total_photos += 1,
                takeout::MediaType::Video => total_videos += 1,
            }
            // Determine whether this file should be imported
            let dominated = already_imported.contains(&relative)
                || (retry_failed && !failed_paths.contains(&relative));
            if !dominated {
                total_to_process += 1;
            }
            // Always add to the group (needed for live-photo pair detection even
            // when the file itself is already imported)
            dirs.entry(dir_key).or_default().media.push(ZipEntry {
                index: i,
                relative_path: relative,
                filename,
                should_import: !dominated,
            });
        } else {
            unknown_stats.unknown_extensions += 1;
            if unknown_stats.unknown_examples.len() < 5 {
                unknown_stats.unknown_examples.push(relative.clone());
            }
            unknown_stats.unknown_files.push(takeout::UnknownFile {
                path: PathBuf::from(&relative),
                ext,
                size_bytes: entry_size,
            });
        }
    }

    // Phase 1 summary
    display::print_info(&format!("Photos: {}", total_photos));
    display::print_info(&format!("Videos: {}", total_videos));
    if !already_imported.is_empty() {
        display::print_info(&format!(
            "Already imported: {} (skipping)",
            already_imported.len()
        ));
    }
    if unknown_stats.unknown_extensions > 0 {
        display::print_warning(&format!(
            "Unknown extensions (skipped): {}",
            unknown_stats.unknown_extensions
        ));
        if !unknown_stats.unknown_examples.is_empty() {
            display::print_info(&format!(
                "Examples: {}",
                unknown_stats.unknown_examples.join(", ")
            ));
        }
    }

    if let Some(report_path) = unknown_report {
        write_unknown_report(report_path, zip_name.as_ref(), &unknown_stats.unknown_files)?;
    }
    if strict_extensions && unknown_stats.unknown_extensions > 0 {
        let examples = if unknown_stats.unknown_examples.is_empty() {
            "<none>".to_string()
        } else {
            unknown_stats.unknown_examples.join(", ")
        };
        return Err(anyhow::anyhow!(format!(
            "{STRICT_EXTENSIONS_ABORT}: Unknown extensions detected ({}). Examples: {}. Re-run without --strict-extensions to proceed.",
            unknown_stats.unknown_extensions, examples
        )));
    }
    if dry_run {
        return Ok(ImportSummary::default());
    }
    if total_to_process == 0 {
        display::print_warning("No media files to import.");
        return Ok(ImportSummary::default());
    }

    // ── Phase 2: Process each directory ──────────────────────────────────

    // Clean stale temp dir
    if tmp_dir.exists() {
        std::fs::remove_dir_all(&tmp_dir)?;
    }

    let start = Instant::now();
    let mut summary = ImportSummary::default();
    let mut album_ids: HashMap<String, String> = HashMap::new();
    let mut all_imported: Vec<(String, String, Option<String>, bool)> = Vec::new();
    let mut all_failed: Vec<(String, String)> = Vec::new();
    let mut all_live_fallbacks: Vec<(String, String, String)> = Vec::new();

    let pb = if verbose {
        ProgressBar::hidden()
    } else {
        let pb = ProgressBar::new(total_to_process as u64);
        pb.set_style(
            ProgressStyle::with_template(
                "[{bar:40}] {pos}/{len} {per_sec:.1}/s ETA {eta} {msg}",
            )
            .unwrap_or_else(|_| ProgressStyle::default_bar())
            .progress_chars("##-"),
        );
        pb
    };

    let mut dir_keys: Vec<String> = dirs.keys().cloned().collect();
    dir_keys.sort();

    for dir_key in &dir_keys {
        let group = dirs.get(dir_key).unwrap();
        // Skip directories with no importable media
        if !group.media.iter().any(|e| e.should_import) {
            continue;
        }

        // Create temp subdirectory matching the original structure
        let extract_dir = if dir_key.is_empty() {
            tmp_dir.clone()
        } else {
            tmp_dir.join(dir_key)
        };
        std::fs::create_dir_all(&extract_dir)?;

        // Extract JSON sidecars
        let mut json_paths = Vec::new();
        for je in &group.json {
            let dest = extract_dir.join(&je.filename);
            let mut zf = archive.by_index(je.index)?;
            let mut out = std::fs::File::create(&dest)?;
            std::io::copy(&mut zf, &mut out)?;
            json_paths.push(dest);
        }

        // Extract ALL media (including already-imported, needed for live-pair detection)
        struct ExtractedMedia {
            disk_path: PathBuf,
            relative_path: String,
            should_import: bool,
        }
        let mut media_map: Vec<ExtractedMedia> = Vec::new();
        for me in &group.media {
            let dest = extract_dir.join(&me.filename);
            let mut zf = archive.by_index(me.index)?;
            let mut out = std::fs::File::create(&dest)?;
            std::io::copy(&mut zf, &mut out)?;
            media_map.push(ExtractedMedia {
                disk_path: dest,
                relative_path: me.relative_path.clone(),
                should_import: me.should_import,
            });
        }

        // ── Per-directory analysis (mirrors scan_directory logic) ────────

        // Album detection
        let dir_path = Path::new(dir_key);
        let album = takeout::detect_album(dir_path, &json_paths);
        let is_year = takeout::is_year_folder(dir_path);
        let effective_album = if is_year { None } else { album };

        // Ensure album exists in Photos.app
        if let Some(ref album_name) = effective_album {
            if !album_ids.contains_key(album_name) {
                match importer::create_album(album_name) {
                    Ok(id) => {
                        album_ids.insert(album_name.clone(), id);
                    }
                    Err(e) => {
                        display::print_warning(&format!(
                            "Failed to create album '{}': {}",
                            album_name, e
                        ));
                    }
                }
            }
        }

        // Sidecar candidates
        let all_disk_files: Vec<PathBuf> = json_paths
            .iter()
            .chain(media_map.iter().map(|m| &m.disk_path))
            .cloned()
            .collect();
        let json_candidates = sidecar::collect_json_candidates(&all_disk_files);

        // Live Photo pairs (uses ALL media files including already-imported)
        let disk_media_paths: Vec<PathBuf> =
            media_map.iter().map(|m| m.disk_path.clone()).collect();
        let live_pairs = takeout::detect_live_photo_pairs(&disk_media_paths);

        // Truncation collision detection
        let mut truncation_counts: HashMap<String, Vec<PathBuf>> = HashMap::new();
        for em in &media_map {
            let name = em
                .disk_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            if let Some(trunc) = sidecar::truncated_media_base(name) {
                truncation_counts
                    .entry(trunc)
                    .or_default()
                    .push(em.disk_path.clone());
            }
        }
        let ambiguous_truncations: HashSet<String> = truncation_counts
            .iter()
            .filter(|(_, v)| v.len() > 1)
            .map(|(k, _)| k.clone())
            .collect();

        // ── Import each media file ──────────────────────────────────────

        for em in &media_map {
            // Skip already-imported (they were extracted only for live-pair detection)
            if !em.should_import {
                continue;
            }

            let filename = em
                .disk_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            pb.set_message(filename.clone());

            let ext = em
                .disk_path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("");
            let Some(media_type) = takeout::classify_extension(ext) else {
                pb.inc(1);
                continue;
            };

            // Skip videos that are Live Photo pair components
            if media_type == takeout::MediaType::Video
                && live_pairs.values().any(|v| v == &em.disk_path)
            {
                // Not counted in total_to_process, so don't increment pb
                continue;
            }

            // Sidecar matching
            let sidecar_match = if sidecar::truncated_media_base(&filename)
                .as_ref()
                .is_some_and(|t| ambiguous_truncations.contains(t))
            {
                None // truncation collision — skip sidecar
            } else {
                sidecar::find_sidecar_with_strength(&em.disk_path, &json_candidates)
            };

            let sidecar_path = sidecar_match.as_ref().map(|m| m.path.clone());
            let sidecar_strength = sidecar_match.as_ref().map(|m| m.strength);
            let takeout_meta = sidecar_path.as_ref().and_then(|sp| {
                let bytes = std::fs::read(sp).ok()?;
                metadata::parse_sidecar(&bytes).ok()
            });

            // Trashed check
            let is_trashed = takeout_meta.as_ref().is_some_and(|m| m.is_trashed());
            let is_strong =
                sidecar_strength == Some(sidecar::SidecarMatchStrength::Strong);
            if is_trashed && is_strong && !include_trashed {
                pb.inc(1);
                continue;
            }

            let photo_metadata = takeout_meta.as_ref().map(|m| m.to_photo_metadata());

            let live_photo_pair = if media_type == takeout::MediaType::Photo {
                live_pairs.get(&em.disk_path).cloned()
            } else {
                None
            };

            // Import the file
            let path_str = match em.disk_path.to_str() {
                Some(p) => p,
                None => {
                    let err = "Invalid UTF-8 file path".to_string();
                    summary.failed.push(ImportFailure {
                        path: em.relative_path.clone(),
                        error: err.clone(),
                    });
                    all_failed.push((em.relative_path.clone(), err));
                    pb.inc(1);
                    continue;
                }
            };

            let mut used_live_fallback = false;
            let import_result = if let Some(ref video_disk) = live_photo_pair {
                let live_result = match video_disk.to_str() {
                    Some(vstr) => {
                        importer::import_live_photo(path_str, vstr, photo_metadata.as_ref())
                    }
                    None => Err(anyhow::anyhow!("Invalid UTF-8 in Live Photo video path")),
                };
                match live_result {
                    Ok(r) if r.success => Ok(r),
                    Ok(r) => {
                        let live_err = r
                            .error
                            .clone()
                            .unwrap_or_else(|| "Live Photo import failed".to_string());
                        match importer::import_photo(path_str, photo_metadata.as_ref(), false) {
                            Ok(fb) if fb.success => {
                                used_live_fallback = true;
                                Ok(fb)
                            }
                            Ok(fb) => {
                                let fb_err = fb
                                    .error
                                    .unwrap_or_else(|| "Fallback failed".to_string());
                                Ok(importer::ImportResult {
                                    success: false,
                                    local_identifier: None,
                                    error: Some(format!(
                                        "Live Photo failed ({live_err}); fallback failed ({fb_err})"
                                    )),
                                })
                            }
                            Err(e) => Err(anyhow::anyhow!(
                                "Live Photo failed ({live_err}); fallback error: {e}"
                            )),
                        }
                    }
                    Err(err) => {
                        match importer::import_photo(path_str, photo_metadata.as_ref(), false) {
                            Ok(fb) if fb.success => {
                                used_live_fallback = true;
                                Ok(fb)
                            }
                            Ok(fb) => {
                                let fb_err = fb
                                    .error
                                    .unwrap_or_else(|| "Fallback failed".to_string());
                                Ok(importer::ImportResult {
                                    success: false,
                                    local_identifier: None,
                                    error: Some(format!(
                                        "Live Photo error ({err}); fallback failed ({fb_err})"
                                    )),
                                })
                            }
                            Err(e) => Err(anyhow::anyhow!(
                                "Live Photo error ({err}); fallback error: {e}"
                            )),
                        }
                    }
                }
            } else {
                let is_video = matches!(media_type, takeout::MediaType::Video);
                importer::import_photo(path_str, photo_metadata.as_ref(), is_video)
            };

            match import_result {
                Ok(result) if result.success => {
                    let Some(local_id) = result.local_identifier.clone() else {
                        let err =
                            "import succeeded but no local identifier returned".to_string();
                        summary.failed.push(ImportFailure {
                            path: em.relative_path.clone(),
                            error: err.clone(),
                        });
                        all_failed.push((em.relative_path.clone(), err));
                        pb.inc(1);
                        continue;
                    };

                    if used_live_fallback {
                        summary.live_photo_fallbacks += 1;
                        if let Some(video_disk) = live_photo_pair.as_ref() {
                            let video_fname = video_disk
                                .file_name()
                                .map(|f| f.to_string_lossy().to_string())
                                .unwrap_or_default();
                            let video_rel = if dir_key.is_empty() {
                                video_fname
                            } else {
                                format!("{}/{}", dir_key, video_fname)
                            };
                            summary
                                .live_photo_fallback_entries
                                .push(LivePhotoFallback {
                                    photo_path: PathBuf::from(&em.relative_path),
                                    video_path: PathBuf::from(&video_rel),
                                    local_id: local_id.clone(),
                                });
                            all_live_fallbacks.push((
                                em.relative_path.clone(),
                                video_rel,
                                local_id.clone(),
                            ));
                        }
                        pb.println(format!(
                            "  ! Live Photo import failed; imported still photo only: {}",
                            em.relative_path
                        ));
                    }

                    let is_live = live_photo_pair.is_some() && !used_live_fallback;
                    let creation_date =
                        photo_metadata.as_ref().and_then(|m| m.creation_date.clone());

                    summary.imported.push(ImportedFile {
                        path: PathBuf::from(&em.relative_path),
                        local_id: local_id.clone(),
                        album: effective_album.clone(),
                        creation_date: creation_date.clone(),
                        is_live_photo: is_live,
                    });
                    all_imported.push((
                        em.relative_path.clone(),
                        local_id.clone(),
                        creation_date,
                        is_live,
                    ));

                    // Album assignment
                    if let Some(album_name) = effective_album.as_ref()
                        && let Some(album_id) = album_ids.get(album_name)
                        && let Some(actual_id) = result.local_identifier.as_deref()
                    {
                        match importer::add_to_album(album_id, actual_id) {
                            Ok(true) => {}
                            Ok(false) => {
                                pb.println(format!(
                                    "  ! Failed to add '{}' to album '{}'",
                                    filename, album_name
                                ));
                            }
                            Err(e) => {
                                pb.println(format!(
                                    "  ! Failed to add '{}' to album '{}': {}",
                                    filename, album_name, e
                                ));
                            }
                        }
                    }

                    if verbose {
                        let label = if live_photo_pair.is_some() {
                            let vname = live_photo_pair
                                .as_ref()
                                .and_then(|p| p.file_name())
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default();
                            format!("{}+{}", filename, vname)
                        } else {
                            filename.clone()
                        };
                        display::print_success(&format!(
                            "[{}/{}] {} -> {}",
                            summary.imported.len(),
                            total_to_process,
                            label,
                            local_id
                        ));
                    }
                }
                Ok(result) => {
                    let err = result
                        .error
                        .unwrap_or_else(|| "unknown error".to_string());
                    summary.failed.push(ImportFailure {
                        path: em.relative_path.clone(),
                        error: err.clone(),
                    });
                    all_failed.push((em.relative_path.clone(), err.clone()));
                    if verbose {
                        pb.println(format!("  ! {} — {}", filename, err));
                    }
                }
                Err(error) => {
                    let err = error.to_string();
                    summary.failed.push(ImportFailure {
                        path: em.relative_path.clone(),
                        error: err.clone(),
                    });
                    all_failed.push((em.relative_path.clone(), err.clone()));
                    if verbose {
                        pb.println(format!("  ! {} — {}", filename, err));
                    }
                }
            }

            pb.inc(1);
        }

        // Clean up this directory's files before processing the next
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    pb.finish_and_clear();
    summary.elapsed = start.elapsed();

    // ── Phase 3: Write manifest ─────────────────────────────────────────

    manifest::merge_and_write(
        &manifest_path,
        &zip_name,
        &all_imported,
        &all_failed,
        &all_live_fallbacks,
    )?;

    Ok(summary)
}

fn cmd_import(file: &Path, metadata_json: Option<&str>) -> Result<()> {
    let path = file
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Invalid file path"))?;

    display::print_header(&format!("Importing {}", file.display()));

    let metadata = match metadata_json {
        Some(json) => Some(serde_json::from_str::<importer::PhotoMetadata>(json)?),
        None => None,
    };

    let is_video = match takeout::media_type_from_path(file) {
        Some(takeout::MediaType::Video) => true,
        Some(takeout::MediaType::Photo) => false,
        None => {
            display::print_warning("Unknown file extension — assuming photo import");
            false
        }
    };

    let result = importer::import_photo(path, metadata.as_ref(), is_video)?;

    if result.success {
        display::print_success(&format!(
            "Imported → {}",
            result.local_identifier.as_deref().unwrap_or("unknown")
        ));
    } else {
        display::print_error(&format!(
            "Failed: {}",
            result.error.as_deref().unwrap_or("unknown error")
        ));
    }

    Ok(())
}

fn cmd_albums(dir: &Path) -> Result<()> {
    let dir = expand_tilde(dir);
    display::print_header(&format!("Scanning albums in {}", dir.display()));

    let zips = takeout::find_takeout_zips(&dir)?;
    if zips.is_empty() {
        display::print_info("No Takeout zips found.");
        return Ok(());
    }

    let mut all_albums = Vec::new();

    for zip_path in &zips {
        let extract_dir = dir.join(format!(
            ".photoferry-extract-{}",
            zip_path.file_stem().unwrap_or_default().to_string_lossy()
        ));
        std::fs::create_dir_all(&extract_dir)?;

        let content_root = match takeout::extract_zip(zip_path, &extract_dir) {
            Ok(root) => root,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&extract_dir);
                return Err(e.context(format!("Failed to extract {}", zip_path.display())));
            }
        };
        let inventory = match takeout::scan_directory(&content_root, &takeout::ScanOptions::default()) {
            Ok(inv) => inv,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&extract_dir);
                return Err(e.context(format!(
                    "Failed to scan extracted content for {}",
                    zip_path.display()
                )));
            }
        };
        all_albums.extend(inventory.albums);

        std::fs::remove_dir_all(&extract_dir)?;
    }

    all_albums.sort();
    all_albums.dedup();

    if all_albums.is_empty() {
        display::print_info("No albums detected.");
    } else {
        display::print_info(&format!("Found {} album(s):", all_albums.len()));
        for album in &all_albums {
            display::print_info(&format!("  {album}"));
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_download(
    job_id: &str,
    user_id: &str,
    dir: &Path,
    start: usize,
    end: usize,
    download_only: bool,
    use_chrome: bool,
    verbose: bool,
    include_trashed: bool,
    strict_extensions: bool,
    unknown_report: Option<&Path>,
    icloud_confirmed: bool,
) -> Result<()> {
    let dir = expand_tilde(dir);
    std::fs::create_dir_all(&dir)?;

    display::print_header(&format!(
        "Downloading Takeout parts {start}–{end} → {}",
        dir.display()
    ));
    if !download_only && !icloud_confirmed {
        display::print_warning(
            "iCloud sync is not confirmed. ZIPs will be kept even when local verify passes.",
        );
    }

    // Check Photos access up front (unless download-only)
    if !download_only {
        let access = importer::check_access()?;
        ensure_full_photos_access(&access, "download/import verify")?;
        display::print_success(&format!("Photos access: {} (authorized)", access.status));
    }

    // Load or create download progress manifest
    let mut progress = downloader::DownloadProgress::load(&dir, job_id)?;
    progress.user_id = user_id.to_string();
    progress.save(&dir)?;

    let mut total_imported = 0usize;
    let mut total_failed_dl = 0usize;
    let mut total_failed_import = 0usize;

    // Build HTTP client (only needed for non-Chrome mode)
    let mut client: Option<Client> = if !use_chrome {
        fn fresh_client() -> Result<Client> {
            let cookies = downloader::get_chrome_cookies().context(
                "Failed to get Chrome cookies — ensure Chrome is running and logged into Google",
            )?;
            display::print_info(&format!("Loaded {} Google cookies", cookies.len()));
            downloader::build_client(&cookies)
        }
        Some(fresh_client()?)
    } else {
        display::print_info("Using Chrome browser for downloads (passkey/re-auth handled natively)");
        None
    };

    for i in start..=end {
        if progress.is_completed(i) {
            display::print_info(&format!("  [{i:02}] Already done, skipping"));
            continue;
        }

        println!();
        display::print_header(&format!("Part {i}/{end}"));

        // Disk space guard — wait until ≥20GB free before downloading
        const MIN_FREE_GB: u64 = 20;
        loop {
            match available_space_gb(&dir) {
                Some(gb) if gb >= MIN_FREE_GB => break,
                Some(gb) => {
                    display::print_warning(&format!(
                        "  [{i:02}] Low disk: {gb}GB free (need {MIN_FREE_GB}GB) — waiting 60s for iCloud to upload"
                    ));
                    std::thread::sleep(std::time::Duration::from_secs(60));
                }
                None => break, // Can't check — proceed anyway
            }
        }

        // Download — Chrome mode or direct HTTP
        let zip_path = if use_chrome {
            match downloader::download_via_chrome(job_id, user_id, i, &dir) {
                Ok(p) => p,
                Err(e) => {
                    display::print_error(&format!("  [{i:02}] Chrome download failed: {e} — skipping"));
                    progress.mark_failed(i, &dir);
                    total_failed_dl += 1;
                    continue;
                }
            }
        } else {
            // Direct HTTP download with cookie refresh per-part
            fn fresh_client() -> Result<Client> {
                let cookies = downloader::get_chrome_cookies().context(
                    "Failed to get Chrome cookies",
                )?;
                downloader::build_client(&cookies)
            }
            if let Ok(c) = fresh_client() {
                client = Some(c);
            }
            let c = client.as_ref().unwrap();
            match downloader::download_zip(c, job_id, user_id, i, &dir) {
                Ok(p) => p,
                Err(e) => {
                    display::print_error(&format!("  [{i:02}] Download failed: {e}"));
                    display::print_info(&format!("  [{i:02}] Refreshing cookies and retrying in 10s..."));
                    std::thread::sleep(std::time::Duration::from_secs(10));
                    if let Ok(c) = fresh_client() {
                        client = Some(c);
                    }
                    match downloader::download_zip(client.as_ref().unwrap(), job_id, user_id, i, &dir) {
                        Ok(p) => p,
                        Err(e2) => {
                            display::print_error(&format!("  [{i:02}] Retry failed: {e2} — skipping"));
                            progress.mark_failed(i, &dir);
                            total_failed_dl += 1;
                            continue;
                        }
                    }
                }
            }
        };

        if download_only {
            display::print_success(&format!("  [{i:02}] Downloaded → {}", zip_path.display()));
            progress.mark_completed(i, &dir);
            total_imported += 1;
            continue;
        }

        // Import
        display::print_info(&format!(
            "  [{i:02}] Importing {}...",
            zip_path.file_name().unwrap_or_default().to_string_lossy()
        ));
        match process_one_zip(
            &zip_path,
            &dir,
            false,
            verbose,
            include_trashed,
            false,
            strict_extensions,
            unknown_report,
        ) {
            Ok(summary) => {
                print_import_summary(&summary);
                total_imported += summary.imported.len();
                let had_failures = !summary.failed.is_empty();
                if had_failures {
                    total_failed_import += summary.failed.len();
                    display::print_warning(&format!(
                        "  [{i:02}] {} files failed — zip kept for retry",
                        summary.failed.len()
                    ));
                    // Don't mark completed — allow retry on next run
                } else {
                    // Verify all assets exist in Photos Library before deleting zip
                    if verify_zip_manifest(&zip_path, &dir) {
                        progress.mark_completed(i, &dir);
                        match verify_success_action(icloud_confirmed) {
                            VerifySuccessAction::KeepZipAndMarkCompleted => {
                                display::print_warning(&format!(
                                    "  [{i:02}] Verify passed, but iCloud sync not confirmed (--icloud-confirmed not set) — keeping zip (part marked completed)"
                                ));
                                continue;
                            }
                            VerifySuccessAction::DeleteZipAndMarkCompleted => {
                                if let Err(e) = std::fs::remove_file(&zip_path) {
                                    display::print_warning(&format!(
                                        "  [{i:02}] Verified OK but could not delete zip: {e}"
                                    ));
                                } else {
                                    display::print_success(&format!(
                                        "  [{i:02}] Verified + deleted {}",
                                        zip_path.file_name().unwrap_or_default().to_string_lossy()
                                    ));
                                }
                            }
                        }
                    } else {
                        display::print_warning(&format!(
                            "  [{i:02}] Import OK but verify failed — keeping zip"
                        ));
                        // Don't mark completed — verify on next run
                    }
                }
            }
            Err(e) => {
                let msg = e.to_string();
                if msg.starts_with(STRICT_EXTENSIONS_ABORT) {
                    let cleaned = msg
                        .strip_prefix(STRICT_EXTENSIONS_ABORT)
                        .unwrap_or(&msg)
                        .trim_start_matches(':')
                        .trim();
                    return Err(anyhow::anyhow!(cleaned.to_string()));
                }
                display::print_error(&format!("  [{i:02}] Import failed: {e} — zip kept"));
                progress.mark_failed(i, &dir);
                total_failed_import += 1;
            }
        }
    }

    println!();
    display::print_header("Download run complete");
    display::print_info(&format!("Parts completed: {}", progress.completed.len()));
    if download_only {
        display::print_info(&format!("Downloaded: {total_imported}"));
    } else {
        display::print_info(&format!("Photos imported: {total_imported}"));
    }
    if total_failed_dl > 0 {
        display::print_error(&format!("Download failures: {total_failed_dl}"));
    }
    if total_failed_import > 0 {
        display::print_warning(&format!("Import failures: {total_failed_import}"));
    }
    if total_failed_dl == 0 && total_failed_import == 0 {
        display::print_success("All parts completed successfully");
    }

    Ok(())
}

// MARK: - Helpers

fn write_unknown_report(
    report_path: &Path,
    zip_name: &str,
    unknown_files: &[takeout::UnknownFile],
) -> Result<()> {
    if unknown_files.is_empty() {
        return Ok(());
    }
    let mut needs_header = true;
    if let Ok(meta) = std::fs::metadata(report_path) {
        if meta.len() > 0 {
            needs_header = false;
        }
    }

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(report_path)?;

    if needs_header {
        use std::io::Write;
        writeln!(file, "zip,relative_path,ext,size_bytes")?;
    }

    use std::io::Write;
    for entry in unknown_files {
        let rel = entry.path.to_string_lossy().replace('"', "\"\"");
        let ext = entry.ext.replace('"', "\"\"");
        writeln!(file, "\"{zip_name}\",\"{rel}\",\"{ext}\",{}", entry.size_bytes)?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VerifySuccessAction {
    KeepZipAndMarkCompleted,
    DeleteZipAndMarkCompleted,
}

fn verify_success_action(icloud_confirmed: bool) -> VerifySuccessAction {
    if icloud_confirmed {
        VerifySuccessAction::DeleteZipAndMarkCompleted
    } else {
        VerifySuccessAction::KeepZipAndMarkCompleted
    }
}

#[derive(Debug)]
struct ImportFailure {
    path: String,
    error: String,
}

#[derive(Debug)]
struct ImportedFile {
    path: PathBuf,
    local_id: String,
    album: Option<String>,
    creation_date: Option<String>,
    is_live_photo: bool,
}

#[derive(Debug)]
struct LivePhotoFallback {
    photo_path: PathBuf,
    video_path: PathBuf,
    local_id: String,
}

#[derive(Debug, Default)]
struct ImportSummary {
    imported: Vec<ImportedFile>,
    failed: Vec<ImportFailure>,
    elapsed: std::time::Duration,
    live_photo_fallbacks: usize,
    live_photo_fallback_entries: Vec<LivePhotoFallback>,
}

impl ImportSummary {
    fn merge(&mut self, other: &ImportSummary) {
        self.imported
            .extend(other.imported.iter().map(|file| ImportedFile {
                path: file.path.clone(),
                local_id: file.local_id.clone(),
                album: file.album.clone(),
                creation_date: file.creation_date.clone(),
                is_live_photo: file.is_live_photo,
            }));
        self.failed
            .extend(other.failed.iter().map(|f| ImportFailure {
                path: f.path.clone(),
                error: f.error.clone(),
            }));
        self.elapsed += other.elapsed;
        self.live_photo_fallbacks += other.live_photo_fallbacks;
        self.live_photo_fallback_entries
            .extend(other.live_photo_fallback_entries.iter().map(|e| LivePhotoFallback {
                photo_path: e.photo_path.clone(),
                video_path: e.video_path.clone(),
                local_id: e.local_id.clone(),
            }));
    }
}

fn import_inventory(inventory: &takeout::TakeoutInventory, verbose: bool) -> ImportSummary {
    let total = inventory.files.len();
    let mut summary = ImportSummary::default();
    let start = Instant::now();
    let mut album_ids: HashMap<String, String> = HashMap::new();

    if total == 0 {
        display::print_warning("No media files found to import.");
        return summary;
    }

    for album in inventory.albums.iter().cloned().collect::<HashSet<_>>() {
        match importer::create_album(&album) {
            Ok(album_id) => {
                album_ids.insert(album, album_id);
            }
            Err(err) => {
                display::print_warning(&format!("Failed to create album '{}': {}", album, err));
            }
        }
    }

    let pb = if verbose {
        ProgressBar::hidden()
    } else {
        let pb = ProgressBar::new(total as u64);
        let style =
            ProgressStyle::with_template("[{bar:40}] {pos}/{len} {per_sec:.1}/s ETA {eta} {msg}")
                .unwrap_or_else(|_| ProgressStyle::default_bar())
                .progress_chars("##-");
        pb.set_style(style);
        pb
    };

    for (index, file) in inventory.files.iter().enumerate() {
        let filename = file
            .path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        pb.set_message(filename.clone());

        let path = match file.path.to_str() {
            Some(p) => p,
            None => {
                let err = "Invalid UTF-8 file path".to_string();
                summary.failed.push(ImportFailure {
                    path: file.path.display().to_string(),
                    error: err.clone(),
                });
                if verbose {
                    pb.println(format!(
                        "  ! [{}/{}] {} — {}",
                        index + 1,
                        total,
                        filename,
                        err
                    ));
                }
                pb.inc(1);
                continue;
            }
        };

        let mut used_live_fallback = false;
        let import_result = if let Some(ref video_path) = file.live_photo_pair {
            let live_result = match video_path.to_str() {
                Some(video_str) => {
                    importer::import_live_photo(path, video_str, file.metadata.as_ref())
                }
                None => Err(anyhow::anyhow!("Invalid UTF-8 in Live Photo video path")),
            };

            match live_result {
                Ok(result) if result.success => Ok(result),
                Ok(result) => {
                    let live_err = result
                        .error
                        .clone()
                        .unwrap_or_else(|| "Live Photo import failed".to_string());
                    match importer::import_photo(path, file.metadata.as_ref(), false) {
                        Ok(fallback) if fallback.success => {
                            used_live_fallback = true;
                            Ok(fallback)
                        }
                        Ok(fallback) => {
                            let fb_err = fallback
                                .error
                                .unwrap_or_else(|| "Fallback photo import failed".to_string());
                            Ok(importer::ImportResult {
                                success: false,
                                local_identifier: None,
                                error: Some(format!(
                                    "Live Photo failed ({live_err}); fallback failed ({fb_err})"
                                )),
                            })
                        }
                        Err(e) => Err(anyhow::anyhow!(
                            "Live Photo failed ({live_err}); fallback error: {e}"
                        )),
                    }
                }
                Err(err) => match importer::import_photo(path, file.metadata.as_ref(), false) {
                    Ok(fallback) if fallback.success => {
                        used_live_fallback = true;
                        Ok(fallback)
                    }
                    Ok(fallback) => {
                        let fb_err = fallback
                            .error
                            .unwrap_or_else(|| "Fallback photo import failed".to_string());
                        Ok(importer::ImportResult {
                            success: false,
                            local_identifier: None,
                            error: Some(format!(
                                "Live Photo error ({err}); fallback failed ({fb_err})"
                            )),
                        })
                    }
                    Err(e) => Err(anyhow::anyhow!(
                        "Live Photo error ({err}); fallback error: {e}"
                    )),
                },
            }
        } else {
            let is_video = matches!(file.media_type, takeout::MediaType::Video);
            importer::import_photo(path, file.metadata.as_ref(), is_video)
        };

        match import_result {
            Ok(result) if result.success => {
                let Some(local_id) = result.local_identifier.clone() else {
                    let err = "import succeeded but no local identifier returned".to_string();
                    summary.failed.push(ImportFailure {
                        path: file.path.display().to_string(),
                        error: err.clone(),
                    });
                    if verbose {
                        pb.println(format!(
                            "  ! [{}/{}] {} — {}",
                            index + 1,
                            total,
                            filename,
                            err
                        ));
                    }
                    pb.inc(1);
                    continue;
                };
                if used_live_fallback {
                    summary.live_photo_fallbacks += 1;
                    if let Some(video_path) = file.live_photo_pair.as_ref() {
                        summary.live_photo_fallback_entries.push(LivePhotoFallback {
                            photo_path: file.path.clone(),
                            video_path: video_path.clone(),
                            local_id: local_id.clone(),
                        });
                    }
                    pb.println(format!(
                        "  ! Live Photo import failed; imported still photo only: {}",
                        file.path.display()
                    ));
                }

                summary.imported.push(ImportedFile {
                    path: file.path.clone(),
                    local_id: local_id.clone(),
                    album: file.album.clone(),
                    creation_date: file.metadata.as_ref().and_then(|m| m.creation_date.clone()),
                    is_live_photo: file.live_photo_pair.is_some() && !used_live_fallback,
                });

                if let Some(album_name) = file.album.as_ref()
                    && let Some(album_id) = album_ids.get(album_name)
                {
                    if let Some(actual_local_id) = result.local_identifier.as_deref() {
                        match importer::add_to_album(album_id, actual_local_id) {
                            Ok(true) => {}
                            Ok(false) => {
                                pb.println(format!(
                                    "  ! Failed to add '{}' to album '{}'",
                                    filename, album_name
                                ));
                            }
                            Err(err) => {
                                pb.println(format!(
                                    "  ! Failed to add '{}' to album '{}': {}",
                                    filename, album_name, err
                                ));
                            }
                        }
                    } else {
                        pb.println(format!(
                            "  ! No local identifier for '{}'; skipping album assignment",
                            filename
                        ));
                    }
                }

                if verbose {
                    let label = if file.live_photo_pair.is_some() {
                        let video_name = file
                            .live_photo_pair
                            .as_ref()
                            .and_then(|p| p.file_name())
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        format!("{}+{}", filename, video_name)
                    } else {
                        filename.clone()
                    };
                    display::print_success(&format!(
                        "[{}/{}] {} -> {}",
                        index + 1,
                        total,
                        label,
                        local_id
                    ));
                }
            }
            Ok(result) => {
                let err = result.error.unwrap_or_else(|| "unknown error".to_string());
                summary.failed.push(ImportFailure {
                    path: file.path.display().to_string(),
                    error: err.clone(),
                });
                if verbose {
                    pb.println(format!(
                        "  ! [{}/{}] {} — {}",
                        index + 1,
                        total,
                        filename,
                        err
                    ));
                }
            }
            Err(error) => {
                let err = error.to_string();
                summary.failed.push(ImportFailure {
                    path: file.path.display().to_string(),
                    error: err.clone(),
                });
                if verbose {
                    pb.println(format!(
                        "  ! [{}/{}] {} — {}",
                        index + 1,
                        total,
                        filename,
                        err
                    ));
                }
            }
        }

        pb.inc(1);
    }

    pb.finish_and_clear();
    summary.elapsed = start.elapsed();
    summary
}

fn print_import_summary(summary: &ImportSummary) {
    let secs = summary.elapsed.as_secs();
    let elapsed_str = if secs >= 60 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}s", secs)
    };

    display::print_info(&format!("Imported: {}", summary.imported.len()));
    display::print_info(&format!("Failed: {}", summary.failed.len()));
    display::print_info(&format!("Elapsed: {}", elapsed_str));
    if summary.live_photo_fallbacks > 0 {
        display::print_warning(&format!(
            "Live Photo fallbacks (still photo only): {}",
            summary.live_photo_fallbacks
        ));
    }

    if !summary.failed.is_empty() {
        display::print_warning("Failed files:");
        for failed in &summary.failed {
            display::print_error(&format!("{} — {}", failed.path, failed.error));
        }
    }
}

fn cmd_verify(dir: &Path) -> Result<()> {
    let dir = expand_tilde(dir);
    display::print_header(&format!("Verifying imports in {}", dir.display()));

    let manifests: Vec<PathBuf> = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with(".photoferry-manifest-") && n.ends_with(".json"))
                .unwrap_or(false)
        })
        .collect();

    if manifests.is_empty() {
        display::print_info("No manifests found.");
        return Ok(());
    }

    let access = importer::check_access()?;
    ensure_full_photos_access(&access, "verify")?;

    let mut total_verified_ok = 0usize;
    let mut total_missing = 0usize;
    let mut total_wrong_date = 0usize;
    let mut total_live_photo_pair_missing = 0usize;
    let mut total_live_photo_fallback = 0usize;

    for manifest_path in &manifests {
        let manifest = match manifest::read_manifest_strict(manifest_path) {
            Ok(Some(m)) => m,
            Ok(None) => {
                display::print_warning(&format!("Could not read {:?}", manifest_path));
                continue;
            }
            Err(e) => {
                return Err(e.context(format!(
                    "Refusing to verify with corrupt manifest {}",
                    manifest_path.display()
                )));
            }
        };

        display::print_header(&format!("Verifying {}", manifest.zip));
        display::print_info(&format!(
            "Checking {} imported assets...",
            manifest.imported.len()
        ));

        let mut live_photo_paths = HashSet::new();
        let zip_path = dir.join(&manifest.zip);
        if zip_path.exists() {
            match live_photo_paths_from_zip(&zip_path, &dir) {
                Ok(paths) => live_photo_paths = paths,
                Err(e) => display::print_warning(&format!(
                    "Live Photo fallback scan failed for {}: {}",
                    manifest.zip, e
                )),
            }
        }

        let ids: Vec<&str> = manifest
            .imported
            .iter()
            .map(|e| e.local_id.as_str())
            .collect();
        let results = importer::verify_assets(&ids)?;

        let result_map: HashMap<&str, &importer::AssetVerifyResult> = results
            .iter()
            .map(|r| (r.local_identifier.as_str(), r))
            .collect();

        let mut missing = vec![];
        let mut wrong_date = vec![];
        let mut live_pair_missing = vec![];
        let mut live_photo_fallback = vec![];

        for entry in &manifest.imported {
            match result_map.get(entry.local_id.as_str()) {
                None | Some(importer::AssetVerifyResult { found: false, .. }) => {
                    missing.push(entry);
                }
                Some(result) => {
                    if entry.is_live_photo == Some(true) && !result.has_paired_video {
                        live_pair_missing.push(entry);
                        continue;
                    }
                    if date_mismatch(entry.creation_date.as_deref(), result.creation_date.as_deref())
                    {
                        wrong_date.push((
                            entry,
                            result
                                .creation_date
                                .clone()
                                .unwrap_or_else(|| "<missing>".to_string()),
                        ));
                        continue;
                    }
                    if entry.is_live_photo == Some(false)
                        && live_photo_paths.contains(&entry.path)
                    {
                        live_photo_fallback.push(entry);
                    }
                    total_verified_ok += 1;
                }
            }
        }

        for e in &missing {
            display::print_error(&format!("MISSING: {} ({})", e.path, e.local_id));
            total_missing += 1;
        }
        for (e, actual) in &wrong_date {
            display::print_warning(&format!(
                "DATE MISMATCH: {} — expected {} got {}",
                e.path,
                e.creation_date.as_deref().unwrap_or("?"),
                actual
            ));
            total_wrong_date += 1;
        }
        for e in &live_pair_missing {
            display::print_warning(&format!(
                "LIVE PHOTO PAIR MISSING: {} ({})",
                e.path, e.local_id
            ));
            total_live_photo_pair_missing += 1;
        }
        for e in &live_photo_fallback {
            display::print_warning(&format!("LIVE PHOTO FELL BACK: {}", e.path));
            total_live_photo_fallback += 1;
        }

        display::print_info(&format!(
            "Verified: {} | Missing: {} | Wrong date: {} | Live pair missing: {} | Live fallback: {}",
            manifest.imported.len()
                - missing.len()
                - wrong_date.len()
                - live_pair_missing.len(),
            missing.len(),
            wrong_date.len(),
            live_pair_missing.len(),
            live_photo_fallback.len()
        ));
    }

    println!();
    display::print_header("Total");
    display::print_info(&format!("Verified OK: {}", total_verified_ok));
    if total_missing > 0 {
        display::print_error(&format!("Missing: {}", total_missing));
    }
    if total_wrong_date > 0 {
        display::print_warning(&format!("Wrong date: {}", total_wrong_date));
    }
    if total_live_photo_pair_missing > 0 {
        display::print_warning(&format!(
            "Live Photo pair missing: {}",
            total_live_photo_pair_missing
        ));
    }
    if total_live_photo_fallback > 0 {
        display::print_warning(&format!(
            "Live Photo fallbacks (still photo only): {}",
            total_live_photo_fallback
        ));
    }
    if total_missing == 0 && total_wrong_date == 0 && total_live_photo_pair_missing == 0 {
        display::print_success("All assets verified successfully");
    }

    Ok(())
}

fn cmd_retry_missing(dir: &Path, verbose: bool) -> Result<()> {
    let dir = expand_tilde(dir);
    display::print_header(&format!("Retrying missing assets in {}", dir.display()));

    let manifests: Vec<PathBuf> = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with(".photoferry-manifest-") && n.ends_with(".json"))
                .unwrap_or(false)
        })
        .collect();

    if manifests.is_empty() {
        display::print_info("No manifests found.");
        return Ok(());
    }

    let access = importer::check_access()?;
    ensure_full_photos_access(&access, "retry-missing verification")?;

    let mut total_reimported = 0usize;
    let mut total_retry_failed = 0usize;
    let mut total_missing_unresolved = 0usize;

    for manifest_path in &manifests {
        let manifest = match manifest::read_manifest_strict(manifest_path) {
            Ok(Some(m)) => m,
            Ok(None) => {
                display::print_warning(&format!("Could not read {:?}", manifest_path));
                continue;
            }
            Err(e) => {
                return Err(e.context(format!(
                    "Refusing retry-missing with corrupt manifest {}",
                    manifest_path.display()
                )));
            }
        };
        if manifest.imported.is_empty() {
            continue;
        }

        let ids: Vec<&str> = manifest
            .imported
            .iter()
            .map(|e| e.local_id.as_str())
            .collect();
        let results = importer::verify_assets(&ids)?;
        let result_map: HashMap<&str, &importer::AssetVerifyResult> = results
            .iter()
            .map(|r| (r.local_identifier.as_str(), r))
            .collect();
        let retry_entries: Vec<&manifest::ManifestEntry> = manifest
            .imported
            .iter()
            .filter(|entry| match result_map.get(entry.local_id.as_str()) {
                None | Some(importer::AssetVerifyResult { found: false, .. }) => true,
                Some(result) => {
                    if entry.is_live_photo == Some(true) && !result.has_paired_video {
                        return true;
                    }
                    date_mismatch(entry.creation_date.as_deref(), result.creation_date.as_deref())
                }
            })
            .collect();

        if retry_entries.is_empty() {
            display::print_info(&format!("{}: no retry-needed assets", manifest.zip));
            continue;
        }

        let zip_path = dir.join(&manifest.zip);
        if !zip_path.exists() {
            display::print_warning(&format!(
                "{}: {} missing assets but zip not found at {}",
                manifest.zip,
                retry_entries.len(),
                zip_path.display()
            ));
            total_missing_unresolved += retry_entries.len();
            continue;
        }

        display::print_header(&format!(
            "{}: retrying {} assets",
            manifest.zip,
            retry_entries.len()
        ));

        let extract_dir = dir.join(format!(
            ".photoferry-retry-extract-{}",
            zip_path.file_stem().unwrap_or_default().to_string_lossy()
        ));
        if extract_dir.exists() {
            std::fs::remove_dir_all(&extract_dir)?;
        }
        std::fs::create_dir_all(&extract_dir)?;

        let content_root = match takeout::extract_zip(&zip_path, &extract_dir) {
            Ok(root) => root,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&extract_dir);
                return Err(e.context(format!("Failed to extract {}", zip_path.display())));
            }
        };
        let inventory = match takeout::scan_directory(&content_root, &takeout::ScanOptions::default()) {
            Ok(inv) => inv,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&extract_dir);
                return Err(e.context(format!(
                    "Failed to scan extracted content for {}",
                    zip_path.display()
                )));
            }
        };

        let mut by_relative: HashMap<String, takeout::MediaFile> = HashMap::new();
        for file in &inventory.files {
            let rel = file
                .path
                .strip_prefix(&content_root)
                .unwrap_or(&file.path)
                .to_string_lossy()
                .to_string();
            by_relative.insert(rel, file.clone());
        }

        let mut retry_files = Vec::new();
        let mut unresolved = 0usize;
        for entry in &retry_entries {
            if let Some(file) = by_relative.get(&entry.path) {
                retry_files.push(file.clone());
            } else {
                display::print_warning(&format!(
                    "Missing in zip content (cannot retry): {}",
                    entry.path
                ));
                unresolved += 1;
            }
        }

        if retry_files.is_empty() {
            total_missing_unresolved += retry_entries.len();
            let _ = std::fs::remove_dir_all(&extract_dir);
            continue;
        }

        let retry_albums: Vec<String> = retry_files
            .iter()
            .filter_map(|f| f.album.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let retry_inventory = takeout::TakeoutInventory {
            files: retry_files,
            albums: retry_albums,
            stats: Default::default(),
        };

        let summary = import_inventory(&retry_inventory, verbose);
        print_import_summary(&summary);

        let new_imported: Vec<(String, String, Option<String>, bool)> = summary
            .imported
            .iter()
            .map(|file| {
                (
                    file.path
                        .strip_prefix(&content_root)
                        .unwrap_or(&file.path)
                        .to_string_lossy()
                        .to_string(),
                    file.local_id.clone(),
                    file.creation_date.clone(),
                    file.is_live_photo,
                )
            })
            .collect();
        let new_failed: Vec<(String, String)> = summary
            .failed
            .iter()
            .map(|file| {
                let p = std::path::Path::new(&file.path);
                (
                    p.strip_prefix(&content_root)
                        .unwrap_or(p)
                        .to_string_lossy()
                        .to_string(),
                    file.error.clone(),
                )
            })
            .collect();
        manifest::merge_and_write(
            manifest_path,
            &manifest.zip,
            &new_imported,
            &new_failed,
            &[],
        )?;

        total_reimported += summary.imported.len();
        total_retry_failed += summary.failed.len();
        total_missing_unresolved += unresolved;
        std::fs::remove_dir_all(&extract_dir)?;
    }

    println!();
    display::print_header("Retry missing summary");
    display::print_info(&format!("Re-imported: {}", total_reimported));
    if total_retry_failed > 0 {
        display::print_warning(&format!("Retry import failures: {}", total_retry_failed));
    }
    if total_missing_unresolved > 0 {
        display::print_warning(&format!(
            "Still unresolved missing assets: {}",
            total_missing_unresolved
        ));
    }

    Ok(())
}

fn cmd_retry_live_photo_fallbacks(dir: &Path, verbose: bool) -> Result<()> {
    let dir = expand_tilde(dir);
    display::print_header(&format!(
        "Retrying Live Photo fallbacks in {}",
        dir.display()
    ));

    let manifests: Vec<PathBuf> = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with(".photoferry-manifest-") && n.ends_with(".json"))
                .unwrap_or(false)
        })
        .collect();

    if manifests.is_empty() {
        display::print_info("No manifests found.");
        return Ok(());
    }

    let access = importer::check_access()?;
    ensure_full_photos_access(&access, "retry-live-photo-fallbacks")?;

    let mut total_reimported = 0usize;
    let mut total_failed = 0usize;
    let mut total_unresolved = 0usize;

    for manifest_path in &manifests {
        let mut manifest = match manifest::read_manifest_strict(manifest_path) {
            Ok(Some(m)) => m,
            Ok(None) => {
                display::print_warning(&format!("Could not read {:?}", manifest_path));
                continue;
            }
            Err(e) => {
                return Err(e.context(format!(
                    "Refusing retry-live-photo-fallbacks with corrupt manifest {}",
                    manifest_path.display()
                )));
            }
        };

        if manifest.live_photo_fallbacks.is_empty() {
            continue;
        }

        let zip_path = dir.join(&manifest.zip);
        if !zip_path.exists() {
            display::print_warning(&format!(
                "{}: {} live photo fallbacks but zip not found at {}",
                manifest.zip,
                manifest.live_photo_fallbacks.len(),
                zip_path.display()
            ));
            total_unresolved += manifest.live_photo_fallbacks.len();
            continue;
        }

        display::print_header(&format!(
            "{}: retrying {} live photo fallbacks",
            manifest.zip,
            manifest.live_photo_fallbacks.len()
        ));

        let extract_dir = dir.join(format!(
            ".photoferry-live-retry-extract-{}",
            zip_path.file_stem().unwrap_or_default().to_string_lossy()
        ));
        if extract_dir.exists() {
            std::fs::remove_dir_all(&extract_dir)?;
        }
        std::fs::create_dir_all(&extract_dir)?;

        let content_root = match takeout::extract_zip(&zip_path, &extract_dir) {
            Ok(root) => root,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&extract_dir);
                return Err(e.context(format!("Failed to extract {}", zip_path.display())));
            }
        };
        let inventory = match takeout::scan_directory(&content_root, &takeout::ScanOptions::default()) {
            Ok(inv) => inv,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&extract_dir);
                return Err(e.context(format!(
                    "Failed to scan extracted content for {}",
                    zip_path.display()
                )));
            }
        };

        let mut by_relative: HashMap<String, takeout::MediaFile> = HashMap::new();
        for file in &inventory.files {
            let rel = file
                .path
                .strip_prefix(&content_root)
                .unwrap_or(&file.path)
                .to_string_lossy()
                .to_string();
            by_relative.insert(rel, file.clone());
        }

        let mut resolved_paths = HashSet::new();
        let mut updated_imports: HashMap<String, String> = HashMap::new();

        for fallback in &manifest.live_photo_fallbacks {
            let Some(photo_file) = by_relative.get(&fallback.photo_path) else {
                display::print_warning(&format!(
                    "Missing photo in zip content: {}",
                    fallback.photo_path
                ));
                total_unresolved += 1;
                continue;
            };
            let video_abs = content_root.join(&fallback.video_path);
            if !video_abs.exists() {
                display::print_warning(&format!(
                    "Missing video in zip content: {}",
                    fallback.video_path
                ));
                total_unresolved += 1;
                continue;
            }

            let photo_abs = &photo_file.path;
            let import_result = importer::import_live_photo(
                photo_abs.to_str().unwrap_or_default(),
                video_abs.to_str().unwrap_or_default(),
                photo_file.metadata.as_ref(),
            );

            match import_result {
                Ok(result) if result.success => {
                    total_reimported += 1;
                    resolved_paths.insert(fallback.photo_path.clone());
                    if let Some(local_id) = result.local_identifier {
                        updated_imports.insert(fallback.photo_path.clone(), local_id);
                    }
                    if verbose {
                        display::print_success(&format!(
                            "Re-imported Live Photo: {}",
                            fallback.photo_path
                        ));
                    }
                }
                Ok(result) => {
                    total_failed += 1;
                    let err = result.error.unwrap_or_else(|| "unknown error".to_string());
                    display::print_warning(&format!(
                        "Live Photo retry failed: {} — {}",
                        fallback.photo_path, err
                    ));
                }
                Err(err) => {
                    total_failed += 1;
                    display::print_warning(&format!(
                        "Live Photo retry error: {} — {}",
                        fallback.photo_path, err
                    ));
                }
            }
        }

        if !resolved_paths.is_empty() {
            // Update manifest: remove resolved fallbacks and update imported entry to live photo
            manifest.live_photo_fallbacks.retain(|f| !resolved_paths.contains(&f.photo_path));
            for entry in &mut manifest.imported {
                if let Some(new_id) = updated_imports.get(&entry.path) {
                    entry.local_id = new_id.clone();
                    entry.is_live_photo = Some(true);
                }
            }
            // Write updated manifest
            let imported: Vec<(String, String, Option<String>, bool)> = manifest
                .imported
                .iter()
                .map(|e| {
                    (
                        e.path.clone(),
                        e.local_id.clone(),
                        e.creation_date.clone(),
                        e.is_live_photo.unwrap_or(false),
                    )
                })
                .collect();
            let failed: Vec<(String, String)> = manifest
                .failed
                .iter()
                .map(|e| (e.path.clone(), e.error.clone()))
                .collect();
            let live_photo_fallbacks: Vec<(String, String, String)> = manifest
                .live_photo_fallbacks
                .iter()
                .map(|e| (e.photo_path.clone(), e.video_path.clone(), e.local_id.clone()))
                .collect();
            manifest::write_manifest(manifest_path, &manifest.zip, &imported, &failed, &live_photo_fallbacks)?;

            if !updated_imports.is_empty() {
                display::print_warning(
                    "Live Photo retries create new assets; check Photos.app for duplicates.",
                );
            }
        }

        std::fs::remove_dir_all(&extract_dir)?;
    }

    println!();
    display::print_header("Retry Live Photo fallbacks summary");
    display::print_info(&format!("Re-imported: {}", total_reimported));
    if total_failed > 0 {
        display::print_warning(&format!("Retry failures: {}", total_failed));
    }
    if total_unresolved > 0 {
        display::print_warning(&format!(
            "Unresolved fallbacks (missing in zip): {}",
            total_unresolved
        ));
    }

    Ok(())
}

fn dates_match(a: &str, b: &str) -> bool {
    let parsed_a = chrono::DateTime::parse_from_rfc3339(a)
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc));
    let parsed_b = chrono::DateTime::parse_from_rfc3339(b)
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc));
    match (parsed_a, parsed_b) {
        (Some(da), Some(db)) => da == db,
        _ => a.trim() == b.trim(),
    }
}

fn date_mismatch(expected: Option<&str>, actual: Option<&str>) -> bool {
    match expected {
        None => false,
        Some(expected_value) => match actual {
            Some(actual_value) => !dates_match(expected_value, actual_value),
            None => true,
        },
    }
}

/// Returns available disk space in GB for the filesystem containing `path`.
/// Uses `df -k` — returns None if the command fails or output is unparseable.
fn available_space_gb(path: &Path) -> Option<u64> {
    let output = Command::new("df").arg("-k").arg(path).output().ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Second line is the data row: Filesystem 1K-blocks Used Available Capacity ...
    let data_line = stdout.lines().nth(1)?;
    let fields: Vec<&str> = data_line.split_whitespace().collect();
    let avail_kb: u64 = fields.get(3)?.parse().ok()?;
    Some(avail_kb / (1024 * 1024)) // KB → GB
}

/// Batch-verify all assets recorded in a zip's manifest exist in Photos Library.
/// Returns true if all present (safe to delete zip), false if any missing.
fn verify_zip_manifest(zip_path: &Path, manifest_dir: &Path) -> bool {
    let zip_stem = zip_path.file_stem().unwrap_or_default().to_string_lossy();
    let manifest_path = manifest_dir.join(format!(".photoferry-manifest-{}.json", zip_stem));
    let manifest = match manifest::read_manifest_strict(&manifest_path) {
        Ok(Some(m)) => m,
        Ok(None) => {
            display::print_warning("  Verify: manifest missing — refusing to delete zip");
            return false;
        }
        Err(e) => {
            display::print_warning(&format!(
                "  Verify: manifest unreadable/corrupt ({e}) — refusing to delete zip"
            ));
            return false;
        }
    };
    if manifest.imported.is_empty() {
        if !manifest.failed.is_empty() {
            display::print_warning(&format!(
                "  Verify: {} failed imports — keeping zip",
                manifest.failed.len()
            ));
            return false;
        }
        return true;
    }
    let ids: Vec<&str> = manifest
        .imported
        .iter()
        .map(|e| e.local_id.as_str())
        .collect();
    match importer::verify_assets(&ids) {
        Ok(results) => {
            let result_map: HashMap<&str, &importer::AssetVerifyResult> = results
                .iter()
                .map(|r| (r.local_identifier.as_str(), r))
                .collect();
            let mut missing = 0usize;
            let mut wrong_date = 0usize;
            let mut live_pair_missing = 0usize;
            let mut confirmed = 0usize;
            for entry in &manifest.imported {
                let Some(result) = result_map.get(entry.local_id.as_str()) else {
                    missing += 1;
                    continue;
                };
                if !result.found {
                    missing += 1;
                    continue;
                }
                if entry.is_live_photo == Some(true) && !result.has_paired_video {
                    live_pair_missing += 1;
                    continue;
                }
                if date_mismatch(entry.creation_date.as_deref(), result.creation_date.as_deref()) {
                    wrong_date += 1;
                    continue;
                }
                confirmed += 1;
            }
            if missing > 0 || live_pair_missing > 0 || wrong_date > 0 {
                display::print_warning(&format!(
                    "  Verify: {}/{} confirmed — {} missing, {} wrong date, {} live pair missing; keeping zip",
                    confirmed,
                    manifest.imported.len(),
                    missing,
                    wrong_date,
                    live_pair_missing
                ));
                false
            } else {
                display::print_success(&format!(
                    "  Verify: all {} assets confirmed in Photos Library",
                    confirmed
                ));
                true
            }
        }
        Err(e) => {
            display::print_warning(&format!("  Verify error: {e} — keeping zip as precaution"));
            false
        }
    }
}

fn live_photo_paths_from_zip(zip_path: &Path, manifest_dir: &Path) -> Result<HashSet<String>> {
    let zip_stem = zip_path.file_stem().unwrap_or_default().to_string_lossy();
    let extract_dir = manifest_dir.join(format!(
        ".photoferry-verify-extract-{}",
        zip_stem
    ));
    if extract_dir.exists() {
        std::fs::remove_dir_all(&extract_dir)?;
    }
    std::fs::create_dir_all(&extract_dir)?;

    let result = (|| -> Result<HashSet<String>> {
        let content_root = takeout::extract_zip(zip_path, &extract_dir)?;
        let inventory = takeout::scan_directory(&content_root, &takeout::ScanOptions::default())?;

        let mut live_paths = HashSet::new();
        for file in &inventory.files {
            if file.live_photo_pair.is_some() {
                let rel = file
                    .path
                    .strip_prefix(&content_root)
                    .unwrap_or(&file.path)
                    .to_string_lossy()
                    .to_string();
                live_paths.insert(rel);
            }
        }
        Ok(live_paths)
    })();

    let _ = std::fs::remove_dir_all(&extract_dir);
    result
}

fn ensure_full_photos_access(access: &importer::AccessResult, action: &str) -> Result<()> {
    if !access.authorized {
        bail!(
            "Photos access: {} — grant in System Settings > Privacy & Security > Photos",
            access.status
        );
    }
    if access.status == "limited" {
        bail!(
            "Photos access is limited — {} requires full library access for reliable results",
            action
        );
    }
    Ok(())
}

fn expand_tilde(path: &Path) -> PathBuf {
    if let Some(rest) = path.to_str().and_then(|s: &str| s.strip_prefix("~/"))
        && let Ok(home) = std::env::var("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::{VerifySuccessAction, date_mismatch, dates_match, verify_success_action};

    #[test]
    fn dates_match_normalizes_timezone() {
        assert!(dates_match(
            "2026-02-22T10:00:00+08:00",
            "2026-02-22T02:00:00Z"
        ));
    }

    #[test]
    fn dates_match_detects_real_difference() {
        assert!(!dates_match("2026-02-22T10:00:00Z", "2026-02-22T10:00:01Z"));
    }

    #[test]
    fn dates_match_falls_back_to_trimmed_string() {
        assert!(dates_match("not-a-date ", "not-a-date"));
    }

    #[test]
    fn date_mismatch_is_false_without_expected_date() {
        assert!(!date_mismatch(None, None));
        assert!(!date_mismatch(None, Some("2026-02-22T10:00:00Z")));
    }

    #[test]
    fn date_mismatch_is_true_when_expected_exists_but_actual_missing() {
        assert!(date_mismatch(Some("2026-02-22T10:00:00Z"), None));
    }

    #[test]
    fn date_mismatch_uses_dates_match_when_both_present() {
        assert!(!date_mismatch(
            Some("2026-02-22T10:00:00+08:00"),
            Some("2026-02-22T02:00:00Z")
        ));
        assert!(date_mismatch(
            Some("2026-02-22T10:00:00Z"),
            Some("2026-02-22T10:00:01Z")
        ));
    }

    #[test]
    fn verify_success_action_keeps_zip_but_marks_completed_without_icloud_confirmation() {
        assert_eq!(
            verify_success_action(false),
            VerifySuccessAction::KeepZipAndMarkCompleted
        );
    }

    #[test]
    fn verify_success_action_deletes_zip_when_icloud_is_confirmed() {
        assert_eq!(
            verify_success_action(true),
            VerifySuccessAction::DeleteZipAndMarkCompleted
        );
    }
}
