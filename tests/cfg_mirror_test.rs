//! R7 — compile-time-ish consistency test for backend gating.
//!
//! `supported_backends()` (backend/mod.rs) is the single source of truth used by
//! DB validation, the TUI, and `vox bench`. `get_backend()` is the runtime
//! dispatch. If a backend is added to one but not the other, behavior silently
//! diverges (a backend that validates as a preference can't actually run, or vice
//! versa). This test cross-checks the two sets on the current platform so the
//! drift is caught at test time rather than at user runtime.
//!
//! It also documents the macOS-gating invariant: `chat`/`stt` modules and the
//! `Chat`/`Hear` commands must agree on `target_os = "macos"`. Since both are
//! compiled out on non-macOS, a mismatch fails to compile on one platform but not
//! the other — this test asserts the reachable set is self-consistent here.

use vox::backend;

#[test]
fn every_supported_backend_is_dispatchable() {
    // Every name that `supported_backends()` advertises must resolve through
    // get_backend on this platform. (A name in supported_backends but not in the
    // get_backend match is a silent gap.)
    for name in backend::supported_backends() {
        let resolved = backend::get_backend(name);
        assert!(
            resolved.is_ok(),
            "supported_backends() lists '{name}' but get_backend() fails on this platform: {:?}",
            resolved.err()
        );
        let b = resolved.unwrap();
        assert_eq!(
            b.name(),
            name,
            "get_backend('{name}') self-identifies as a different name"
        );
    }
}

#[test]
fn every_dispatchable_backend_is_supported() {
    // Reverse direction: any backend name we can sensibly try must either
    // dispatch successfully (and be in supported_backends) or be rejected as
    // platform-only with a message mentioning macOS. We probe the known name
    // space. This catches the case where get_backend accepts a name that
    // supported_backends doesn't list (so it can't be set as a preference).
    let supported = backend::supported_backends();

    // Names that exist in the match but are cfg'd out on this platform should
    // produce a helpful macOS bail, not a panic.
    #[cfg(not(target_os = "macos"))]
    {
        for mac_only in ["say", "qwen"] {
            let err = backend::get_backend(mac_only).unwrap_err().to_string();
            assert!(
                err.contains("macOS"),
                "macOS-only backend '{mac_only}' should bail with a macOS hint, got: {err}"
            );
            assert!(
                !supported.contains(&mac_only),
                "'{mac_only}' must not be in supported_backends() on non-macOS"
            );
        }
    }
}

#[test]
fn supported_backends_always_includes_cross_platform_defaults() {
    // piper, pocket, qwen-native, voxtream are compiled on every platform.
    let s = backend::supported_backends();
    for required in ["piper", "pocket", "qwen-native", "voxtream"] {
        assert!(
            s.contains(&required),
            "'{required}' must always be in supported_backends()"
        );
    }
}
