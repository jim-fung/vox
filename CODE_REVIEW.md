# Code Review: vox

Scope: multifaceted read-only review of `rtk-ai/vox` cloned locally, followed by targeted remediation of the highest-priority findings.

## Verification Snapshot

- `lsp_diagnostics`: 0 errors, 27 hints
- `rtk cargo test`: 191 passed, 17 suites
- `cargo fmt -- --check`: passed
- `rtk cargo clippy --all-targets -- -D warnings`: failed before remediation on 4 collapsible-if/match warnings that CI suppresses for one lint class

## Findings

### HIGH: Release formula references removed macOS Intel artifact

File: `.github/workflows/release.yml`

The release matrix no longer builds `x86_64-apple-darwin`, but Homebrew formula generation still parsed and emitted an Intel macOS URL/checksum. This could publish a formula with an empty checksum or dead URL.

Remediation: remove Intel checksum parsing and the Intel Homebrew formula block unless the artifact is restored.

### HIGH: Installer does not verify checksums/signatures

File: `install.sh`

The installer downloaded and extracted release archives without validating `checksums.txt`, signatures, or hashes.

Remediation: download `checksums.txt`, locate the selected archive checksum, and verify with `sha256sum` or `shasum -a 256` before extraction.

### HIGH: Daemon accepts unbounded request bodies

File: `src/daemon.rs`

The HTTP handler trusted `Content-Length` and allocated `vec![0u8; content_length]`. Any local process could force a large allocation against `127.0.0.1`.

Remediation: cap request body size and return `413 Payload Too Large` for oversized requests.

### HIGH: VoXtream daemon builds Python script with insufficient escaping

File: `src/daemon.rs`

`text`, `prompt_audio`, and `output_wav` were interpolated into a Python `-c` script after only replacing double quotes. Backslashes and control characters could break the generated Python source.

Remediation: pass dynamic values as argv/environment data instead of interpolating them into Python source.

### MEDIUM: Sound-pack manifest filenames can path-traverse

File: `src/pack.rs`

Remote manifest `sound.file` values were joined directly under the sounds directory. A malformed or compromised manifest could write/read outside the intended directory via absolute paths or `..` components.

Remediation: reject absolute paths, empty paths, and non-normal path components before downloading, playing, or returning sound paths.

### MEDIUM: Voice clone validation is extension-only

File: `src/clone.rs`

`validate_audio` only checked file existence and extension.

Remediation: add basic metadata validation: path must be a regular non-empty file and below a bounded size limit.

### MEDIUM: Strict local clippy failed

Files: `src/backend/voxtream.rs`, `src/config.rs`, `src/db.rs`

Local `-D warnings` failed on collapsible-if/match lints.

Remediation: simplify the flagged control flow so strict local clippy and CI expectations stay aligned.
