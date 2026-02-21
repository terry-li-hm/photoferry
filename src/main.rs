mod display;
mod importer;
mod manifest;
mod metadata;
mod sidecar;
mod takeout;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
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
        }) => cmd_run(&dir, once, dry_run, verbose)?,
        Some(Commands::Import { file, metadata }) => cmd_import(&file, metadata.as_deref())?,
        Some(Commands::Albums { dir }) => cmd_albums(&dir)?,
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

fn cmd_run(dir: &PathBuf, once: bool, dry_run: bool, verbose: bool) -> Result<()> {
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

        // Extract to a temp directory alongside the zip
        let extract_dir = dir.join(format!(
            ".photoferry-extract-{}",
            zip_path.file_stem().unwrap_or_default().to_string_lossy()
        ));
        std::fs::create_dir_all(&extract_dir)?;

        let zip_stem = zip_path.file_stem().unwrap_or_default().to_string_lossy();
        let manifest_path = dir.join(format!(".photoferry-manifest-{}.json", zip_stem));

        let content_root = takeout::extract_zip(zip_path, &extract_dir)?;
        let mut inventory = takeout::scan_directory(&content_root)?;

        let existing_manifest = manifest::read_manifest(&manifest_path);
        let already_imported: HashSet<String> = existing_manifest
            .as_ref()
            .map(|m| m.imported.iter().map(|e| e.path.clone()).collect())
            .unwrap_or_default();
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

        print_inventory_summary(&inventory);

        if !dry_run {
            let summary = import_inventory(&inventory, verbose);
            print_import_summary(&summary);
            total_summary.merge(&summary);

            let mut manifest_imported = existing_manifest
                .as_ref()
                .map(|m| {
                    m.imported
                        .iter()
                        .map(|entry| (entry.path.clone(), entry.local_id.clone()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            manifest_imported.extend(summary.imported.iter().map(|file| {
                (
                    file.path
                        .strip_prefix(&content_root)
                        .unwrap_or(&file.path)
                        .to_string_lossy()
                        .to_string(),
                    file.local_id.clone(),
                )
            }));

            let mut manifest_failed = existing_manifest
                .as_ref()
                .map(|m| {
                    m.failed
                        .iter()
                        .map(|entry| (entry.path.clone(), entry.error.clone()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            manifest_failed.extend(summary.failed.iter().map(|file| {
                let file_path = Path::new(&file.path);
                (
                    file_path
                        .strip_prefix(&content_root)
                        .unwrap_or(file_path)
                        .to_string_lossy()
                        .to_string(),
                    file.error.clone(),
                )
            }));

            manifest::write_manifest(
                &manifest_path,
                &zip_path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy(),
                &manifest_imported,
                &manifest_failed,
            )?;
        }

        // Clean up extraction directory
        std::fs::remove_dir_all(&extract_dir)?;
    }

    // Print totals if multiple zips processed
    if !dry_run && zips_to_process.len() > 1 {
        println!();
        display::print_header("Total across all zips");
        print_import_summary(&total_summary);
    }

    Ok(())
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
        let style = ProgressStyle::with_template("[{bar:40}] {pos}/{len} {msg}")
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
                    display::print_error(&format!(
                        "[{}/{}] {} — {}",
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
                });

                if let Some(album_name) = file.album.as_ref() {
                    if let Some(album_id) = album_ids.get(album_name) {
                        if let Some(actual_local_id) = result.local_identifier.as_deref() {
                            match importer::add_to_album(album_id, actual_local_id) {
                                Ok(true) => {}
                                Ok(false) => {
                                    display::print_warning(&format!(
                                        "Failed to add '{}' to album '{}'",
                                        filename, album_name
                                    ));
                                }
                                Err(err) => {
                                    display::print_warning(&format!(
                                        "Failed to add '{}' to album '{}': {}",
                                        filename, album_name, err
                                    ));
                                }
                            }
                        } else {
                            display::print_warning(&format!(
                                "No local identifier for '{}'; skipping album assignment",
                                filename
                            ));
                        }
                    }
                }

                if verbose {
                    display::print_success(&format!(
                        "[{}/{}] {} -> {}",
                        index + 1,
                        total,
                        filename,
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
                    display::print_error(&format!(
                        "[{}/{}] {} — {}",
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
                    display::print_error(&format!(
                        "[{}/{}] {} — {}",
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

fn expand_tilde(path: &Path) -> PathBuf {
    if let Some(rest) = path.to_str().and_then(|s: &str| s.strip_prefix("~/")) {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    path.to_path_buf()
}
