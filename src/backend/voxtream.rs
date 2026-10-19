//! VoXtream2 TTS backend — zero-shot streaming TTS with dynamic speaking rate control.
//!
//! 0.5B param model, 74ms first-packet latency, 4x real-time on consumer GPU.
//! Supports zero-shot voice cloning via audio prompt (3-10s).
//! Requires: `pip install "voxtream>=0.2"` and `espeak-ng`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

use super::{AudioBuffer, SpeakOptions, TtsBackend};
use crate::audio;
use crate::config;

/// Read a WAV file into an in-memory mono f32 buffer for `synthesize`.
fn load_wav(path: &Path) -> Result<AudioBuffer> {
    let mut reader = hound::WavReader::open(path)
        .with_context(|| format!("failed to open voxtream WAV: {}", path.display()))?;
    let spec = reader.spec();
    let sample_rate = spec.sample_rate;
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<Vec<_>, _>>()?,
        hound::SampleFormat::Int => {
            let max_val = (1i32 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 / max_val))
                .collect::<Result<Vec<_>, _>>()?
        }
    };
    Ok(AudioBuffer {
        samples,
        sample_rate,
    })
}

/// Default prompt audio for voxtream when no voice clone is provided.
/// Generated on first use via macOS `say` or a bundled fallback.
/// Stored under the vox config dir (user-owned, no world-writable collision risk)
/// rather than /tmp — see review VX3.
pub fn default_prompt_audio() -> Result<PathBuf> {
    let path = config::config_dir().join("voxtream_default_prompt.wav");
    if path.exists() {
        return Ok(path);
    }

    // Try generating with macOS say
    #[cfg(target_os = "macos")]
    {
        std::fs::create_dir_all(config::config_dir()).ok();
        let status = Command::new("/usr/bin/say")
            .arg("-v")
            .arg("Samantha")
            .arg("-o")
            .arg(&*path.to_string_lossy())
            .arg("--data-format=LEI16@16000")
            .arg("Hello, my name is Samantha. I am testing voice synthesis today.")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if let Ok(s) = status {
            if s.success() && path.exists() {
                return Ok(path);
            }
        }
    }

    anyhow::bail!(
        "VoXtream2 requires a prompt audio file (3-10s). Provide one via voice clone:\n\
         vox clone add myvoice --audio ~/voice.wav\n\
         vox -b voxtream -v myvoice \"text\""
    )
}

/// Port the voxtream-server WebSocket process listens on. Lives here (not in
/// daemon.rs) because the server lifecycle is a backend concern — see R3.
pub const VOXTREAM_SERVER_PORT: u16 = 7860;

/// A running `voxtream-server` child process, managed as a warm backend.
///
/// Moved out of `daemon.rs` (R3) so the daemon is backend-agnostic: it owns the
/// `Mutex<Option<VoxtreamServer>>` lifetime, but the server itself — its binary
/// discovery, config staging, readiness probe, and the WebSocket client that
/// drives synthesis — belongs to the voxtream backend module.
pub struct VoxtreamServer {
    child: std::process::Child,
}

impl VoxtreamServer {
    /// Spawn voxtream-server (the FastAPI/WebSocket process) and wait until it's
    /// ready to accept connections on `VOXTREAM_SERVER_PORT`.
    pub fn start() -> Result<Self> {
        let bin = find_voxtream().context("voxtream not installed")?;
        // voxtream-server is next to voxtream binary
        let server_bin = bin.with_file_name("voxtream-server");
        if !server_bin.exists() {
            anyhow::bail!("voxtream-server not found at {}", server_bin.display());
        }

        // voxtream-server needs configs/generator.json relative to CWD
        let config_dir = config::config_dir().join("voxtream");
        let configs_subdir = config_dir.join("configs");
        if !configs_subdir.exists() {
            std::fs::create_dir_all(&configs_subdir).ok();
            // Copy config files into configs/ subdirectory
            for name in ["generator.json", "speaking_rate.json"] {
                let src = config_dir.join(name);
                let dst = configs_subdir.join(name);
                if src.exists() && !dst.exists() {
                    std::fs::copy(&src, &dst).ok();
                }
            }
        }

        eprintln!("[daemon] Starting voxtream-server on port {VOXTREAM_SERVER_PORT}...");
        let child = Command::new(&server_bin)
            .current_dir(&config_dir)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| {
                format!("failed to start voxtream-server: {}", server_bin.display())
            })?;

