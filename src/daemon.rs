//! Lazy daemon — keeps heavy TTS models warm in memory.
//!
//! `vox daemon start` launches a local HTTP server. Subsequent `vox -b voxtream "text"`
//! calls route through the daemon for ~1-2s latency instead of 20-60s cold start.
//! Auto-shuts down after idle timeout (default 5min).
//!
//! For voxtream: launches voxtream-server (FastAPI/WebSocket) as child process,
//! then proxies speak requests through a Python WebSocket client script.
//!
//! # Security
//!
//! The daemon binds 127.0.0.1 and authenticates every request with a random token
//! generated at startup. The token is written to the PID file (mode 0600) and read
//! by in-process clients via [`read_auth_token`]. A request without a matching
//! `X-Vox-Token` header is rejected with 401, so only the user who started the
//! daemon can drive it.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, Notify};

use crate::audio;
use crate::backend::voxtream::VoxtreamServer;
use crate::backend::{self, SpeakOptions};
use crate::config;

const DEFAULT_PORT: u16 = 19876;
const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 300;

/// Upper bound on a request body. A speak text never needs megabytes; anything
/// larger is almost certainly an abuse attempt. Rejects before allocating.
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

/// Header carrying the per-daemon auth token.
const AUTH_HEADER: &str = "x-vox-token";

pub fn daemon_port() -> u16 {
    std::env::var("VOX_DAEMON_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_PORT)
}

pub fn pid_path() -> PathBuf {
    config::config_dir().join("daemon.pid")
}

fn daemon_url(path: &str) -> String {
    format!("http://127.0.0.1:{}{}", daemon_port(), path)
}

// ── PID file + auth token management ──────────────────────────
//
// The PID file carries two lines:
//   line 1: the daemon's OS pid
//   line 2: a 64-hex-char auth token (32 random bytes)
// Created with 0600 perms on Unix so only the owning user can read the token.

fn write_pid() -> Result<String> {
    let path = pid_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    // 32 random bytes -> 64 hex chars.
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .context("failed to write PID file")?;
        writeln!(f, "{}", std::process::id())?;
        writeln!(f, "{token}")?;
    }
    #[cfg(not(unix))]
    {
        let mut f = std::fs::File::create(&path).context("failed to write PID file")?;
        writeln!(f, "{}", std::process::id())?;
        writeln!(f, "{token}")?;
    }

    Ok(token)
}

pub fn read_pid() -> Option<u32> {
    let content = std::fs::read_to_string(pid_path()).ok()?;
    content.lines().next().and_then(|s| s.trim().parse().ok())
}

/// Read the auth token from the PID file. Returns `None` if there is no PID file
/// or it predates the token feature (single-line file).
pub fn read_auth_token() -> Option<String> {
    let content = std::fs::read_to_string(pid_path()).ok()?;
    let mut lines = content.lines();
    lines.next()?; // pid
    let token = lines.next()?;
    let token = token.trim();
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

pub fn remove_pid() {
    let _ = std::fs::remove_file(pid_path());
}

// ── Health check (client side) ───────────────────────────────

/// Build a blocking request to the daemon with the auth token attached, if one
/// is present in the PID file.
fn authed_request(
    client: &reqwest::blocking::Client,
    method: reqwest::Method,
    path: &str,
) -> reqwest::blocking::RequestBuilder {
    let mut req = client.request(method, daemon_url(path));
    if let Some(token) = read_auth_token() {
        req = req.header(AUTH_HEADER, token);
    }
    req
}

pub fn is_running() -> bool {
    let client = reqwest::blocking::Client::new();
    authed_request(&client, reqwest::Method::GET, "/health")
        .timeout(Duration::from_millis(500))
        .send()
        .is_ok_and(|r| r.status().is_success())
}

// ── Request / Response types ─────────────────────────────────

#[derive(Serialize, Deserialize)]
pub struct SpeakRequest {
    pub text: String,
    pub backend: String,
    #[serde(default)]
    pub voice: Option<String>,
    #[serde(default)]
    pub lang: Option<String>,
    #[serde(default)]
    pub rate: Option<u32>,
    #[serde(default)]
    pub gender: Option<String>,
    #[serde(default)]
    pub style: Option<String>,
    #[serde(default)]
    pub ref_audio: Option<String>,
    #[serde(default)]
    pub ref_text: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default = "default_volume")]
    pub volume: f32,
}

fn default_volume() -> f32 {
    1.0
}

