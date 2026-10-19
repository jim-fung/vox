//! Qwen TTS backend — MLX-Audio Python on Apple Silicon (macOS only).
//!
//! Supports voice cloning via ref_audio/ref_text. Multi-chunk pipeline overlaps
//! generation with playback to reduce perceived latency on long texts.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

use super::{SpeakOptions, TtsBackend};
use crate::audio;
use crate::chat::sentence::MIN_CHUNK_CHARS;

pub struct QwenBackend;

const DEFAULT_MODEL: &str = "mlx-community/Qwen3-TTS-12Hz-0.6B-Base-bf16";

impl QwenBackend {
    pub fn build_generate_command(text: &str, opts: &SpeakOptions) -> Command {
        let model = opts.model.as_deref().unwrap_or(DEFAULT_MODEL);
        let mut cmd = Command::new("python3");
        cmd.arg("-m").arg("mlx_audio.tts.generate");
        cmd.arg("--text").arg(text);
        cmd.arg("--model").arg(model);
        cmd.arg("--play");
        cmd.arg("--stream");
        Self::apply_voice_opts(&mut cmd, opts);
        cmd
    }

    /// Like `build_generate_command` but generates to file (no `--play`/`--stream`),
    /// with `current_dir` set to `output_dir` so the WAV lands there.
    pub fn build_generate_command_to_file(
        text: &str,
        opts: &SpeakOptions,
        output_dir: &Path,
    ) -> Command {
        let model = opts.model.as_deref().unwrap_or(DEFAULT_MODEL);
        let mut cmd = Command::new("python3");
        cmd.arg("-m").arg("mlx_audio.tts.generate");
        cmd.arg("--text").arg(text);
        cmd.arg("--model").arg(model);
        Self::apply_voice_opts(&mut cmd, opts);
        cmd.current_dir(output_dir);
        cmd
    }

    fn apply_voice_opts(cmd: &mut Command, opts: &SpeakOptions) {
        if let Some(ref voice) = opts.voice {
            cmd.arg("--voice").arg(voice);
        }
        if let Some(ref lang) = opts.lang {
            cmd.arg("--lang_code").arg(lang);
        }
        if let Some(ref ref_audio) = opts.ref_audio {
            cmd.arg("--ref_audio").arg(ref_audio);
        }
        if let Some(ref ref_text) = opts.ref_text {
            cmd.arg("--ref_text").arg(ref_text);
        }
    }

    /// Split text into sentences for chunked generation.
    /// Small consecutive sentences are merged to reduce subprocess overhead.
    pub fn split_sentences(text: &str) -> Vec<String> {
        let mut raw = Vec::new();
        let mut current = String::new();

        for c in text.chars() {
            current.push(c);
            if matches!(c, '.' | '!' | '?' | ';') {
                let trimmed = current.trim().to_string();
                if !trimmed.is_empty() {
                    raw.push(trimmed);
                }
                current.clear();
            }
        }
        let trimmed = current.trim().to_string();
        if !trimmed.is_empty() {
            raw.push(trimmed);
        }

        // Merge small consecutive sentences to reduce generation calls
        let mut merged = Vec::new();
        let mut buf = String::new();

        for sentence in raw {
            if buf.is_empty() {
                buf = sentence;
            } else if buf.len() + sentence.len() + 1 < MIN_CHUNK_CHARS {
                buf.push(' ');
                buf.push_str(&sentence);
            } else {
                merged.push(buf);
                buf = sentence;
            }
        }
        if !buf.is_empty() {
            merged.push(buf);
        }
        merged
    }
}

/// Find the first `audio_*.wav` file in a directory.
fn find_wav_in_dir(dir: &Path) -> Result<PathBuf> {
    let mut wavs: Vec<PathBuf> = std::fs::read_dir(dir)
        .context("failed to read chunk directory")?
        .flatten()
        .filter_map(|e| {
            let name = e.file_name();
            let s = name.to_string_lossy();
            if s.starts_with("audio_") && s.ends_with(".wav") {
                Some(e.path())
            } else {
                None
            }
        })
        .collect();
    wavs.sort();
    wavs.into_iter()
        .next()
        .context("no audio_*.wav file found in chunk directory")
}

