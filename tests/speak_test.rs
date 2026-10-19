//! R1 — tests for the shared speak resolution.
//!
//! The headline case is the A4 regression: before R1, the MCP path checked a
//! smaller clone-capable set than the CLI, so a voice clone requested on the
//! `pocket` backend was silently switched to `qwen-native` and the clone was
//! lost. These tests assert the shared `resolve()` keeps `pocket` (and the other
//! clone-capable backends) unchanged when a clone resolves.

use vox::db;
use vox::speak::{self, SpeakInput};

/// Add a clone to the DB and return the name used.
fn add_clone(conn: &db::Connection, name: &str) -> String {
    // A nonexistent audio path is fine — resolve() only looks up the DB row, it
    // does not validate the file (that happens inside the backend at dispatch).
    let path = format!("/tmp/{name}.wav");
    db::add_clone(conn, name, &path, Some("reference text")).unwrap();
    name.to_string()
}

#[test]
fn clone_on_pocket_keeps_pocket_backend() {
    // The A4 fix: pocket is clone-capable, so no auto-switch.
    let conn = db::open_in_memory().unwrap();
    let name = add_clone(&conn, "myvoice");
    let input = SpeakInput {
        text: "hello".into(),
        backend_override: Some("pocket".into()),
        voice: Some(name),
        ..Default::default()
    };
    let resolved = speak::resolve(&conn, &input).unwrap();
    assert_eq!(
        resolved.backend, "pocket",
        "pocket is clone-capable; must not switch"
    );
    // Voice cleared (clone name not passed as literal voice).
    assert_eq!(resolved.opts.voice, None);
    // ref_audio populated from the clone.
    assert_eq!(resolved.opts.ref_audio.as_deref(), Some("/tmp/myvoice.wav"));
}

#[test]
fn clone_on_voxtream_keeps_voxtream_backend() {
    let conn = db::open_in_memory().unwrap();
    let name = add_clone(&conn, "vc");
    let input = SpeakInput {
        text: "hello".into(),
        backend_override: Some("voxtream".into()),
        voice: Some(name),
        ..Default::default()
    };
    let resolved = speak::resolve(&conn, &input).unwrap();
    assert_eq!(resolved.backend, "voxtream");
}

#[test]
fn clone_on_non_capable_backend_switches_to_platform_default() {
    // piper is NOT clone-capable, so a clone on piper switches to the platform
    // default clone backend (qwen on macOS, qwen-native elsewhere).
    let conn = db::open_in_memory().unwrap();
    let name = add_clone(&conn, "vc2");
    let input = SpeakInput {
        text: "hello".into(),
        backend_override: Some("piper".into()),
        voice: Some(name),
        ..Default::default()
    };
    let resolved = speak::resolve(&conn, &input).unwrap();
    #[cfg(target_os = "macos")]
    assert_eq!(resolved.backend, "qwen");
    #[cfg(not(target_os = "macos"))]
    assert_eq!(resolved.backend, "qwen-native");
}

#[test]
fn unknown_voice_name_passes_through_as_literal() {
    // A voice name that isn't a clone is passed verbatim to the backend as
    // --voice; backend is unchanged.
    let conn = db::open_in_memory().unwrap();
    let input = SpeakInput {
        text: "hello".into(),
        backend_override: Some("piper".into()),
        voice: Some("Chelsie".into()),
        ..Default::default()
    };
    let resolved = speak::resolve(&conn, &input).unwrap();
    assert_eq!(resolved.backend, "piper");
    assert_eq!(resolved.opts.voice.as_deref(), Some("Chelsie"));
    assert_eq!(resolved.opts.ref_audio, None);
}

#[test]
fn no_voice_uses_backend_default() {
    let conn = db::open_in_memory().unwrap();
    let input = SpeakInput {
        text: "hello".into(),
        backend_override: Some("pocket".into()),
        ..Default::default()
    };
    let resolved = speak::resolve(&conn, &input).unwrap();
    assert_eq!(resolved.backend, "pocket");
    assert_eq!(resolved.opts.voice, None);
}

#[test]
fn backend_falls_back_to_lang_default_when_not_overridden() {
    // No backend override + non-English lang → piper (per default_backend_for_lang).
    let conn = db::open_in_memory().unwrap();
    let input = SpeakInput {
        text: "bonjour".into(),
        lang: Some("fr".into()),
        ..Default::default()
    };
    let resolved = speak::resolve(&conn, &input).unwrap();
    assert_eq!(resolved.backend, "piper");
}

#[test]
fn prefs_are_honored_as_fallback() {
    let conn = db::open_in_memory().unwrap();
    db::set_preference(&conn, "backend", "qwen-native").unwrap();
    db::set_preference(&conn, "lang", "en").unwrap();
    let input = SpeakInput {
        text: "hello".into(),
        ..Default::default() // no overrides → prefs win
    };
    let resolved = speak::resolve(&conn, &input).unwrap();
    assert_eq!(resolved.backend, "qwen-native");
}

#[test]
fn args_override_prefs() {
    let conn = db::open_in_memory().unwrap();
    db::set_preference(&conn, "backend", "qwen-native").unwrap();
    let input = SpeakInput {
        text: "hello".into(),
        backend_override: Some("piper".into()),
        ..Default::default()
    };
    let resolved = speak::resolve(&conn, &input).unwrap();
    assert_eq!(
        resolved.backend, "piper",
        "explicit override beats stored pref"
    );
}
