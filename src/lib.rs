//! vox — Cross-platform TTS CLI with MCP server for AI assistants.
//!
//! Six backends: `say` (macOS native), `piper` (pure Rust/ONNX), `qwen-native` (pure Rust),
//! `voxtream` (zero-shot voice cloning), `qwen` (MLX Python), `kokoro` (pure Rust/ONNX).
//! Exposes 14 MCP tools over stdio for integration with
//! Claude Code, Cursor, VS Code, and 11 other AI tools.

pub mod audio;
pub mod backend;
#[cfg(target_os = "macos")]
pub mod chat;
pub mod clone;
pub mod config;
pub mod daemon;
pub mod db;
pub mod init;
pub mod input;
pub mod mcp;
pub mod pack;
#[cfg(target_os = "macos")]
pub mod stt;
pub mod tui;
