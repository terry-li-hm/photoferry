use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use reqwest::StatusCode;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::notify::{self, Notifier};

// MARK: - Parallel download types

/// Events sent from download worker threads to the main thread.
pub enum DownloadEvent {
    Completed {
        part: usize,
        zip_path: PathBuf,
        duration: Duration,
        size: u64,
    },
    Failed {
        part: usize,
        error: String,
    },
}

/// Gate that blocks until sufficient disk space is available.
pub struct DiskSpaceGate {
    dir: PathBuf,
    min_free_gb: u64,
}

impl DiskSpaceGate {
    pub fn new(dir: PathBuf, min_free_gb: u64) -> Self {
        Self { dir, min_free_gb }
    }

    /// Block until at least `min_free_gb` GB are free. Polls every 30s.
    pub fn wait(&self, part: usize) {
        loop {
            match available_space_gb(&self.dir) {
                Some(gb) if gb >= self.min_free_gb => return,
                Some(gb) => {
                    println!(
                        "  [{part:02}] Low disk: {gb}GB free (need {}GB) — waiting 30s",
                        self.min_free_gb
                    );
                    std::thread::sleep(Duration::from_secs(30));
                }
                None => return, // Can't check — proceed anyway
            }
        }
    }
}

/// Returns available disk space in GB for the filesystem containing `path`.
/// Uses `df -k` — returns None if the command fails or output is unparseable.
pub fn available_space_gb(path: &Path) -> Option<u64> {
    let output = Command::new("df").arg("-k").arg(path).output().ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let data_line = stdout.lines().nth(1)?;
    let fields: Vec<&str> = data_line.split_whitespace().collect();
    let avail_kb: u64 = fields.get(3)?.parse().ok()?;
    Some(avail_kb / (1024 * 1024))
}

const COOKIES_SALT: &[u8] = b"saltysalt";
const COOKIES_ITERATIONS: u32 = 1003;
const COOKIES_KEY_LEN: usize = 16;

// MARK: - Download progress manifest

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct DownloadProgress {
    pub job_id: String,
    pub user_id: String,
    pub completed: Vec<usize>,
    pub failed: Vec<usize>,
}

