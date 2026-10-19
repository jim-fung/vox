//! Integration tests for the daemon: auth, body-size cap, and graceful shutdown.
//!
//! Each test spins up a real daemon subprocess on an isolated random port with an
//! isolated config dir, then talks raw HTTP via reqwest. No TTS model is touched
//! — we only exercise the protocol/hardening surface.

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use assert_cmd::cargo::cargo_bin;

/// Pick a free TCP port by binding a temporary listener and immediately dropping it.
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind for free port");
    listener.local_addr().expect("local addr").port()
}

/// Isolation harness: unique config dir + unique port for one daemon instance.
struct DaemonEnv {
    config_dir: PathBuf,
    port: u16,
    child: Option<Child>,
}

impl DaemonEnv {
    fn new() -> Self {
        let config_dir = tempfile::tempdir().unwrap().keep();
        let port = free_port();
        Self {
            config_dir,
            port,
            child: None,
        }
    }

    fn pid_path(&self) -> PathBuf {
        self.config_dir.join("daemon.pid")
    }

    fn read_token(&self) -> Option<String> {
        let content = std::fs::read_to_string(self.pid_path()).ok()?;
        content.lines().nth(1).map(|s| s.trim().to_string())
    }

    /// Start the daemon subprocess and wait until /health answers (with token).
    fn start(&mut self) {
        let bin = cargo_bin("vox");
        let child = Command::new(&bin)
            .arg("daemon")
            .arg("_run")
            .arg("--idle-timeout")
            .arg("0") // disable idle watchdog for the test
            .env("VOX_CONFIG_DIR", &self.config_dir)
            .env("VOX_DAEMON_PORT", self.port.to_string())
            .env("VOX_NO_DAEMON", "1") // prevent nested auto-start attempts
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn vox daemon");
        self.child = Some(child);

        // Wait for readiness (PID file written + /health answering with token).
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if self.read_token().is_some() && self.health_with_token().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!(
            "daemon did not become ready within 15s on port {}",
            self.port
        );
    }

    fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// GET /health with the token; returns the status code on success.
    fn health_with_token(&self) -> Option<u16> {
        let token = self.read_token()?;
        let client = reqwest::blocking::Client::new();
        let resp = client
            .get(format!("{}/health", self.base_url()))
            .header("x-vox-token", token)
            .timeout(Duration::from_millis(500))
            .send()
            .ok()?;
        Some(resp.status().as_u16())
    }

    /// GET /health without the token; returns the status code.
    fn health_without_token(&self) -> Option<u16> {
        let client = reqwest::blocking::Client::new();
        let resp = client
            .get(format!("{}/health", self.base_url()))
            .timeout(Duration::from_millis(500))
            .send()
            .ok()?;
        Some(resp.status().as_u16())
    }

    /// Send a POST with a declared Content-Length and the given body bytes,
    /// returning the raw response status. Used to exercise the body-size cap.
    fn post_speak_raw(&self, content_length: usize, token: Option<&str>) -> Option<u16> {
        use std::io::{Read, Write};
        use std::net::TcpStream;
        let mut stream = TcpStream::connect_timeout(
            &format!("127.0.0.1:{}", self.port).parse().unwrap(),
            Duration::from_secs(2),
        )
        .ok()?;
        stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
        let mut req = format!(
            "POST /speak HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\nConnection: close\r\n",
            content_length
        );
        if let Some(t) = token {
            req.push_str(&format!("x-vox-token: {}\r\n", t));
        }
        req.push_str("\r\n");
        stream.write_all(req.as_bytes()).ok()?;
        // Send a tiny body (server reads exactly content_length bytes).
        stream.write_all(&vec![b'{'; content_length.min(64)]).ok()?;
        if content_length > 64 {
            // Close early to signal we won't actually send the declared size; the
            // server reads content_length and will hit EOF — but the size cap is
            // checked *before* reading, so we still get the 413 we want for the
            // oversized case.
        }
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).ok()?;
        let s = String::from_utf8_lossy(&buf);
        let status_line = s.lines().next()?;
        let code = status_line.split_whitespace().nth(1)?;
        code.parse().ok()
    }

    /// POST /shutdown with the token; returns the status code.
    fn shutdown_with_token(&self) -> Option<u16> {
        let token = self.read_token()?;
        let client = reqwest::blocking::Client::new();
        let resp = client
            .post(format!("{}/shutdown", self.base_url()))
            .header("x-vox-token", token)
            .timeout(Duration::from_secs(2))
            .send()
            .ok()?;
        Some(resp.status().as_u16())
    }

    /// Has the daemon process exited?
    fn child_exited(&mut self) -> bool {
        if let Some(child) = &mut self.child {
            child.try_wait().ok().flatten().is_some()
        } else {
            true
        }
    }
}

impl Drop for DaemonEnv {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // Best-effort cleanup so a failed test doesn't leak a daemon.
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_file(self.pid_path());
    }
}

#[test]
fn health_requires_token() {
    let mut env = DaemonEnv::new();
    env.start();
    // Without token -> 401.
    assert_eq!(env.health_without_token(), Some(401));
    // With token -> 200.
    assert_eq!(env.health_with_token(), Some(200));
}

#[test]
fn speak_without_token_is_unauthorized() {
    let mut env = DaemonEnv::new();
    env.start();
    // Oversized body but no token — auth is checked first, so 401 not 413.
    let status = env.post_speak_raw(8 * 1024 * 1024, None);
    assert_eq!(status, Some(401));
}

#[test]
fn oversized_body_is_rejected_before_allocation() {
    let mut env = DaemonEnv::new();
    env.start();
    let token = env.read_token().expect("token present after start");
    // 8 MB declared Content-Length, well over the 4 MB cap. Must 413 without the
    // server trying to allocate the full 8 MB.
    let status = env.post_speak_raw(8 * 1024 * 1024, Some(&token));
    assert_eq!(status, Some(413));
}

#[test]
fn graceful_shutdown_removes_pid_file() {
    let mut env = DaemonEnv::new();
    env.start();
    assert!(env.pid_path().exists(), "PID file exists while running");
    assert_eq!(env.shutdown_with_token(), Some(200));

    // The daemon signals its accept loop and tears down; wait for exit + PID removal.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if env.child_exited() && !env.pid_path().exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!(
        "daemon did not shut down cleanly within 5s (exited={}, pid_exists={})",
        env.child_exited(),
        env.pid_path().exists()
    );
}
