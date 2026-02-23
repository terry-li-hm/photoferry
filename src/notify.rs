use reqwest::blocking::Client;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Telegram notifier. Constructed from env vars; silent no-op if unset.
pub struct Notifier {
    client: Client,
    bot_token: String,
    chat_id: String,
}

impl Notifier {
    /// Returns `Some(Notifier)` if both `TELEGRAM_BOT_TOKEN` and `TELEGRAM_CHAT_ID` are set.
    pub fn from_env() -> Option<Self> {
        let bot_token = std::env::var("TELEGRAM_BOT_TOKEN").ok()?;
        let chat_id = std::env::var("TELEGRAM_CHAT_ID").ok()?;
        if bot_token.is_empty() || chat_id.is_empty() {
            return None;
        }
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .ok()?;
        Some(Self {
            client,
            bot_token,
            chat_id,
        })
    }

    /// Send a message. Errors are silently swallowed.
    pub fn send(&self, text: &str) {
        let url = format!(
            "https://api.telegram.org/bot{}/sendMessage",
            self.bot_token
        );
        let _ = self
            .client
            .post(&url)
            .form(&[("chat_id", &self.chat_id), ("text", &text.to_string())])
            .send();
    }
}

/// Convenience: send if notifier is present, no-op otherwise.
pub fn notify(notifier: Option<&Notifier>, text: &str) {
    if let Some(n) = notifier {
        n.send(text);
    }
}

/// Tracks download+import pipeline timing for running ETA.
pub struct PipelineStats {
    pipeline_start: Instant,
    completed: Mutex<Vec<PartStat>>,
    total_parts: usize,
}

struct PartStat {
    bytes: u64,
    wall_duration: Duration,
}

impl PipelineStats {
    pub fn new(total_parts: usize) -> Self {
        Self {
            pipeline_start: Instant::now(),
            completed: Mutex::new(Vec::new()),
            total_parts,
        }
    }

    /// Record completion of one part (download + import wall time).
    pub fn record_part(&self, bytes: u64, duration: Duration) {
        let mut parts = self.completed.lock().unwrap();
        parts.push(PartStat {
            bytes,
            wall_duration: duration,
        });
    }

    /// Human-readable ETA string.
    pub fn eta_string(&self) -> String {
        let parts = self.completed.lock().unwrap();
        if parts.is_empty() {
            return "ETA: calculating...".to_string();
        }
        let done = parts.len();
        let remaining = self.total_parts.saturating_sub(done);
        let total_bytes: u64 = parts.iter().map(|p| p.bytes).sum();
        let total_secs: f64 = parts.iter().map(|p| p.wall_duration.as_secs_f64()).sum();

        let avg_secs = total_secs / done as f64;
        let avg_mbps = if total_secs > 0.0 {
            (total_bytes as f64 / 1024.0 / 1024.0) / total_secs
        } else {
            0.0
        };
        let eta_secs = avg_secs * remaining as f64;
        let eta_h = (eta_secs / 3600.0) as u64;
        let eta_m = ((eta_secs % 3600.0) / 60.0) as u64;

        let elapsed = self.pipeline_start.elapsed();
        let elapsed_h = elapsed.as_secs() / 3600;
        let elapsed_m = (elapsed.as_secs() % 3600) / 60;

        format!(
            "Avg: {avg_mbps:.1} MB/s | {done}/{} done | Elapsed: {elapsed_h}h{elapsed_m:02}m | ETA: {eta_h}h{eta_m:02}m ({remaining} remaining)",
            self.total_parts
        )
    }
}
