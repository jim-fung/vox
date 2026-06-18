//! TTS backend abstraction layer.
//!
//! Each backend implements `TtsBackend` and is selected at runtime via `get_backend()`.
//! Platform-gated: `say` and `qwen` are macOS-only; `kokoro`, `qwen-native`, and `voxtream` are cross-platform.

#[cfg(feature = "kokoro")]
pub mod kokoro;
pub mod piper;
#[cfg(target_os = "macos")]
pub mod qwen;
pub mod qwen_native;
#[cfg(target_os = "macos")]
pub mod say;
pub mod voxtream;

use anyhow::Result;

#[derive(Debug, Clone)]
pub struct SpeakOptions {
    pub voice: Option<String>,
    pub lang: Option<String>,
    pub rate: Option<u32>,
    pub gender: Option<String>,
    pub style: Option<String>,
    pub ref_audio: Option<String>,
    pub ref_text: Option<String>,
    pub model: Option<String>,
    pub volume: f32,
}

impl Default for SpeakOptions {
    fn default() -> Self {
        Self {
            voice: None,
            lang: None,
            rate: None,
            gender: None,
            style: None,
            ref_audio: None,
            ref_text: None,
            model: None,
            volume: 1.0,
        }
    }
}

pub trait TtsBackend {
    fn name(&self) -> &str;
    fn speak(&self, text: &str, opts: &SpeakOptions) -> Result<()>;
    fn list_voices(&self) -> Result<Vec<String>>;
    fn is_available(&self) -> bool;
}

// ── Backend-name registry (single source of truth) ──────────────────────
//
// Every place that needs to know "which backends exist / are clone-capable /
// are daemon-heavy" should read from these, rather than maintaining its own
// hard-coded list (which is how the CLI/MCP clone-switch divergence and the
// db preference-validation gaps crept in).

/// Names of backends compiled into the current build. Single source of truth
/// for backend validation (used by db preference setter, TUI, etc.).
pub fn supported_backends() -> Vec<&'static str> {
    #[allow(unused_mut)]
    let mut v: Vec<&'static str> = vec!["piper", "qwen-native", "voxtream"];
    #[cfg(feature = "kokoro")]
    v.push("kokoro");
    #[cfg(target_os = "macos")]
    {
        v.push("say");
        v.push("qwen");
    }
    v
}

/// Validate that a backend name is known and compiled for this build.
pub fn is_valid_backend(name: &str) -> bool {
    supported_backends().contains(&name)
}

/// Backends that support voice cloning via reference audio.
/// When a clone name is resolved, the effective backend auto-switches to one
/// of these if the requested backend isn't already clone-capable.
pub const CLONE_CAPABLE: &[&str] = &["qwen", "qwen-native", "voxtream"];

/// Backends whose models benefit from staying warm in a long-lived daemon.
pub const DAEMON_HEAVY: &[&str] = &["voxtream", "qwen", "qwen-native", "kokoro"];

/// Default clone-capable backend for the current platform.
pub fn default_clone_backend() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "qwen"
    }
    #[cfg(not(target_os = "macos"))]
    {
        "qwen-native"
    }
}

/// Returns whether a backend name supports voice cloning.
pub fn is_clone_capable(name: &str) -> bool {
    CLONE_CAPABLE.contains(&name)
}

/// Returns whether a backend benefits from daemon routing (warm model).
pub fn is_daemon_heavy(name: &str) -> bool {
    DAEMON_HEAVY.contains(&name)
}

pub fn get_backend(name: &str) -> Result<Box<dyn TtsBackend>> {
    match name {
        #[cfg(feature = "kokoro")]
        "kokoro" => Ok(Box::new(kokoro::KokoroBackend)),
        #[cfg(target_os = "macos")]
        "say" => Ok(Box::new(say::SayBackend)),
        #[cfg(target_os = "macos")]
        "qwen" => Ok(Box::new(qwen::QwenBackend)),
        "qwen-native" => Ok(Box::new(qwen_native::QwenNativeBackend)),
        "voxtream" => Ok(Box::new(voxtream::VoxtreamBackend)),
        "piper" => Ok(Box::new(piper::PiperBackend)),
        #[cfg(not(target_os = "macos"))]
        "say" | "qwen" => {
            anyhow::bail!("Backend '{name}' is only available on macOS. Use 'qwen-native' instead.")
        }
        _ => anyhow::bail!("Unknown backend: {name}"),
    }
}
