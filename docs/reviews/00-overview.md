# vox — Deep Code & Performance Review: Executive Overview

**Scope:** full `rtk-ai/vox` repository @ `0.15.1` (main, commit `9597c2f`)
**Subject:** ~6,900 LOC Rust TTS CLI + MCP server (7 backends, daemon, voice cloning, streaming chat, sound packs, auto-init for 14 AI tools)
**Method:** line-by-line read of every source file (`src/**/*.rs`), all 15 test files, CI/release workflows, build scripts, install script, and docs; cross-checked claims against code.
**Date:** 2026-08-02

This is the **entry document** for a 9-part review. Each part is a standalone report under `docs/reviews/`. All claims carry `file:line` citations.

---

## 1. What vox is

vox is a cross-platform text-to-speech CLI with a first-class MCP (Model Context Protocol) server so AI coding assistants (Claude Code, Cursor, VS Code Copilot, Zed, Codex, …) can speak aloud. It bundles **7 TTS backends** behind one trait:

| Backend | Engine | Platform | Voice clone |
|---|---|---|---|
| `say` | macOS native `/usr/bin/say` | macOS | ❌ |
| `piper` | ONNX via `piper-rs` | all | ❌ |
| `pocket` | `pocket-tts` (candle, 100M, CPU) — **default** | all | ✓* |
| `qwen-native` | `qwen3-tts-rs` (candle, 0.6B) | all (+Metal/CUDA) | ✓ |
| `kokoro` | `kokoro-tts` (ONNX, opt-in feature) | all | ❌ |
| `voxtream` | external VoXtream2 (PyTorch 0.5B, subprocess) | all (+CUDA/MPS) | ✓ |
| `qwen` | MLX-Audio (Python subprocess) | macOS (Apple Silicon) | ✓ |

State (preferences, voice clones, usage logs) lives in SQLite (`rusqlite`, WAL). A lazy HTTP daemon keeps heavy models warm. A TUI (`vox setup`) and an auto-config installer (`vox init`) round out the surface.

---

## 2. Overall assessment

vox is a **well-engineered, genuinely useful** single-binary TTS tool. The code is idiomatic Rust, mostly clean, and the security posture on the SQLite + preference path is *strong* — parameterized queries everywhere, whitelist-validated keys, a thoughtful test suite for injection/traversal (see `tests/security_test.rs`). For a project of this maturity it is above average.

That said, the review surfaced a set of **concrete, fixable issues** that matter in production:

- A handful of **real correctness bugs** (UTF-8 slicing panic in MCP, volume flag silently ignored on macOS piper, duplicate-clobber MCP init, `/tmp` path collisions for recording).
- **Performance anti-patterns** around the model-cache design (per-process `Mutex<Option<T>>` defeats cross-process caching; CLI cold-starts every call) and a hand-rolled single-threaded HTTP/1.1 daemon with no request size limit.
- **Documentation drift** — README, `lib.rs` doc-comment, `docs/ARCHITECTURE.md`, Homebrew formula, and MCP tool descriptions all still describe **4 backends** (or "three") when there are now **7**; the French-only CLAUDE.md instructions are baked in.
- **Supply-chain hygiene** — two `git` dependencies are not pinned to a rev/tag (`piper-rs`, `qwen3-tts-rs`), making builds non-reproducible.
- **CI gaps** — no `cargo audit`/supply-chain scan, no coverage, dependencies not deduped/audited, Homebrew `desc`/formula Intel-Mac branch reference stale artifacts.

None of these are fatal; most are one-day fixes. Severity and counts below.

---

## 3. Findings at a glance

### 3.1 By severity

```mermaid
%%{init: {"theme": "default"}}%%
xychart-beta
    title "Findings by severity (count)"
    x-axis ["Critical", "High", "Medium", "Low", "Info"]
    y-axis "Count" 0 --> 14
    bar [2, 8, 16, 13, 9]
```

- **Critical (2):** MCP UTF-8 byte-slice panic on multi-byte text (`src/mcp.rs:592`); unpinned git dependencies break reproducibility + accept arbitrary upstream code on next build (`Cargo.toml`).
- **High (8):** volume flag ignored on macOS piper (`src/backend/piper.rs:217-223`); `vox init` clobbers existing MCP entries instead of no-oping (`src/init.rs:213-218`); `/tmp/vox_hear_input.wav` & `vox_chat_input.wav` fixed-name collisions across users/processes (`src/mcp.rs:939`, `src/main.rs:483`, `src/chat/streaming.rs:117`); daemon has no request body size cap → trivial memory DoS (`src/daemon.rs:393-396`); daemon binds with no auth token (any local user can speak/drive) (`src/daemon.rs:336`); speak request volume not validated before deserialization path; voxtream prompt generation writes world-readable to `/tmp`; `pack::play` uses `subsec_nanos()` modulo as RNG (weak, biased, predictable).
- **Medium (16):** per-process model cache means every CLI invocation cold-starts (the daemon only helps when running); hand-rolled HTTP parser is single-threaded & fragile; `OnceLock` env-var set is `unsafe` (`piper.rs:61`); streaming chat hardcodes French strings; SSE buffer grows unbounded for malformed streams; `is_kokoro_voice` byte-index check is unsafe on non-ASCII; no graceful shutdown draining for voxtream child; `apply_wav_gain` downcasts float→i16 with floor (DC bias); TUI has no error surface for backend failure beyond status text; etc.
- **Low / Info:** duplicated command-building boilerplate; magic constants; French hardcoded in `chat/mod.rs` SYSTEM_PROMPT; Homebrew formula `desc` stale; localized READMEs out of sync; `lib.rs` doc says "14 tools" / "four backends".