impl SpeakRequest {
    /// Clamp volume to valid range after deserialization.
    pub fn validated(mut self) -> Self {
        self.volume = self.volume.clamp(0.0, 5.0);
        self
    }
}

#[derive(Serialize, Deserialize)]
struct HealthResponse {
    status: String,
    uptime_secs: u64,
    loaded_backends: Vec<String>,
    pid: u32,
}

#[derive(Serialize, Deserialize)]
struct SpeakResponse {
    success: bool,
    error: Option<String>,
    duration_ms: Option<u64>,
}

// ── Daemon state ─────────────────────────────────────────────
//
// VoxtreamServer itself now lives in backend/voxtream.rs (R3); the daemon only
// owns its lifetime via the Mutex<Option<VoxtreamServer>> below.

struct DaemonState {
    start_time: Instant,
    last_request: AtomicU64,
    /// Per-request auth token. Kept on `DaemonState` so the lifetime ties to the
    /// running daemon rather than a global.
    auth_token: String,
    voxtream_server: Mutex<Option<VoxtreamServer>>,
}

impl DaemonState {
    fn new(auth_token: String) -> Self {
        Self {
            start_time: Instant::now(),
            last_request: AtomicU64::new(now_epoch_secs()),
            auth_token,
            voxtream_server: Mutex::new(None),
        }
    }

    fn touch(&self) {
        self.last_request.store(now_epoch_secs(), Ordering::Relaxed);
    }

    fn idle_secs(&self) -> u64 {
        now_epoch_secs().saturating_sub(self.last_request.load(Ordering::Relaxed))
    }

    fn uptime_secs(&self) -> u64 {
        self.start_time.elapsed().as_secs()
    }

    /// Returns true if the presented token matches the daemon's token.
    fn check_auth(&self, presented: Option<&str>) -> bool {
        // Constant-time-ish comparison: compare lengths then bytes. Tokens are
        // fixed-length (64 hex), so a length mismatch short-circuits cheaply
        // without leaking position info.
        match presented {
            Some(p) => {
                let a = p.as_bytes();
                let b = self.auth_token.as_bytes();
                a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x == y)
            }
            None => false,
        }
    }

    async fn loaded_backends(&self) -> Vec<String> {
        let mut backends = Vec::new();
        if self.voxtream_server.lock().await.is_some() {
            backends.push("voxtream".into());
        }
        backends
    }

    /// Ensure voxtream-server is running, start if needed.
    async fn ensure_voxtream_server(&self) -> Result<()> {
        let mut server = self.voxtream_server.lock().await;
        if let Some(ref mut s) = *server {
            if s.is_alive() {
                return Ok(());
            }
            eprintln!("[daemon] voxtream-server died, restarting...");
        }
        let s = tokio::task::spawn_blocking(VoxtreamServer::start).await??;
        *server = Some(s);
        Ok(())
    }
}

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Backends whose model-load cost is high enough that routing through the
/// warm daemon is worthwhile. Shared source of truth for CLI and MCP.
pub fn is_heavy_backend(name: &str) -> bool {
    matches!(
        name,
        "voxtream" | "qwen" | "qwen-native" | "kokoro" | "pocket"
    )
}

/// Whether the caller wants the daemon auto-started/opted-out. Honors both a
/// `--no-daemon`-set pref (passed in) and the `VOX_NO_DAEMON` env var.
pub fn daemon_disabled(no_daemon_flag: bool) -> bool {
    no_daemon_flag || std::env::var("VOX_NO_DAEMON").is_ok_and(|v| !v.is_empty())
}

/// Ensure the daemon is running; spawn it (silently) if not. Used by the CLI and
/// MCP paths to transparently warm heavy backends on first use.
pub fn ensure_running() -> Result<()> {
    if is_running() {
        return Ok(());
    }
    spawn_daemon_background(false, None)?;
    // Brief readiness wait — the daemon writes the PID file (with token) before
    // binding, so once /health answers we can talk to it.
    let client = reqwest::blocking::Client::new();
    for _ in 0..30 {
        std::thread::sleep(Duration::from_millis(200));
        if authed_request(&client, reqwest::Method::GET, "/health")
            .timeout(Duration::from_millis(500))
            .send()
            .is_ok_and(|r| r.status().is_success())
        {
            return Ok(());
        }
    }
    anyhow::bail!("daemon failed to become ready within 6 seconds")
}

