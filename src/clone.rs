//! Voice cloning — audio validation, microphone recording, clone resolution.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use rusqlite::Connection;

use crate::config;
use crate::db;

const VALID_AUDIO_EXTENSIONS: &[&str] = &["wav", "mp3", "flac", "ogg", "m4a"];

pub fn sox_install_hint() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "Failed to run rec (sox). Is sox installed? (brew install sox)"
    }
    #[cfg(target_os = "linux")]
    {
        "Failed to run rec (sox). Is sox installed? (sudo apt install sox)"
    }
    #[cfg(target_os = "windows")]
    {
        "Failed to run rec (sox). Is sox installed? (choco install sox)"
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        "Failed to run rec (sox). Is sox installed?"
    }
}

pub fn validate_audio(path: &str) -> Result<()> {
    let p = Path::new(path);
    if !p.exists() {
        bail!("Audio file not found: {path}");
    }
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .unwrap_or_default();
    if !VALID_AUDIO_EXTENSIONS.contains(&ext.as_str()) {
        bail!(
            "Unsupported audio format: .{ext}. Supported: {}",
            VALID_AUDIO_EXTENSIONS.join(", ")
        );
    }
    Ok(())
}

pub fn build_record_command(output_path: &str, duration: u32) -> Command {
    let mut cmd = Command::new("rec");
    cmd.arg(output_path);
    cmd.arg("trim").arg("0").arg(duration.to_string());
    cmd
}

pub fn resolve_voice(conn: &Connection, voice_name: &str) -> Result<Option<db::VoiceClone>> {
    db::get_clone(conn, voice_name)
}

/// Sanitize a clone name into a safe filename component. Strips path separators,
/// `..`, and other shell/FS-significant characters so `record_clone` can't write
/// outside the clones directory (review CL2). The DB stores the original name;
/// only the on-disk WAV filename is sanitized.
pub fn sanitize_clone_filename(name: &str) -> String {
    // Replace any path separator or null with underscore, collapse leading dots.
    let safe: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | '\0' | ':' | '\n' | '\r' => '_',
            _ => c,
        })
        .collect();
    // Strip leading dots so the file isn't hidden or ".."-traversable.
    let safe = safe.trim_start_matches('.');
    if safe.is_empty() {
        "clone".to_string()
    } else {
        safe.to_string()
    }
}

pub fn record_clone(name: &str, duration: u32) -> Result<String> {
    let dir = config::clones_dir();
    std::fs::create_dir_all(&dir).context("Failed to create clones directory")?;
    let filename = sanitize_clone_filename(name);
    let output_path = dir.join(format!("{filename}.wav"));
    let output_str = output_path.to_string_lossy().to_string();

    eprintln!("Recording {duration}s of audio... Speak now!");
    let status = build_record_command(&output_str, duration)
        .status()
        .context(sox_install_hint())?;
    if !status.success() {
        bail!("Recording failed with status {status}");
    }
    eprintln!("Recording saved to {output_str}");
    Ok(output_str)
}
