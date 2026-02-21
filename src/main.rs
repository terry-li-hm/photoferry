mod display;
mod downloader;
mod importer;
mod manifest;
mod metadata;
mod sidecar;
mod takeout;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

#[derive(Parser)]
#[command(name = "photoferry", version, about = "Google Photos → iCloud migration")]
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
        /// Retry only files that previously failed in manifest
        #[arg(long)]
        retry_failed: bool,
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
        /// Print per-file import results
        #[arg(long)]
        verbose: bool,
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
            retry_failed,
        }) => cmd_run(&dir, once, dry_run, verbose, retry_failed)?,
        Some(Commands::Import { file, metadata }) => cmd_import(&file, metadata.as_deref())?,
        Some(Commands::Albums { dir }) => cmd_albums(&dir)?,
        Some(Commands::Verify { dir }) => cmd_verify(&dir)?,
        Some(Commands::Download {
            job,
            user,
            dir,
            start,
            end,
            download_only,
            verbose,
        }) => cmd_download(&job, &user, &dir, start, end, download_only, verbose)?,
    }

    Ok(())
}

fn cmd_check() -> Result<()> {
    display::print_header("Checking Photos.app access...");
    let result = importer::check_access()?;

    if result.authorized {
        display::print_success(&format!("Photos access: {} (authorized)", result.status));
    } else {
        display::print_error(&format!(
            "Photos access: {} — grant in System Settings > Privacy & Security > Photos",
            result.status
        ));
    }

    Ok(())
}

fn cmd_run(
    dir: &PathBuf,
    once: bool,
    dry_run: bool,
    verbose: bool,
    retry_failed: bool,
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
        if !access.authorized {
            bail!(
                "Photos access: {} — grant in System Settings > Privacy & Security > Photos",
                access.status
            );
        }
        display::print_success(&format!("Photos access: {} (authorized)", access.status));
    }

    let mut total_summary = ImportSummary::default();

    for zip_path in zips_to_process {
        display::print_header(&format!(
            "Processing {}",
            zip_path.file_name().unwrap_or_default().to_string_lossy()
        ));
        match process_one_zip(zip_path, &dir, dry_run, verbose, retry_failed) {
            Ok(summary) => {
                print_import_summary(&summary);
                total_summary.merge(&summary);
            }
            Err(e) => {
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

/// Extract, scan, import, and write manifest for a single zip.
/// Returns ImportSummary (empty if dry_run).
fn process_one_zip(
    zip_path: &Path,
    manifest_dir: &Path,
    dry_run: bool,
    verbose: bool,
    retry_failed: bool,
) -> Result<ImportSummary> {
    let extract_dir = manifest_dir.join(format!(
        ".photoferry-extract-{}",
        zip_path.file_stem().unwrap_or_default().to_string_lossy()
    ));
    if extract_dir.exists() {
        display::print_info(&format!(
            "Cleaning stale extract dir: {}",
            extract_dir.display()
        ));
        std::fs::remove_dir_all(&extract_dir)?;
    }
    std::fs::create_dir_all(&extract_dir)?;

    let zip_stem = zip_path.file_stem().unwrap_or_default().to_string_lossy();
    let manifest_path = manifest_dir.join(format!(".photoferry-manifest-{}.json", zip_stem));

    let content_root = match takeout::extract_zip(zip_path, &extract_dir) {
        Ok(root) => root,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&extract_dir);
            return Err(e.context("Failed to extract zip"));
        }
    };
    let mut inventory = takeout::scan_directory(&content_root)?;

    let existing_manifest = manifest::read_manifest(&manifest_path);
    let already_imported: HashSet<String> = existing_manifest
        .as_ref()
        .map(|m| m.imported.iter().map(|e| e.path.clone()).collect())
        .unwrap_or_default();
    if dry_run && !already_imported.is_empty() {
        display::print_info(&format!(
            "{} already imported (skipping)",
            already_imported.len()
        ));
    }
    if !already_imported.is_empty() {
        inventory.files.retain(|file| {
            let relative = file
                .path
                .strip_prefix(&content_root)
                .unwrap_or(&file.path)
                .to_string_lossy()
                .to_string();
            !already_imported.contains(&relative)
        });
    }
    if retry_failed {
        let failed_paths: HashSet<String> = existing_manifest
            .as_ref()
            .map(|m| m.failed.iter().map(|e| e.path.clone()).collect())
            .unwrap_or_default();
        if failed_paths.is_empty() {
            display::print_info("No previously-failed files to retry.");
            let _ = std::fs::remove_dir_all(&extract_dir);
            return Ok(ImportSummary::default());
        }
        display::print_info(&format!(
            "Retrying {} previously-failed files",
            failed_paths.len()
        ));
        inventory.files.retain(|file| {
            let relative = file
                .path
                .strip_prefix(&content_root)
                .unwrap_or(&file.path)
                .to_string_lossy()
                .to_string();
            failed_paths.contains(&relative)
        });
    }

    print_inventory_summary(&inventory);

    if dry_run {
        let _ = std::fs::remove_dir_all(&extract_dir);
        return Ok(ImportSummary::default());
    }

    let summary = import_inventory(&inventory, verbose);

    let new_imported: Vec<(String, String, Option<String>)> = summary
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
        &manifest_path,
        &zip_path.file_name().unwrap_or_default().to_string_lossy(),
        &new_imported,
        &new_failed,
    )?;

    std::fs::remove_dir_all(&extract_dir)?;
    Ok(summary)
}