### 3.2 Dimension scores

```mermaid
%%{init: {"theme": "default", "themeVariables": {"textColor": "#1f2937"}}}%%
radar-beta
    title "vox — dimension scores (0–10)"
    axis "Architecture", "Backends", "Concurrency", "Security", "Build/CI", "Testing", "CLI/UX/MCP", "Docs/Deps"
    curve [7.5, 7.0, 5.5, 6.5, 6.0, 6.5, 7.0, 5.5]
    max 10
```

Interpretation: **strong architecture & backend abstraction**, **adequate but uneven security** (great on SQL, weak on the local daemon surface), **mediocre concurrency** (right patterns, several blocking-in-async hotspots + process model mismatch), **weakest area is docs/dependency hygiene** (drift, unpinned git deps).

---

## 4. Top priorities (ranked)

| # | Finding | Effort | Impact | Where fixed |
|---|---|---|---|---|
| 1 | Pin `piper-rs` & `qwen3-tts-rs` git deps to a rev/tag | XS | reproducible, auditable builds | `Cargo.toml` |
| 2 | Fix MCP UTF-8 panic: slice on `chars`, not bytes | XS | no crash on CJK/emoji in tool text | `src/mcp.rs:592-594` |
| 3 | `vox init` should no-op when MCP entry exists (don't clobber) | XS | idempotency promise holds | `src/init.rs:213-218` |
| 4 | Apply volume + gain uniformly across all backends (piper macOS path) | S | `--volume` actually works everywhere | `src/backend/piper.rs` |
| 5 | Daemon: bind+auth token in PID file; cap body size; concurrent reads | S | closes local attack surface | `src/daemon.rs` |
| 6 | Unique temp filenames (`tempfile::NamedTempFile`) for record paths | XS | no cross-user/cross-process collision | mcp/main/streaming |
| 7 | Refresh docs to 7 backends (README, lib.rs, ARCHITECTURE, formula) | S | accuracy/trust | docs + comments |
| 8 | Add `cargo audit` + `cargo deny` to CI; dedupe deps | S | supply chain | `.github/workflows` |
| 9 | Replace `subsec_nanos()` RNG with `rand` for pack sound pick | XS | unbiased randomness | `src/pack.rs:182` |
| 10 | Move model cache to a long-lived server process (or expand daemon to all backends) | L | kills 5–60s CLI cold starts | `daemon.rs` + backends |

---

## 5. What's genuinely good

Worth naming explicitly so the "fix list" isn't read in a vacuum:

- **Backend trait is minimal and honest** (`src/backend/mod.rs:48-53`) — 4 methods, each backend self-contained.
- **SQL layer is exemplary for this size** — 100% parameterized, `?N` everywhere, whitelist-validated preference keys, migration via introspection (`db.rs:90-93`). The security test suite actively tries injection/traversal/null-bytes.
- **External commands never go through a shell** — every `std::process::Command` uses separate args (verified across `say`, `qwen`, `voxtream`, `rec`, `python3`). A test even asserts `python3` not `sh`.
- **Streaming TTS pipeline is well thought-out** — sentence accumulator + mpsc channels + cosine crossfade between synthesized chunks (`chat/streaming.rs`). Real engineering to minimize perceived latency.
- **`panic = abort` + LTO + strip** release profile is appropriate for a distributable CLI.
- **CI runs clippy with `-D warnings` and `cargo fmt --check`** across the matrix.
- **build.rs cleverly solves** espeak-ng's hard-coded data-path problem by embedding `espeak-ng-data` into the binary and extracting at first use.

---

## 6. Reading order

| Doc | Covers |
|---|---|
| [`01-architecture.md`](01-architecture.md) | Module graph, layering, trait design, cohesion/coupling |
| [`02-backends.md`](02-backends.md) | Per-backend deep dive, model caching, audio, packs, cloning |
| [`03-concurrency-daemon.md`](03-concurrency-daemon.md) | Daemon design, async/blocking, locks, streaming, races |
| [`04-security.md`](04-security.md) | SQL, traversal, secrets, daemon surface, supply chain |
| [`05-build-cicd.md`](05-build-cicd.md) | Cargo, features, build.rs, CI matrix, release pipeline, install.sh |
| [`06-testing.md`](06-testing.md) | Coverage matrix, test quality, flakiness, gaps |
| [`07-cli-ux-mcp.md`](07-cli-ux-mcp.md) | CLI flags, TUI, MCP protocol, voice-clone UX |
| [`08-docs-dependencies.md`](08-docs-dependencies.md) | Doc drift, dep table, feature flags, Cargo.lock |
| [`09-consolidated-findings.md`](09-consolidated-findings.md) | Consolidated issue table + roadmap |

> Conventions: every finding cites `path:line` against commit `9597c2f`. "S/M/L" effort is T-shirt sizing. Mermaid diagrams render in GitHub.