impl TtsBackend for QwenBackend {
    fn name(&self) -> &str {
        "qwen"
    }

    fn speak(&self, text: &str, opts: &SpeakOptions) -> Result<()> {
        let chunks = Self::split_sentences(text);

        if chunks.len() <= 1 {
            // Single chunk: use --play --stream for best latency. Run in a tempdir
            // so mlx_audio's stray audio_*.wav files don't pollute the user's CWD
            // (review QW2) — previously cleanup_cwd_wav swept the real CWD.
            let tempdir = tempfile::tempdir().context("failed to create temp directory")?;
            for chunk in &chunks {
                let mut cmd = Self::build_generate_command(chunk, opts);
                cmd.current_dir(tempdir.path());
                let status = cmd
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .context(
                        "Failed to run mlx_audio. Is mlx-audio installed? (pip install mlx-audio)",
                    )?;
                if !status.success() {
                    anyhow::bail!("mlx_audio generation failed with status {status}");
                }
            }
            // tempdir dropped here → stray files cleaned up automatically.
            return Ok(());
        }

        // Multiple chunks: pipeline — generate to file, overlap playback with next generation
        let tempdir = tempfile::tempdir().context("failed to create temp directory")?;
        let chunk_dirs: Vec<PathBuf> = (0..chunks.len())
            .map(|i| tempdir.path().join(format!("chunk_{i}")))
            .collect();
        for d in &chunk_dirs {
            std::fs::create_dir(d).context("failed to create chunk sub-directory")?;
        }

        // Generate chunk 0 synchronously
        let status = Self::build_generate_command_to_file(&chunks[0], opts, &chunk_dirs[0])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("Failed to run mlx_audio. Is mlx-audio installed? (pip install mlx-audio)")?;
        if !status.success() {
            anyhow::bail!("mlx_audio generation failed for chunk 0 with status {status}");
        }

        // Start playing chunk 0
        let wav0 = find_wav_in_dir(&chunk_dirs[0])?;
        let mut play_handle = audio::play_wav_async(&wav0)?;

        // Start generating chunk 1 (if exists)
        let mut gen_child: Option<std::process::Child> = if chunks.len() > 1 {
            Some(
                Self::build_generate_command_to_file(&chunks[1], opts, &chunk_dirs[1])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .context("failed to spawn generation for chunk 1")?,
            )
        } else {
            None
        };

        // Pipeline loop: for chunks 1..N
        for i in 1..chunks.len() {
            // Wait for generation of chunk i to finish
            if let Some(mut child) = gen_child.take() {
                let status = child
                    .wait()
                    .context(format!("generation failed for chunk {i}"))?;
                if !status.success() {
                    anyhow::bail!("mlx_audio generation failed for chunk {i} with status {status}");
                }
            }

            // Wait for playback of chunk i-1 to finish
            play_handle.wait()?;

            // Start playing chunk i
            let wav_i = find_wav_in_dir(&chunk_dirs[i])?;
            play_handle = audio::play_wav_async(&wav_i)?;

            // Start generating chunk i+1 (if exists)
            if i + 1 < chunks.len() {
                gen_child = Some(
                    Self::build_generate_command_to_file(&chunks[i + 1], opts, &chunk_dirs[i + 1])
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()
                        .context(format!("failed to spawn generation for chunk {}", i + 1))?,
                );
            }
        }

        // Wait for last playback to finish
        play_handle.wait()?;

        // tempdir is cleaned up automatically on drop
        Ok(())
    }

    fn list_voices(&self) -> Result<Vec<String>> {
        Ok(vec![
            "Chelsie".into(),
            "Aidan".into(),
            "Luna".into(),
            "Ryan".into(),
        ])
    }

    fn is_available(&self) -> bool {
        Command::new("python3")
            .arg("-c")
            .arg("import mlx_audio")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }
}