fn cmd_import(file: &PathBuf, metadata_json: Option<&str>) -> Result<()> {
    let path = file
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Invalid file path"))?;

    display::print_header(&format!("Importing {}", file.display()));

    let metadata = match metadata_json {
        Some(json) => Some(serde_json::from_str::<importer::PhotoMetadata>(json)?),
        None => None,
    };

    let result = importer::import_photo(path, metadata.as_ref())?;

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

fn cmd_albums(dir: &PathBuf) -> Result<()> {
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

        let content_root = takeout::extract_zip(zip_path, &extract_dir)?;
        let inventory = takeout::scan_directory(&content_root)?;
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
    dir: &PathBuf,
    start: usize,
    end: usize,
    download_only: bool,
    verbose: bool,
) -> Result<()> {
    let dir = expand_tilde(dir);
    std::fs::create_dir_all(&dir)?;

    display::print_header(&format!(
        "Downloading Takeout parts {start}–{end} → {}",
        dir.display()
    ));

    // Check Photos access up front (unless download-only)
    if !download_only {
        let access = importer::check_access()?;
        if !access.authorized {
            bail!(
                "Photos access: {} — grant in System Settings > Privacy & Security > Photos",
                access.status
            );
        }
        display::print_success(&format!("Photos access: {} (authorized)", access.status));
    }

    // Load or create download progress manifest
    let mut progress = downloader::DownloadProgress::load(&dir, job_id);
    progress.user_id = user_id.to_string();
    progress.save(&dir)?;

    display::print_info("Extracting Chrome cookies...");
    let cookies = downloader::get_chrome_cookies()
        .context("Failed to get Chrome cookies — ensure Chrome is running and logged into Google")?;
    display::print_success(&format!("Got {} Google cookies", cookies.len()));

    let client = downloader::build_client(&cookies)?;

    let mut total_imported = 0usize;
    let mut total_failed_dl = 0usize;
    let mut total_failed_import = 0usize;

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

        // Download
        let zip_path = match downloader::download_zip(&client, job_id, user_id, i, &dir) {
            Ok(p) => p,
            Err(e) => {
                display::print_error(&format!("  [{i:02}] Download failed: {e}"));
                // Retry once after 10s
                std::thread::sleep(std::time::Duration::from_secs(10));
                match downloader::download_zip(&client, job_id, user_id, i, &dir) {
                    Ok(p) => p,
                    Err(e2) => {
                        display::print_error(&format!(
                            "  [{i:02}] Retry failed: {e2} — skipping"
                        ));
                        progress.mark_failed(i, &dir);
                        total_failed_dl += 1;
                        continue;
                    }
                }
            }
        };

        if download_only {
            display::print_success(&format!(
                "  [{i:02}] Downloaded → {}",
                zip_path.display()
            ));
            progress.mark_completed(i, &dir);
            total_imported += 1;
            continue;
        }

        // Import
        display::print_info(&format!(
            "  [{i:02}] Importing {}...",
            zip_path.file_name().unwrap_or_default().to_string_lossy()
        ));
        match process_one_zip(&zip_path, &dir, false, verbose, false) {
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
                        progress.mark_completed(i, &dir);
                    } else {
                        display::print_warning(&format!(
                            "  [{i:02}] Import OK but verify failed — keeping zip"
                        ));
                        // Don't mark completed — verify on next run
                    }
                }
            }
            Err(e) => {
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

fn print_inventory_summary(inventory: &takeout::TakeoutInventory) {
    let s = &inventory.stats;
    display::print_info(&format!("Photos: {}", s.photos));
    display::print_info(&format!("Videos: {}", s.videos));
    display::print_info(&format!(
        "Sidecar matched: {} / unmatched: {}",
        s.with_sidecar, s.without_sidecar
    ));
    if s.live_photo_pairs > 0 {
        display::print_info(&format!("Live Photo pairs: {}", s.live_photo_pairs));
    }
    if s.trashed_skipped > 0 {
        display::print_info(&format!("Trashed (skipped): {}", s.trashed_skipped));
    }
    if s.duplicates_skipped > 0 {
        display::print_info(&format!("Duplicates (skipped): {}", s.duplicates_skipped));
    }
    if !inventory.albums.is_empty() {
        display::print_info(&format!("Albums: {}", inventory.albums.join(", ")));
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
}

#[derive(Debug, Default)]
struct ImportSummary {
    imported: Vec<ImportedFile>,
    failed: Vec<ImportFailure>,
    elapsed: std::time::Duration,
}

impl ImportSummary {
    fn merge(&mut self, other: &ImportSummary) {
        self.imported
            .extend(other.imported.iter().map(|file| ImportedFile {
                path: file.path.clone(),
                local_id: file.local_id.clone(),
                album: file.album.clone(),
                creation_date: file.creation_date.clone(),
            }));
        self.failed.extend(other.failed.iter().map(|f| ImportFailure {
            path: f.path.clone(),
            error: f.error.clone(),
        }));
        self.elapsed += other.elapsed;
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
        let style = ProgressStyle::with_template(
            "[{bar:40}] {pos}/{len} {per_sec:.1}/s ETA {eta} {msg}",
        )
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

        let import_result = if let Some(ref video_path) = file.live_photo_pair {
            match video_path.to_str() {
                Some(video_str) => {
                    importer::import_live_photo(path, video_str, file.metadata.as_ref())
                }
                None => Err(anyhow::anyhow!("Invalid UTF-8 in Live Photo video path")),
            }
        } else {
            importer::import_photo(path, file.metadata.as_ref())
        };

        match import_result {
            Ok(result) if result.success => {
                let local_id = result
                    .local_identifier
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string());
                summary.imported.push(ImportedFile {
                    path: file.path.clone(),
                    local_id: local_id.clone(),
                    album: file.album.clone(),
                    creation_date: file.metadata.as_ref().and_then(|m| m.creation_date.clone()),
                });

                if let Some(album_name) = file.album.as_ref() {
                    if let Some(album_id) = album_ids.get(album_name) {
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

    if !summary.failed.is_empty() {
        display::print_warning("Failed files:");
        for failed in &summary.failed {
            display::print_error(&format!("{} — {}", failed.path, failed.error));
        }
    }
}

fn cmd_verify(dir: &PathBuf) -> Result<()> {
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
    if !access.authorized {
        bail!("Photos access not authorized");
    }

    let mut total_verified = 0usize;
    let mut total_missing = 0usize;
    let mut total_wrong_date = 0usize;

    for manifest_path in &manifests {
        let manifest = match manifest::read_manifest(manifest_path) {
            Some(m) => m,
            None => {
                display::print_warning(&format!("Could not read {:?}", manifest_path));
                continue;
            }
        };

        display::print_header(&format!("Verifying {}", manifest.zip));
        display::print_info(&format!(
            "Checking {} imported assets...",
            manifest.imported.len()
        ));

        let ids: Vec<&str> = manifest.imported.iter().map(|e| e.local_id.as_str()).collect();
        let results = importer::verify_assets(&ids)?;

        let result_map: HashMap<&str, &importer::AssetVerifyResult> =
            results.iter().map(|r| (r.local_identifier.as_str(), r)).collect();

        let mut missing = vec![];
        let mut wrong_date = vec![];

        for entry in &manifest.imported {
            match result_map.get(entry.local_id.as_str()) {
                None | Some(importer::AssetVerifyResult { found: false, .. }) => {
                    missing.push(entry);
                }
                Some(result) => {
                    total_verified += 1;
                    if let (Some(expected), Some(actual)) =
                        (&entry.creation_date, &result.creation_date)
                    {
                        if !dates_match(expected, actual) {
                            wrong_date.push((entry, actual.clone()));
                        }
                    }
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
    if total_missing > 0 {
        display::print_error(&format!("Missing: {}", total_missing));
    }
    if total_wrong_date > 0 {
        display::print_warning(&format!("Wrong date: {}", total_wrong_date));
    }
    if total_missing == 0 && total_wrong_date == 0 {
        display::print_success("All assets verified successfully");
    }

    Ok(())
}

fn dates_match(a: &str, b: &str) -> bool {
    // Compare first 19 chars (YYYY-MM-DDTHH:MM:SS) — ignore timezone/subsecond
    a.len() >= 19 && b.len() >= 19 && a[..19] == b[..19]
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
    let manifest = match manifest::read_manifest(&manifest_path) {
        Some(m) => m,
        None => {
            display::print_warning("  Verify: manifest missing — refusing to delete zip");
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
    let ids: Vec<&str> = manifest.imported.iter().map(|e| e.local_id.as_str()).collect();
    match importer::verify_assets(&ids) {
        Ok(results) => {
            let missing = results.iter().filter(|r| !r.found).count();
            if missing > 0 {
                display::print_warning(&format!(
                    "  Verify: {}/{} confirmed — {} missing, keeping zip",
                    results.len() - missing,
                    results.len(),
                    missing
                ));
                false
            } else {
                display::print_success(&format!(
                    "  Verify: all {} assets confirmed in Photos Library",
                    results.len()
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

fn expand_tilde(path: &Path) -> PathBuf {
    if let Some(rest) = path.to_str().and_then(|s: &str| s.strip_prefix("~/")) {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    path.to_path_buf()
}