        // Wait for server to be ready
        for i in 0..60 {
            std::thread::sleep(std::time::Duration::from_secs(1));
            if reqwest::blocking::Client::new()
                .get(format!("http://127.0.0.1:{VOXTREAM_SERVER_PORT}/"))
                .timeout(std::time::Duration::from_millis(500))
                .send()
                .is_ok_and(|r| r.status().is_success())
            {
                eprintln!("[daemon] voxtream-server ready after {i}s.");
                return Ok(Self { child });
            }
        }

        anyhow::bail!("voxtream-server failed to start within 60s")
    }

    /// True if the child is still running.
    pub fn is_alive(&mut self) -> bool {
        self.child.try_wait().ok().flatten().is_none()
    }

    /// Speak via the WebSocket client (Python script). Writes a WAV to
    /// `output_wav`.
    ///
    /// The init JSON payload and all paths are built with serde_json (correct
    /// escaping) and written to a temp file that the script reads — no `format!`
    /// interpolation into Python source (review VX1).
    pub fn speak(&self, text: &str, prompt_audio: &str, output_wav: &str) -> Result<()> {
        // Build the init payload with serde_json so all escaping is correct.
        let init_payload = serde_json::json!({
            "event": "init",
            "prompt_audio_path": prompt_audio,
            "text": text.replace('\n', " "),
        });
        let init_json = serde_json::to_string(&init_payload)?;
        let config_json = serde_json::json!({
            "init": init_json,
            "output": output_wav,
            "port": VOXTREAM_SERVER_PORT,
        });
        let config_str = serde_json::to_string(&config_json)?;

        // Write config to a temp file the script reads — no interpolation at all.
        let tmp = tempfile::NamedTempFile::new().context("failed to create voxtream config")?;
        std::fs::write(tmp.path(), &config_str)?;
        let config_path = tmp.path().to_string_lossy().to_string();

        let script = r#"
import json, sys, numpy as np, soundfile as sf
from websockets.sync.client import connect
with open(sys.argv[1]) as f:
    cfg = json.load(f)
ws = connect(f"ws://127.0.0.1:{cfg['port']}/voxtream", close_timeout=5)
try:
    ws.send(cfg['init'])
    sr = 24000
    frames = []
    while True:
        try:
            msg = ws.recv()
        except Exception:
            break
        if isinstance(msg, bytes):
            frames.append(np.frombuffer(msg, dtype=np.float32))
        elif isinstance(msg, str):
            data = json.loads(msg)
            if data.get("type") == "config":
                sr = data.get("sample_rate", 24000)
            elif data.get("type") in ("eos", "done", "error"):
                break
    if frames:
        sf.write(cfg['output'], np.concatenate(frames), sr)
finally:
    try:
        ws.close()
    except Exception:
        pass
"#;

        let python = find_voxtream()
            .map(|p| {
                p.parent()
                    .unwrap_or(std::path::Path::new("."))
                    .join("python3")
            })
            .unwrap_or_else(|| PathBuf::from("python3"));

        let output = Command::new(&python)
            .arg("-c")
            .arg(script)
            .arg(&config_path)
            .output()
            .context("failed to run voxtream WebSocket client")?;

        let _ = std::fs::remove_file(tmp.path());

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            anyhow::bail!("voxtream WS client failed: {stderr}\nstdout: {stdout}");
        }

        Ok(())
    }
}

impl Drop for VoxtreamServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        eprintln!("[daemon] voxtream-server stopped.");
    }
}

pub struct VoxtreamBackend;

/// Cached result of `find_voxtream` — the probe spawns a subprocess, so cache it
/// per-process instead of re-running on every `is_available()`/`speak()` call
/// (review VX4).
static VOXTREAM_BIN: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();