impl DownloadProgress {
    pub fn load(dir: &Path, job_id: &str) -> Result<Self> {
        let path = progress_path(dir, job_id);
        match std::fs::read_to_string(&path) {
            Ok(data) => {
                let parsed = serde_json::from_str::<DownloadProgress>(&data).with_context(|| {
                    format!("Corrupt download progress JSON at {}", path.display())
                })?;
                Ok(parsed)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DownloadProgress {
                job_id: job_id.to_string(),
                user_id: String::new(),
                completed: Vec::new(),
                failed: Vec::new(),
            }),
            Err(e) => Err(e).with_context(|| format!("Failed to read {}", path.display())),
        }
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        let path = progress_path(dir, &self.job_id);
        std::fs::write(&path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn mark_completed(&mut self, i: usize, dir: &Path) {
        if !self.completed.contains(&i) {
            self.completed.push(i);
        }
        self.failed.retain(|&x| x != i);
        let _ = self.save(dir);
    }

    pub fn mark_failed(&mut self, i: usize, dir: &Path) {
        if !self.failed.contains(&i) {
            self.failed.push(i);
        }
        let _ = self.save(dir);
    }

    pub fn is_completed(&self, i: usize) -> bool {
        self.completed.contains(&i)
    }
}

fn progress_path(dir: &Path, job_id: &str) -> PathBuf {
    // Keep a readable prefix and add a hash to avoid collisions across jobs.
    let prefix: String = job_id.chars().take(8).collect();
    let mut hasher = Sha1::new();
    hasher.update(job_id.as_bytes());
    let digest = hasher.finalize();
    let hash = digest[..6]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    dir.join(format!(".photoferry-download-{prefix}-{hash}.json"))
}

// MARK: - Chrome cookie extraction

/// Extract Google cookies from Chrome on macOS using Keychain AES key.
pub fn get_chrome_cookies() -> Result<HashMap<String, String>> {
    let key = derive_aes_key()?;
    let cookies_db = find_chrome_cookies_db()?;

    // Copy DB to temp — Chrome may have a write lock on it
    let tmp = std::env::temp_dir().join("photoferry-cookies-tmp.db");
    std::fs::copy(&cookies_db, &tmp).context("Failed to copy Chrome cookies DB")?;

    let result = read_cookies(&tmp, &key);
    let _ = std::fs::remove_file(&tmp);
    result
}

fn derive_aes_key() -> Result<[u8; COOKIES_KEY_LEN]> {
    let output = Command::new("security")
        .args(["find-generic-password", "-s", "Chrome Safe Storage", "-w"])
        .output()
        .context("Failed to run `security` command")?;

    if !output.status.success() {
        bail!(
            "Could not get Chrome Safe Storage key from Keychain. \
             Ensure Chrome is installed and has been run at least once.\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let password =
        String::from_utf8(output.stdout).context("Chrome Safe Storage key is not valid UTF-8")?;
    let password = password.trim();

    let mut key = [0u8; COOKIES_KEY_LEN];
    pbkdf2::pbkdf2_hmac::<sha1::Sha1>(
        password.as_bytes(),
        COOKIES_SALT,
        COOKIES_ITERATIONS,
        &mut key,
    );
    Ok(key)
}

fn find_chrome_cookies_db() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    let candidates = [
        format!("{home}/Library/Application Support/Google/Chrome/Default/Network/Cookies"),
        format!("{home}/Library/Application Support/Google/Chrome/Default/Cookies"),
        format!("{home}/Library/Application Support/Google/Chrome Profile 1/Network/Cookies"),
        format!("{home}/Library/Application Support/Chromium/Default/Network/Cookies"),
    ];
    for path in &candidates {
        let p = PathBuf::from(path);
        if p.exists() {
            return Ok(p);
        }
    }
    bail!(
        "Chrome cookies database not found. Tried:\n{}",
        candidates.join("\n")
    )
}

fn read_cookies(db_path: &Path, key: &[u8; COOKIES_KEY_LEN]) -> Result<HashMap<String, String>> {
    let conn = Connection::open(db_path).context("Failed to open cookies DB")?;

    // Chrome 130+ (DB meta version ≥ 24) prefixes decrypted values with
    // SHA256(host_key). We must strip those 32 bytes to get the real value.
    // Note: Chrome stores the version as TEXT, not INTEGER.
    let db_version: i64 = conn
        .query_row("SELECT value FROM meta WHERE key='version'", [], |r| {
            r.get::<_, String>(0)
                .ok()
                .and_then(|s| s.parse::<i64>().ok())
                .map(Ok)
                .unwrap_or_else(|| r.get::<_, i64>(0))
        })
        .unwrap_or(0);

    // Match cookies for google.com and direct subdomains (same as pycookiecheat for takeout.google.com)
    // host_key values: '.google.com', 'takeout.google.com', 'google.com'
    let mut stmt = conn
        .prepare(
            "SELECT name, encrypted_value, host_key FROM cookies \
             WHERE host_key IN ('.google.com', 'google.com', 'takeout.google.com', \
             '.takeout.google.com', 'accounts.google.com', '.accounts.google.com')",
        )
        .context("Failed to query cookies")?;

    let mut cookies = HashMap::new();
    let mut rows = stmt.query([]).context("Failed to execute cookie query")?;

    while let Some(row) = rows.next().context("Error reading cookie row")? {
        let name: String = row.get(0)?;
        let encrypted: Vec<u8> = row.get(1)?;
        let host_key: String = row.get(2)?;
        if let Ok(value) = decrypt_cookie_value(&encrypted, key, db_version, &host_key)
            && !value.is_empty()
        {
            cookies.insert(name, value);
        }
    }

    Ok(cookies)
}

fn decrypt_cookie_value(
    encrypted: &[u8],
    key: &[u8; COOKIES_KEY_LEN],
    db_version: i64,
    host_key: &str,
) -> Result<String> {
    if encrypted.is_empty() {
        return Ok(String::new());
    }

    // Chrome v10 prefix: first 3 bytes are b"v10", rest is AES-128-CBC ciphertext
    if encrypted.len() >= 3 && &encrypted[..3] == b"v10" {
        let ciphertext = &encrypted[3..];
        let iv = [b' '; 16]; // 16 ASCII spaces

        use aes::Aes128;
        use cbc::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
        type Aes128CbcDec = cbc::Decryptor<Aes128>;

        let cipher = Aes128CbcDec::new_from_slices(key, &iv)
            .map_err(|e| anyhow::anyhow!("AES key/IV error: {e}"))?;
        let mut buf = ciphertext.to_vec();
        let decrypted = cipher
            .decrypt_padded_mut::<Pkcs7>(&mut buf)
            .map_err(|e| anyhow::anyhow!("AES decrypt error: {e}"))?;

        // Chrome 130+ (DB version ≥ 24): decrypted value is prefixed with
        // SHA256(host_key) — 32 bytes that must be stripped.
        let value_bytes = if db_version >= 24 && decrypted.len() > 32 {
            use sha2::Digest as Sha2Digest;
            let hash = sha2::Sha256::digest(host_key.as_bytes());
            if decrypted[..32] == hash[..] {
                &decrypted[32..]
            } else {
                // Hash doesn't match — might be a different prefix scheme.
                // Strip the 32-byte prefix anyway as it's certainly not cookie data.
                &decrypted[32..]
            }
        } else {
            decrypted
        };

        return Ok(String::from_utf8_lossy(value_bytes).into_owned());
    }

    // Unencrypted (older Chrome format)
    Ok(String::from_utf8_lossy(encrypted).into_owned())
}

// MARK: - HTTP download

fn build_url(job_id: &str, user_id: &str, i: usize) -> String {
    format!("https://takeout.google.com/takeout/download?j={job_id}&i={i}&user={user_id}")
}

pub fn build_client(cookies: &HashMap<String, String>) -> Result<Client> {
    // Build cookie header — skip any pairs that produce invalid header bytes
    let cookie_str: String = cookies
        .iter()
        .filter_map(|(k, v)| {
            let pair = format!("{k}={v}");
            // Check each pair individually — reject if it has control chars or non-ASCII
            if pair.bytes().all(|b| b >= 0x20 && b != 0x7f && b < 0x80) {
                Some(pair)
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("; ");

    let mut headers = reqwest::header::HeaderMap::new();
    if !cookie_str.is_empty()
        && let Ok(val) = cookie_str.parse::<reqwest::header::HeaderValue>()
    {
        headers.insert(reqwest::header::COOKIE, val);
    }

    Client::builder()
        .user_agent(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
             AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
        )
        .default_headers(headers)
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .context("Failed to build HTTP client")
}

/// Download Takeout part `i` to `dir`. Returns the local path.
/// Skips if file already exists and matches Content-Length.
/// Resumes partial downloads using Range header.
pub fn download_zip(
    client: &Client,
    job_id: &str,
    user_id: &str,
    i: usize,
    dir: &Path,
) -> Result<PathBuf> {
    use indicatif::{ProgressBar, ProgressStyle};

    let url = build_url(job_id, user_id, i);

    // HEAD to get filename + Content-Length
    let head = client.head(&url).send().context("HEAD request failed")?;

    if head.status().is_client_error() {
        bail!(
            "HEAD {} → {} (auth issue? re-run to refresh cookies)",
            url,
            head.status()
        );
    }

    // Detect auth redirect — Google returns 200 with text/html for login pages
    let content_type = head
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if content_type.contains("text/html") {
        bail!(
            "HEAD returned text/html (auth redirect to {}) — will fall back to Chrome",
            head.url()
        );
    }

    let filename = extract_filename(&head).unwrap_or_else(|| format!("takeout-part-{i:03}.zip"));
    let dest = dir.join(&filename);

    let content_length = head
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);

    // Skip if already fully downloaded
    if dest.exists() && content_length > 0 {
        let on_disk = dest.metadata()?.len();
        if on_disk == content_length {
            println!("  [{i:02}] {filename} — already downloaded, skipping");
            return Ok(dest);
        }
    }

    // Resume from partial
    let resume_pos = if dest.exists() {
        dest.metadata()?.len()
    } else {
        0
    };

    if resume_pos > 0 {
        println!(
            "  [{i:02}] Resuming {filename} from {}MB",
            resume_pos / 1024 / 1024
        );
    } else {
        println!("  [{i:02}] Downloading {filename}");
    }

    let mut req = client.get(&url);
    if resume_pos > 0 {
        req = req.header(reqwest::header::RANGE, format!("bytes={resume_pos}-"));
    }

    let resp = req.send().context("GET request failed")?;
    let mut effective_resume_pos = resume_pos;
    if resume_pos > 0 {
        match resp.status() {
            StatusCode::PARTIAL_CONTENT => {}
            StatusCode::OK => {
                println!(
                    "  [{i:02}] Server did not honor Range; restarting download from 0 for {filename}"
                );
                effective_resume_pos = 0;
            }
            status => {
                bail!("GET {} resume expected 206/200 → {}", url, status);
            }
        }
    } else if !resp.status().is_success() {
        bail!("GET {} → {}", url, resp.status());
    }

    let total = resp
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .map(|n| n + effective_resume_pos)
        .unwrap_or(0);

    let pb = ProgressBar::new(total);
    pb.set_position(effective_resume_pos);
    pb.set_style(
        ProgressStyle::with_template(
            "  [{bar:40}] {bytes}/{total_bytes} {bytes_per_sec} ETA {eta}",
        )
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .progress_chars("##-"),
    );

    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(effective_resume_pos == 0)
        .append(effective_resume_pos > 0)
        .open(&dest)
        .with_context(|| format!("Failed to open {}", dest.display()))?;
    let mut writer = std::io::BufWriter::new(file);

    let mut stream = resp;
    let mut buf = vec![0u8; 1024 * 1024]; // 1 MB chunks
    loop {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            break;
        }
        writer.write_all(&buf[..n])?;
        pb.inc(n as u64);
    }
    writer.flush()?;
    drop(writer);
    pb.finish_and_clear();

    // Validate ZIP magic bytes — catch HTML error pages saved as .zip
    let mut file = std::fs::File::open(&dest)?;
    let mut magic = [0u8; 4];
    if file.read_exact(&mut magic).is_ok()
        && !magic.starts_with(b"PK\x03\x04")
        && !magic.starts_with(b"PK\x05\x06")
    {
        let _ = std::fs::remove_file(&dest);
        bail!(
            "Downloaded file is not a valid ZIP (got {:?}) — auth may have expired",
            &magic
        );
    }

    let final_size = dest.metadata()?.len();
    println!(
        "  [{i:02}] Done → {} ({}MB)",
        filename,
        final_size / 1024 / 1024
    );

    Ok(dest)
}

// MARK: - Hybrid download

/// Try downloading via HTTP first (fast), fall back to Chrome (reliable/auth) if needed.
pub fn download_hybrid(
    job_id: &str,
    user_id: &str,
    i: usize,
    dir: &Path,
    notifier: Option<&Notifier>,
) -> Result<PathBuf> {
    // 1. Try to get cookies and build client
    let client = match get_chrome_cookies() {
        Ok(cookies) => build_client(&cookies).ok(),
        Err(_) => None,
    };

    // 2. If we have a client, try download_zip
    if let Some(client) = client {
        match download_zip(&client, job_id, user_id, i, dir) {
            Ok(path) => return Ok(path),
            Err(e) => {
                let err_msg = e.to_string();
                // Fall back to Chrome only on auth-related errors
                // (text/html redirect, 4xx status, or invalid ZIP magic bytes)
                let is_auth_error = err_msg.contains("text/html")
                    || err_msg.contains("auth issue")
                    || err_msg.contains("auth may have expired");

                if !is_auth_error {
                    return Err(e);
                }
                println!("  [{i:02}] HTTP download failed (auth?); falling back to Chrome...");
            }
        }
    } else {
        println!("  [{i:02}] Could not load Chrome cookies; using Chrome fallback directly");
    }

    // 3. Fallback to Chrome
    download_via_chrome(job_id, user_id, i, dir, notifier)
}

// MARK: - Chrome-delegated download

/// Download Takeout part `i` by opening the URL in Chrome.
/// Chrome handles passkey/re-auth challenges natively.
/// Watches the download directory for the completed zip file.
///
/// For parallel safety: snapshots existing `.crdownload` files before opening
/// Chrome and only tracks NEW ones belonging to this worker.
pub fn download_via_chrome(
    job_id: &str,
    user_id: &str,
    i: usize,
    dir: &Path,
    notifier: Option<&Notifier>,
) -> Result<PathBuf> {
    let url = build_url(job_id, user_id, i);

    // Snapshot existing zip files before opening Chrome
    let existing_zips: HashSet<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension().map_or(false, |ext| ext == "zip")
                && p.file_name()
                    .map_or(false, |n| n.to_string_lossy().starts_with("takeout-"))
        })
        .collect();

    // Snapshot existing .crdownload files for parallel isolation —
    // only track NEW ones that appear after we open Chrome
    let pre_existing_crdownloads: HashSet<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map_or(false, |ext| ext == "crdownload"))
        .collect();

    println!("  [{i:02}] Opening download URL in Chrome...");
    Command::new("open")
        .args(["-a", "Google Chrome", &url])
        .spawn()
        .context("Failed to open URL in Chrome")?;

    println!(
        "  [{i:02}] Waiting for Chrome to download (authenticate if prompted)..."
    );

    let poll_interval = Duration::from_secs(5);
    let progress_interval = Duration::from_secs(30);
    let auth_alert_timeout = Duration::from_secs(60); // alert if no crdownload after 60s
    let stall_timeout = Duration::from_secs(120); // 2 min stall = retry
    let timeout = Duration::from_secs(7200); // 2h max per part
    let max_retries = 3;
    let start = Instant::now();
    let mut crdownload_seen = false;
    let mut auth_alerted = false;
    let mut last_progress = Instant::now() - progress_interval;
    let mut last_size: u64 = 0;
    let mut last_size_change = Instant::now();
    let mut retries = 0;

    loop {
        if start.elapsed() > timeout {
            bail!("Timed out waiting for Chrome to download part {i}");
        }

        // Check for NEW .crdownload files only (parallel isolation)
        let crdownloads: Vec<PathBuf> = std::fs::read_dir(dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.extension().map_or(false, |ext| ext == "crdownload")
                    && !pre_existing_crdownloads.contains(p)
            })
            .collect();

        if !crdownloads.is_empty() && !crdownload_seen {
            crdownload_seen = true;
            println!("  [{i:02}] Download started in Chrome");
        }

        // Auth stall alert: no .crdownload appeared within 60s
        if !crdownload_seen && !auth_alerted && start.elapsed() > auth_alert_timeout {
            auth_alerted = true;
            let msg = format!("photoferry: Part {i} may need auth — no download started after 60s. Check Chrome.");
            println!("  [{i:02}] WARNING: {msg}");
            notify::notify(notifier, &msg);
        }

        // Stall detection: if download started but size hasn't changed in 2 min, retry
        if crdownload_seen && !crdownloads.is_empty() {
            let current_size: u64 = crdownloads
                .iter()
                .filter_map(|p| p.metadata().ok())
                .map(|m| m.len())
                .sum();

            if current_size != last_size {
                last_size = current_size;
                last_size_change = Instant::now();
            } else if last_size_change.elapsed() > stall_timeout {
                retries += 1;
                if retries > max_retries {
                    bail!(
                        "Part {i} stalled {} times — giving up. Delete .crdownload files and retry manually.",
                        max_retries
                    );
                }
                println!(
                    "  [{i:02}] Download stalled for {}s — deleting and retrying ({retries}/{max_retries})",
                    stall_timeout.as_secs()
                );
                // Delete only OUR stalled .crdownload files
                for cd in &crdownloads {
                    let _ = std::fs::remove_file(cd);
                }
                // Re-open in Chrome
                Command::new("open")
                    .args(["-a", "Google Chrome", &url])
                    .spawn()
                    .context("Failed to re-open URL in Chrome")?;
                crdownload_seen = false;
                last_size = 0;
                last_size_change = Instant::now();
                std::thread::sleep(poll_interval);
                continue;
            }
        }

        // Check for new completed zip files
        let current_zips: HashSet<PathBuf> = std::fs::read_dir(dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.extension().map_or(false, |ext| ext == "zip")
                    && p.file_name()
                        .map_or(false, |n| n.to_string_lossy().starts_with("takeout-"))
            })
            .collect();