/// Spawn the daemon as a detached background process. `announce` prints the
/// "starting" line for the explicit `daemon start` command. `idle_timeout`
/// overrides the default when set.
fn spawn_daemon_background(announce: bool, idle_timeout: Option<u64>) -> Result<()> {
    let exe = std::env::current_exe().context("cannot find vox binary")?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["daemon", "_run"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(t) = idle_timeout {
        cmd.args(["--idle-timeout", &t.to_string()]);
    }
    let child = cmd.spawn().context("failed to spawn daemon process")?;
    if announce {
        println!(
            "Daemon starting (pid {}, port {})...",
            child.id(),
            daemon_port()
        );
    }
    Ok(())
}

/// Run the daemon HTTP server.
pub async fn run(idle_timeout: u64) -> Result<()> {
    let port = daemon_port();
    let timeout = if idle_timeout > 0 {
        idle_timeout
    } else {
        DEFAULT_IDLE_TIMEOUT_SECS
    };

    let auth_token = write_pid()?;
    let state = Arc::new(DaemonState::new(auth_token));
    let shutdown: Arc<Notify> = Arc::new(Notify::new());

    let listener = TcpListener::bind(format!("127.0.0.1:{port}"))
        .await
        .with_context(|| format!("failed to bind port {port}"))?;

    eprintln!("[daemon] vox daemon listening on 127.0.0.1:{port} (idle timeout: {timeout}s)");

    // Idle watchdog — signals shutdown rather than exit()ing, so Drop runs.
    let watchdog_state = Arc::clone(&state);
    let watchdog_shutdown = Arc::clone(&shutdown);
    let watchdog = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(10)).await;
            if watchdog_state.idle_secs() > timeout {
                eprintln!("[daemon] Idle timeout ({timeout}s) — shutting down.");
                watchdog_shutdown.notify_waiters();
                return;
            }
        }
    });

    // OS-signal handler — SIGINT/SIGTERM on Unix, ctrl_c on Windows.
    let signal_shutdown = Arc::clone(&shutdown);
    tokio::spawn(async move {
        let _ = shutdown_on_signal().await;
        eprintln!("[daemon] Shutdown signal received.");
        signal_shutdown.notify_waiters();
    });

    // Accept loop: exits on shutdown signal so the state (and its VoxtreamServer
    // child) is dropped cleanly instead of via process::exit().
    loop {
        tokio::select! {
            biased;
            _ = shutdown.notified() => {
                eprintln!("[daemon] Shutting down.");
                break;
            }
            accept = listener.accept() => {
                let (stream, _peer) = accept?;
                let state = Arc::clone(&state);
                let shutdown = Arc::clone(&shutdown);
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream, state, shutdown).await {
                        eprintln!("[daemon] Connection error: {e}");
                    }
                });
            }
        }
    }

    // Graceful teardown: stop the watchdog, drop state (kills voxtream-server
    // child via VoxtreamServer::drop), remove the PID file.
    watchdog.abort();
    drop(state);
    remove_pid();
    eprintln!("[daemon] Stopped.");
    Ok(())
}

/// Wait for an OS shutdown signal. Unix: SIGINT/SIGTERM. Windows: ctrl_c.
async fn shutdown_on_signal() -> Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigint = signal(SignalKind::interrupt())?;
        let mut sigterm = signal(SignalKind::terminate())?;
        tokio::select! {
            _ = sigint.recv() => {}
            _ = sigterm.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await?;
    }
    Ok(())
}