/// Find the voxtream binary — check PATH first, then common venv locations.
/// Result is cached for the process lifetime.
pub fn find_voxtream() -> Option<PathBuf> {
    VOXTREAM_BIN.get_or_init(find_voxtream_uncached).clone()
}

fn find_voxtream_uncached() -> Option<PathBuf> {
    // Check PATH
    if let Ok(status) = Command::new("voxtream")
        .arg("--help")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        if status.success() {
            return Some(PathBuf::from("voxtream"));
        }
    }

    // Check common venv locations
    let candidates = [
        dirs::home_dir().map(|h| h.join(".local/venvs/voxtream/bin/voxtream")),
        dirs::home_dir().map(|h| h.join(".venvs/voxtream/bin/voxtream")),
        dirs::home_dir().map(|h| h.join("venvs/voxtream/bin/voxtream")),
    ];

    candidates
        .into_iter()
        .flatten()
        .find(|candidate| candidate.exists())
}

impl TtsBackend for VoxtreamBackend {
    fn name(&self) -> &str {
        "voxtream"
    }

    fn synthesize(&self, text: &str, opts: &SpeakOptions) -> Result<super::AudioBuffer> {
        let bin = find_voxtream().context(
            "voxtream not found. Install it:\n\
             python3.11 -m venv ~/.local/venvs/voxtream\n\
             ~/.local/venvs/voxtream/bin/pip install \"voxtream>=0.2\"\n\
             brew install espeak-ng",
        )?;

        let tmp = tempfile::NamedTempFile::new().context("failed to create temp file")?;
        let wav_path = tmp.path().with_extension("wav");
        let wav_str = wav_path.to_string_lossy().to_string();

        // Config files are stored in ~/.config/vox/voxtream/
        let config_dir = config::config_dir().join("voxtream");
        let generator_config = config_dir.join("generator.json");
        let rate_config = config_dir.join("speaking_rate.json");
        if !generator_config.exists() {
            anyhow::bail!(
                "VoXtream2 config not found at {}. Clone the repo configs:\n\
                 git clone --depth 1 https://github.com/herimor/voxtream.git /tmp/voxtream-repo\n\
                 mkdir -p ~/.config/vox/voxtream\n\
                 cp /tmp/voxtream-repo/configs/*.json ~/.config/vox/voxtream/",
                generator_config.display()
            );
        }

        eprintln!("Loading VoXtream2 model (first run may download ~500MB)...");

        let mut cmd = Command::new(&bin);
        cmd.arg("-t").arg(text);
        cmd.arg("-o").arg(&wav_str);
        cmd.arg("-c").arg(&generator_config);

        // Prompt audio is required — use ref_audio (clone) or generate a default
        let prompt_path = match opts.ref_audio {
            Some(ref path) => PathBuf::from(path),
            None => default_prompt_audio()?,
        };
        cmd.arg("-pa").arg(&prompt_path);

        // Always pass speaking rate config (voxtream requires it)
        cmd.arg("--spk-rate-config").arg(&rate_config);

        // Speaking rate (syllables per second)
        if let Some(rate) = opts.rate {
            cmd.arg("-fs");
            cmd.arg("--spk-rate").arg(format!("{}.0", rate));
        }

        let output = cmd
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .context("failed to execute voxtream")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("VoXtream2 TTS failed: {stderr}");
        }

        // Load the generated WAV into an in-memory buffer.
        let buffer = load_wav(&wav_path)?;
        let _ = std::fs::remove_file(&wav_path);
        Ok(buffer)
    }

    fn speak(&self, text: &str, opts: &SpeakOptions) -> Result<()> {
        let buffer = self.synthesize(text, opts)?;
        audio::play(&buffer, opts.volume)
    }

    fn list_voices(&self) -> Result<Vec<String>> {
        // VoXtream2 is zero-shot — any audio prompt works as a "voice"
        Ok(vec![
            "(zero-shot: use --voice with a clone name, or provide any audio prompt)".into(),
        ])
    }

    fn is_available(&self) -> bool {
        find_voxtream().is_some()
    }
}