        let new_zips: Vec<&PathBuf> = current_zips.difference(&existing_zips).collect();

        if !new_zips.is_empty() {
            // Found a new zip — verify it's not still being written
            for zip_path in &new_zips {
                // Check no .crdownload files remain (Chrome renames atomically on completion)
                if crdownloads.is_empty() || (crdownload_seen && crdownloads.is_empty()) {
                    let size = zip_path.metadata()?.len();
                    println!(
                        "  [{i:02}] Chrome download complete → {} ({:.1}GB)",
                        zip_path.file_name().unwrap_or_default().to_string_lossy(),
                        size as f64 / 1024.0 / 1024.0 / 1024.0
                    );
                    return Ok(zip_path.to_path_buf());
                }
            }
        }

        // Show progress for active downloads
        if crdownload_seen && last_progress.elapsed() >= progress_interval {
            for cd in &crdownloads {
                if let Ok(meta) = cd.metadata() {
                    let gb = meta.len() as f64 / 1024.0 / 1024.0 / 1024.0;
                    println!("  [{i:02}] Downloading... {gb:.1}GB so far");
                    last_progress = Instant::now();
                }
            }
        }

        std::thread::sleep(poll_interval);
    }
}

fn extract_filename(resp: &reqwest::blocking::Response) -> Option<String> {
    let cd = resp.headers().get("content-disposition")?.to_str().ok()?;
    // content-disposition: attachment; filename="takeout-xxx.zip"
    let pos = cd.find("filename=")?;
    let rest = cd[pos + "filename=".len()..].trim_start_matches('"');
    let end = rest.find(['"', ';', '\n']).unwrap_or(rest.len());
    let name = rest[..end].trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::{DownloadProgress, progress_path};

    #[test]
    fn progress_path_is_unique_for_distinct_jobs_with_same_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let p1 = progress_path(dir.path(), "abcdefgh-job-one");
        let p2 = progress_path(dir.path(), "abcdefgh-job-two");
        assert_ne!(p1, p2);
    }

    #[test]
    fn load_errors_on_corrupt_progress_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = progress_path(dir.path(), "job-123");
        std::fs::write(path, "{bad-json").unwrap();
        assert!(DownloadProgress::load(dir.path(), "job-123").is_err());
    }
}
