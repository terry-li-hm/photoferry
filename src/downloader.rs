use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

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
    pub fn load(dir: &Path, job_id: &str) -> Self {
        let path = progress_path(dir, job_id);
        if let Ok(data) = std::fs::read_to_string(&path) {
            if let Ok(p) = serde_json::from_str::<DownloadProgress>(&data) {
                return p;
            }
        }
        DownloadProgress {
            job_id: job_id.to_string(),
            user_id: String::new(),
            completed: Vec::new(),
            failed: Vec::new(),
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
    // Use first 8 chars of job_id for a readable but unique filename
    let prefix = if job_id.len() >= 8 {
        &job_id[..8]
    } else {
        job_id
    };
    dir.join(format!(".photoferry-download-{prefix}.json"))
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
    // Match cookies for google.com and direct subdomains (same as pycookiecheat for takeout.google.com)
    // host_key values: '.google.com', 'takeout.google.com', 'google.com'
    let mut stmt = conn
        .prepare(
            "SELECT name, encrypted_value FROM cookies \
             WHERE host_key IN ('.google.com', 'google.com', 'takeout.google.com', \
             '.takeout.google.com', 'accounts.google.com', '.accounts.google.com')",
        )
        .context("Failed to query cookies")?;

    let mut cookies = HashMap::new();
    let mut rows = stmt.query([]).context("Failed to execute cookie query")?;

    while let Some(row) = rows.next().context("Error reading cookie row")? {
        let name: String = row.get(0)?;
        let encrypted: Vec<u8> = row.get(1)?;
        if let Ok(value) = decrypt_cookie_value(&encrypted, key) {
            if !value.is_empty() {
                cookies.insert(name, value);
            }
        }
    }

    Ok(cookies)
}

fn decrypt_cookie_value(encrypted: &[u8], key: &[u8; COOKIES_KEY_LEN]) -> Result<String> {
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

        return Ok(String::from_utf8_lossy(decrypted).into_owned());
    }

    // Unencrypted (older Chrome format)
    Ok(String::from_utf8_lossy(encrypted).into_owned())
}

// MARK: - HTTP download

pub fn build_url(job_id: &str, user_id: &str, i: usize) -> String {
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
    if !cookie_str.is_empty() {
        if let Ok(val) = cookie_str.parse::<reqwest::header::HeaderValue>() {
            headers.insert(reqwest::header::COOKIE, val);
        }
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
    if !resp.status().is_success() {
        bail!("GET {} → {}", url, resp.status());
    }

    let total = resp
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .map(|n| n + resume_pos)
        .unwrap_or(0);

    let pb = ProgressBar::new(total);
    pb.set_position(resume_pos);
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
        .append(resume_pos > 0)
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

fn extract_filename(resp: &reqwest::blocking::Response) -> Option<String> {
    let cd = resp.headers().get("content-disposition")?.to_str().ok()?;
    // content-disposition: attachment; filename="takeout-xxx.zip"
    let pos = cd.find("filename=")?;
    let rest = cd[pos + "filename=".len()..].trim_start_matches('"');
    let end = rest
        .find(|c| c == '"' || c == ';' || c == '\n')
        .unwrap_or(rest.len());
    let name = rest[..end].trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}