/// Minimal HTTP/1.1 handler.
async fn handle_connection(
    stream: tokio::net::TcpStream,
    state: Arc<DaemonState>,
    shutdown: Arc<Notify>,
) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut buf_reader = BufReader::new(reader);

    let mut request_line = String::new();
    buf_reader.read_line(&mut request_line).await?;
    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 2 {
        return Ok(());
    }
    let method = parts[0].to_string();
    let path = parts[1].to_string();

    let mut content_length = 0usize;
    let mut presented_token: Option<String> = None;
    loop {
        let mut line = String::new();
        buf_reader.read_line(&mut line).await?;
        if line.trim().is_empty() {
            break;
        }
        let lower = line.to_lowercase();
        if let Some(val) = lower.strip_prefix("content-length:") {
            content_length = val.trim().parse().unwrap_or(0);
        } else if let Some(val) = lower.strip_prefix(&format!("{AUTH_HEADER}:")) {
            presented_token = Some(val.trim().to_string());
        }
    }

    // Auth check — applies to every route including /health.
    if !state.check_auth(presented_token.as_deref()) {
        let resp = serde_json::json!({"error": "unauthorized"});
        write_response(&mut writer, "401 Unauthorized", &resp).await?;
        return Ok(());
    }

    // Body-size guard — reject before allocating.
    if content_length > MAX_BODY_BYTES {
        let resp = serde_json::json!({"error": "payload too large", "max_bytes": MAX_BODY_BYTES});
        write_response(&mut writer, "413 Payload Too Large", &resp).await?;
        return Ok(());
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        buf_reader.read_exact(&mut body).await?;
    }

    let (status, response_body) = route(&method, &path, &body, &state, &shutdown).await;

    let http = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        response_body.len(),
        response_body
    );
    writer.write_all(http.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

async fn write_response(
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    status: &str,
    body: &serde_json::Value,
) -> Result<()> {
    let body = body.to_string();
    let http = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    writer.write_all(http.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

async fn route(
    method: &str,
    path: &str,
    body: &[u8],
    state: &Arc<DaemonState>,
    shutdown: &Arc<Notify>,
) -> (&'static str, String) {
    match (method, path) {
        ("GET", "/health") => {
            state.touch();
            let resp = serde_json::json!({
                "status": "ok",
                "uptime_secs": state.uptime_secs(),
                "loaded_backends": state.loaded_backends().await,
                "pid": std::process::id(),
            });
            ("200 OK", resp.to_string())
        }
        ("POST", "/speak") => {
            let req: SpeakRequest = match serde_json::from_slice(body) {
                Ok(r) => SpeakRequest::validated(r),
                Err(e) => {
                    let resp = serde_json::json!({"success": false, "error": format!("invalid JSON: {e}")});
                    return ("400 Bad Request", resp.to_string());
                }
            };

            state.touch();

            let is_voxtream = req.backend == "voxtream";

            if is_voxtream {
                // Use warm voxtream-server
                if let Err(e) = state.ensure_voxtream_server().await {
                    let resp = serde_json::json!({"success": false, "error": format!("voxtream-server start failed: {e:#}")});
                    return ("500 Internal Server Error", resp.to_string());
                }

                let text = req.text.clone();
                let ref_audio = req.ref_audio.clone();
                let volume = req.volume;
                let state_clone = Arc::clone(state);

                let result = tokio::task::spawn_blocking(move || {
                    let start = Instant::now();

                    // Get prompt audio
                    let prompt = ref_audio.unwrap_or_else(|| {
                        crate::backend::voxtream::default_prompt_audio()
                            .map(|p| p.to_string_lossy().to_string())
                            .unwrap_or_default()
                    });

                    let tmp = tempfile::NamedTempFile::new()?;
                    let wav_path = tmp.path().with_extension("wav");
                    let wav_str = wav_path.to_string_lossy().to_string();

                    // Use voxtream-server via WS
                    let server_guard = state_clone.voxtream_server.blocking_lock();
                    if let Some(ref server) = *server_guard {
                        server.speak(&text, &prompt, &wav_str)?;
                    } else {
                        anyhow::bail!("voxtream-server not running");
                    }
                    drop(server_guard);

                    // Play the WAV
                    audio::apply_wav_gain(&wav_path, volume)?;
                    audio::play_wav_blocking(&wav_path)?;
                    let _ = std::fs::remove_file(&wav_path);

                    Ok::<_, anyhow::Error>(start.elapsed())
                })
                .await;

                match result {
                    Ok(Ok(dur)) => {
                        let resp = serde_json::json!({"success": true, "duration_ms": dur.as_millis() as u64});
                        ("200 OK", resp.to_string())
                    }
                    Ok(Err(e)) => {
                        let resp = serde_json::json!({"success": false, "error": format!("{e:#}")});
                        ("500 Internal Server Error", resp.to_string())
                    }
                    Err(e) => {
                        let resp = serde_json::json!({"success": false, "error": format!("task panicked: {e}")});
                        ("500 Internal Server Error", resp.to_string())
                    }
                }
            } else {
                // Non-voxtream: direct backend call. No global lock — each backend
                // guards its own model with its own Mutex, so unrelated backends
                // (e.g. piper + pocket) run concurrently.
                let opts = SpeakOptions {
                    voice: req.voice.clone(),
                    lang: req.lang.clone(),
                    rate: req.rate,
                    gender: req.gender.clone(),
                    style: req.style.clone(),
                    ref_audio: req.ref_audio.clone(),
                    ref_text: req.ref_text.clone(),
                    model: req.model.clone(),
                    volume: req.volume,
                };
                let backend_name = req.backend.clone();
                let text = req.text.clone();

                let result = tokio::task::spawn_blocking(move || {
                    let start = Instant::now();
                    let b = backend::get_backend(&backend_name)?;
                    b.speak(&text, &opts)?;
                    Ok::<_, anyhow::Error>(start.elapsed())
                })
                .await;

                match result {
                    Ok(Ok(dur)) => {
                        let resp = serde_json::json!({"success": true, "duration_ms": dur.as_millis() as u64});
                        ("200 OK", resp.to_string())
                    }
                    Ok(Err(e)) => {
                        let resp = serde_json::json!({"success": false, "error": format!("{e:#}")});
                        ("500 Internal Server Error", resp.to_string())
                    }
                    Err(e) => {
                        let resp = serde_json::json!({"success": false, "error": format!("task panicked: {e}")});
                        ("500 Internal Server Error", resp.to_string())
                    }
                }
            }
        }
        ("POST", "/shutdown") => {
            eprintln!("[daemon] Shutdown requested.");
            // Signal the accept loop to exit so state is dropped cleanly. Fire
            // after a tiny delay so this 200 response is flushed first.
            let shutdown = Arc::clone(shutdown);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                shutdown.notify_waiters();
            });
            let resp = serde_json::json!({"status": "shutting_down"});
            ("200 OK", resp.to_string())
        }
        _ => {
            let resp = serde_json::json!({"error": "not found"});
            ("404 Not Found", resp.to_string())
        }
    }
}

// ── Client: speak through daemon ─────────────────────────────

pub fn speak_via_daemon(text: &str, backend: &str, opts: &SpeakOptions) -> Result<()> {
    let req = SpeakRequest {
        text: text.to_string(),
        backend: backend.to_string(),
        voice: opts.voice.clone(),
        lang: opts.lang.clone(),
        rate: opts.rate,
        gender: opts.gender.clone(),
        style: opts.style.clone(),
        ref_audio: opts.ref_audio.clone(),
        ref_text: opts.ref_text.clone(),
        model: opts.model.clone(),
        volume: opts.volume,
    };

    let client = reqwest::blocking::Client::new();
    let resp: SpeakResponse = authed_request(&client, reqwest::Method::POST, "/speak")
        .json(&req)
        .timeout(Duration::from_secs(120))
        .send()
        .context("failed to connect to vox daemon")?
        .json()
        .context("invalid daemon response")?;

    if resp.success {
        Ok(())
    } else {
        anyhow::bail!(
            "daemon speak failed: {}",
            resp.error.unwrap_or_else(|| "unknown error".into())
        )
    }
}

// ── CLI handlers ─────────────────────────────────────────────

pub fn handle_start(idle_timeout: u64) -> Result<()> {
    if is_running() {
        println!("Daemon already running (port {}).", daemon_port());
        return Ok(());
    }

    spawn_daemon_background(true, Some(idle_timeout))?;

    let client = reqwest::blocking::Client::new();
    for _ in 0..30 {
        std::thread::sleep(Duration::from_millis(500));
        if authed_request(&client, reqwest::Method::GET, "/health")
            .timeout(Duration::from_millis(500))
            .send()
            .is_ok_and(|r| r.status().is_success())
        {
            println!("Daemon ready.");
            return Ok(());
        }
    }

    anyhow::bail!("Daemon failed to start within 15 seconds")
}

pub fn handle_stop() -> Result<()> {
    if !is_running() {
        println!("Daemon not running.");
        remove_pid();
        return Ok(());
    }

    let client = reqwest::blocking::Client::new();
    authed_request(&client, reqwest::Method::POST, "/shutdown")
        .timeout(Duration::from_secs(5))
        .send()
        .ok();

    for _ in 0..10 {
        std::thread::sleep(Duration::from_millis(300));
        if !is_running() {
            println!("Daemon stopped.");
            remove_pid();
            return Ok(());
        }
    }

    if let Some(pid) = read_pid() {
        #[cfg(unix)]
        {
            Command::new("kill").arg(pid.to_string()).status().ok();
        }
        remove_pid();
        println!("Daemon killed (pid {pid}).");
    }

    Ok(())
}

pub fn handle_status() -> Result<()> {
    let client = reqwest::blocking::Client::new();
    match authed_request(&client, reqwest::Method::GET, "/health")
        .timeout(Duration::from_millis(1000))
        .send()
    {
        Ok(resp) if resp.status().is_success() => {
            let health: HealthResponse = resp.json().context("invalid health response")?;
            println!(
                "Daemon running (pid {}, port {})",
                health.pid,
                daemon_port()
            );
            println!("  Uptime:  {}s", health.uptime_secs);
            if health.loaded_backends.is_empty() {
                println!("  Models:  (none loaded yet)");
            } else {
                println!("  Models:  {}", health.loaded_backends.join(", "));
            }
        }
        _ => {
            println!("Daemon not running.");
            if let Some(pid) = read_pid() {
                println!("  (stale PID file: {pid})");
                remove_pid();
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Edition 2024 made std::env::set_var / remove_var unsafe (they can race
    // with concurrent readers of the environment). Tests are single-threaded
    // over the env, so this is contained — wrap once here for readability.
    fn set_env(key: &str, val: &str) {
        // SAFETY: env-mutating tests are serialized via ENV_LOCK, so no other
        // thread reads/writes the env concurrently.
        unsafe {
            std::env::set_var(key, val);
        }
    }
    fn remove_env(key: &str) {
        // SAFETY: see set_env.
        unsafe {
            std::env::remove_var(key);
        }
    }

    // The process environment is a global; tests that mutate VOX_* vars must not
    // run in parallel or they observe each other's state. This mutex serializes
    // them while leaving the (env-independent) tests parallel.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn is_heavy_backend_classifies_correctly() {
        // Heavy: model-load cost dominates.
        assert!(is_heavy_backend("voxtream"));
        assert!(is_heavy_backend("qwen"));
        assert!(is_heavy_backend("qwen-native"));
        assert!(is_heavy_backend("kokoro"));
        assert!(is_heavy_backend("pocket"));
        // Light: near-instant, no warm-up benefit.
        assert!(!is_heavy_backend("say"));
        assert!(!is_heavy_backend("piper"));
        assert!(!is_heavy_backend("unknown"));
        assert!(!is_heavy_backend(""));
    }

    #[test]
    fn daemon_disabled_honors_flag_and_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        // Flag off + no env -> enabled.
        remove_env("VOX_NO_DAEMON");
        assert!(!daemon_disabled(false));
        // Flag on -> disabled regardless of env.
        assert!(daemon_disabled(true));
        // Env set -> disabled regardless of flag.
        set_env("VOX_NO_DAEMON", "1");
        assert!(daemon_disabled(false));
        // Empty env value treated as not-set (enabled).
        set_env("VOX_NO_DAEMON", "");
        assert!(!daemon_disabled(false));
        remove_env("VOX_NO_DAEMON");
    }

    #[test]
    fn check_auth_rejects_missing_and_mismatched() {
        let state = DaemonState::new("abcdef0123456789".to_string());
        // Missing token.
        assert!(!state.check_auth(None));
        // Wrong token.
        assert!(!state.check_auth(Some("wrong")));
        // Correct token.
        assert!(state.check_auth(Some("abcdef0123456789")));
        // Length differs -> rejected before byte compare.
        assert!(!state.check_auth(Some("abcdef012345678")));
    }

    /// PID file write/read round-trip under an isolated VOX_CONFIG_DIR, including
    /// the auth token on line 2 and the 0600 perms on Unix.
    #[test]
    fn pid_file_round_trips_pid_and_token() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        set_env("VOX_CONFIG_DIR", tmp.path().to_str().unwrap());
        // write_pid uses std::process::id() of the test process.
        let token = write_pid().unwrap();
        assert_eq!(token.len(), 64, "token is 32 bytes hex-encoded");
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));

        let pid = read_pid().expect("pid readable");
        assert_eq!(pid, std::process::id());

        let read_back = read_auth_token().expect("token readable");
        assert_eq!(read_back, token);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::metadata(pid_path()).unwrap().permissions().mode();
            assert_eq!(perms & 0o777, 0o600, "PID file must be owner-only readable");
        }

        remove_pid();
        assert!(!pid_path().exists());
        remove_env("VOX_CONFIG_DIR");
    }

    /// A legacy single-line PID file (no token) degrades gracefully: pid reads,
    /// token returns None.
    #[test]
    fn legacy_single_line_pid_file_yields_no_token() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        set_env("VOX_CONFIG_DIR", tmp.path().to_str().unwrap());
        std::fs::write(pid_path(), format!("{}\n", std::process::id())).unwrap();
        assert_eq!(read_pid(), Some(std::process::id()));
        assert_eq!(read_auth_token(), None);
        remove_pid();
        remove_env("VOX_CONFIG_DIR");
    }
}
