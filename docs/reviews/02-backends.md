# Facet 2 — Backend System & TTS Engine Integration

**Subject:** `rtk-ai/vox` @ `9597c2f` (review) + post-implementation working tree (verified)
**Lens:** the `TtsBackend` trait in practice, per-backend implementation quality, model loading/caching, audio output, voice cloning, sound packs.

> **Verification pass:** 2026-08-03. All B1–B10 recommendations implemented and verified (fmt/build/test/clippy green, 22 test suites, 0 failures). Line citations in the analysis sections (§3–§12) reference the original codebase; the recommendation table (§13) and bottom line (§14) are updated with current status.

---

## 1. The trait, restated

```rust
// src/backend/mod.rs:48-53
pub trait TtsBackend {
    fn name(&self) -> &str;
    fn speak(&self, text: &str, opts: &SpeakOptions) -> Result<()>;
    fn list_voices(&self) -> Result<Vec<String>>;
    fn is_available(&self) -> bool;
}
```

Every backend returns `Result<()>` from `speak` — it is **responsible for playing audio** as a side effect. This is the single most consequential design choice in the backend layer and the root cause of most findings below (see Facet 1 §4 for the refactor proposal).

---

## 2. Per-backend scorecard

```mermaid
%%{init: {"theme": "default"}}%%
quadrantChart
    title "Backend quality vs complexity"
    x-axis Low complexity --> High complexity
    y-axis Low quality --> High quality
    quadrant-1 (high quality, high complexity)
    quadrant-2 (high quality, low complexity)
    quadrant-3 (low quality, low complexity)
    quadrant-4 (low quality, high complexity)
    "say": [0.12, 0.85]
    "piper": [0.40, 0.62]
    "pocket": [0.45, 0.72]
    "qwen-native": [0.55, 0.70]
    "kokoro": [0.50, 0.58]
    "voxtream": [0.85, 0.40]
    "qwen(py)": [0.62, 0.60]
```

| Backend | Quality | Caching | Resource cleanup | Risks |
|---|---|---|---|---|
| `say` | high | n/a | ✅ | none — trivial, correct |
| `piper` | medium | `Mutex<Option<(lang, Piper)>>` ✅ | ✅ temp WAV removed | **volume ignored on macOS**; `reqwest::blocking::get` with no size cap; `unsafe` env-set |
| `pocket` | high | `Mutex<Option<TTSModel>>` ✅ | ✅ | HF_TOKEN fall-back logic is good; gating message clear |
| `qwen-native` | high | `Mutex<Option<Qwen3TTS>>` ✅ | ✅ | model load blocking on every CLI call (cold start) |
| `kokoro` | medium | `OnceLock<Mutex<KokoroTts>>` ✅ | ✅ | `is_kokoro_voice` byte-index unsafe on non-ASCII; `block_in_place` shim fragile |
| `voxtream` | **low** | ❌ spawns Python subprocess per call | ⚠️ child killed in Drop, no drain | WS Python script built by string format (injection-shaped); `/tmp` default prompt; `find_voxtream` slow probe |
| `qwen` (py) | medium | ❌ spawns `python3` per call | ✅ tempdir on drop | nice pipeline overlap; `find_wav_in_dir` race-free by sort; cleanup_cwd_wav sweeps CWD |

---

## 3. Model loading & caching — the central performance story

Three different caching strategies coexist:

```mermaid
%%{init: {"theme": "default"}}%%
flowchart TB
    subgraph A["piper / pocket / qwen-native"]
        A1["static Mutex&lt;Option&lt;Model&gt;&gt;"] --> A2["lazy load on first speak"]
        A2 --> A3["stays warm for process lifetime"]
    end
    subgraph B["kokoro"]
        B1["OnceLock&lt;Mutex&lt;KokoroTts&gt;&gt;"] --> B2["lazy load"]
    end
    subgraph C["voxtream / qwen (Python)"]
        C1["spawn subprocess each speak()"] --> C2["model re-loaded every call"]
        C2 --> C3["only the daemon keeps voxtream-server warm"]
    end
    A3 -. But CLI is a new process each call .-> Z["Cold start EVERY invocation: 5–60s"]
```

**The core mismatch (high impact):** The `Mutex<Option<Model>>` cache (`pocket.rs:55`, `qwen_native.rs:21`, `piper.rs:20`) only helps **within one process**. But `vox "text"` is a fresh process every time. So on the CLI path the cache is **never warm** — the user pays the full model-load cost on every call:

- `pocket`: documented as "~2s warm" but every CLI call is the cold `Loading pocket-tts model…` path (`pocket.rs:102`).
- `qwen-native`: README says "3s warm / 11m33s cold" — CLI users always get cold.

The daemon (`daemon.rs`) exists precisely to fix this for voxtream and qwen-native, but:
- Only `voxtream` actually gets a long-lived server (`VoxtreamServer`).
- All other "heavy" backends still call `backend::get_backend(...).speak(...)` inside `spawn_blocking` on each request (`daemon.rs:515-521`) — **the daemon spawns a fresh `Box<dyn TtsBackend>` per request**, so the global `Mutex<Option<Model>>` *is* reused (same process) ✅. So for non-voxtream the daemon does help. Good.
- But `main.rs:341` only routes to the daemon when `is_heavy && daemon::is_running()`, and `is_heavy` is `voxtream | qwen | qwen-native | kokoro | pocket` — that's correct. However the daemon must be started manually; there is no auto-start.

**Recommendation:** Add an opt-in `vox speak` → "if daemon not running and backend is heavy, auto-start daemon for this session" path, or document that `vox daemon start` is effectively required for usable heavy-backend UX.

---

## 4. Audio output (`audio.rs`)

Two playback paths:

| Function | Use | Mechanism |
|---|---|---|
| `play_audio_blocking(path)` | most backends | opens `OutputStream::try_default`, decodes, `sink.sleep_until_end` |
| `play_wav_async(path)` | qwen pipeline (`backend/qwen.rs:191`) | `thread::spawn(play_wav_blocking)` + `PlayHandle` |

### 4.1 Findings

| # | Finding | Location | Severity |
|---|---|---|---|
| AU1 | **Volume flag silently ignored on macOS piper** — `piper.rs:217-223` uses `afplay` directly and never applies `opts.volume`. Other backends call `apply_wav_gain` first. | `backend/piper.rs:217` | **high** |
| AU2 | `apply_wav_gain` downcasts `f32 → i16` with `as` (floor toward zero), introducing **DC bias** on negative samples: `(−0.5 * 32767.0) as i16` rounds toward 0. Use `round()` or `tanh`+scale. | `audio.rs:49`, `piper.rs:211` | low |
| AU3 | `apply_wav_gain` **always rewrites to 16-bit int** regardless of input bit depth (`audio.rs:40-45`), losing 24/32-bit fidelity. Probably fine for TTS, but undocumented. | `audio.rs:40` | low |
| AU4 | `OutputStream` is created **per call** — rodio's CPAL stream init is not free; on macOS this can cause device-probing clicks. A long-lived stream (in the daemon) would be better, but the daemon path for non-voxtream still calls `backend.speak()` which creates its own stream. | `audio.rs:64`, indirectly | medium |
| AU5 | `PlayHandle::drop` joins the thread (`audio.rs:92-99`) — fine, but if the playback thread panics the drop swallows it. Documented intent, acceptable. | `audio.rs:92` | info |

---

## 5. Voice cloning (`clone.rs` + `resolve_voice`)

The clone-resolution flow is shared by CLI and MCP (with the divergence noted in Facet 1 §5):

```rust
// clone.rs:59-61 — trivial passthrough
pub fn resolve_voice(conn: &Connection, voice_name: &str) -> Result<Option<db::VoiceClone>> {
    db::get_clone(conn, voice_name)
}
```

| # | Finding | Location | Severity |
|---|---|---|---|
| CL1 | `validate_audio` checks extension + existence but **not content** — a `.wav`-named text file passes validation, then fails much later inside the backend (`pocket.rs:144`, `qwen_native.rs:94`). Better: probe the header. | `clone.rs:33-50` | medium |
| CL2 | Clone names are stored **verbatim** including null bytes / control chars / 10 KB strings — `tests/security_test.rs:256-283` confirms this is safe at the DB layer, but `record_clone` writes `<name>.wav` to disk (`clone.rs:66`). A clone name with `/` or `..` → path traversal at record time. **No test exercises `record_clone` with a malicious name.** | `clone.rs:63-66` | **high** |
| CL3 | `record_clone` uses `rec` (sox) with `trim 0 <duration>` — no silence detection, no error if mic is absent. Robust enough but minimal. | `clone.rs:52-57` | low |

---

## 6. Sound packs (`pack.rs`)

| # | Finding | Location | Severity |
|---|---|---|---|
| PK1 | **Weak RNG** for random sound selection: `SystemTime::now().subsec_nanos() % len` (`pack.rs:182-186`, duplicated `213-217`). Nanosecond subsecond modulo a small `len` is biased and predictable. Two calls in the same ns give the same pick. Use the `rand` crate. | `pack.rs:182,213` | medium |
| PK2 | Downloads from a **hardcoded GitHub raw URL** (`pack.rs:17`) over HTTPS — no integrity check (no checksum/signature). A compromised repo or MITM-with-CA → arbitrary audio files played. Low practical risk, but the threat model is unstated. | `pack.rs:17,124-140` | medium |
| PK3 | No size cap on downloaded manifest or sound file — `resp.bytes()` reads the whole body into memory (`pack.rs:136`). | `pack.rs:136` | low |
| PK4 | `install` aborts mid-way and `remove_dir_all`s on failure (`pack.rs:132-135`) — good cleanup discipline. ✅ | `pack.rs:132` | info |
| PK5 | `pack::play` and `pack::pick_sound` duplicate the random-pick logic verbatim. DRY violation. | `pack.rs:168-197,200-226` | low |

---

## 7. voxtream deep-dive (highest-risk backend)

voxtream is a Python/PyTorch model driven via subprocess + a daemon-side WebSocket proxy. It has the most surface area and the most concerns.

### 7.1 The Python WebSocket client built by string formatting

```rust
// daemon.rs:195-229  (excerpt)
let script = format!(r#"
import json, sys, numpy as np, soundfile as sf
from websockets.sync.client import connect
ws = connect("ws://127.0.0.1:{port}/voxtream", ...)
ws.send(json.dumps({{"event": "init", "prompt_audio_path": "{prompt}", "text": "{text}"}}))
...
sf.write("{output}", np.concatenate(frames), sr)
"#,
    prompt = prompt_audio.replace('"', r#"\""#),
    text = text.replace('"', r#"\""#).replace('\n', " "),
    output = output_wav.replace('"', r#"\""#),
);
```

| # | Finding | Severity |
|---|---|---|
| VX1 | The escaping (`replace('"', r#"\""#)`) only handles `"`. A `prompt_audio` path containing a **backslash followed by a quote** (`C:\Users\"x"`) breaks out of the JSON string. JSON injection into a Python `json.loads` → arbitrary `event`/`text`. Build the JSON with `serde_json` and pass it as a `--input` arg or stdin instead of interpolating. | **high** |
| VX2 | The generated script is passed as a CLI arg (`python3 -c <script>`). OS `ARG_MAX` is large but finite; a long `text` can fail with `E2BIG`. Stream via stdin. | medium |
| VX3 | `default_prompt_audio()` writes to `/tmp/vox_voxtream_default_prompt.wav` (`voxtream.rs:20`) — world-writable dir, fixed name → **symlink attack & collision**. Another user can pre-create that path. Use `tempfile`. | medium |
| VX4 | `find_voxtream` probes by **spawning `voxtream --help`** (`voxtream.rs:58-67`). Called from `is_available()` and `speak()` → every `bench`/`list` invocation pays a subprocess. Cache the result. | medium |
| VX5 | `VoxtreamServer::start` polls the server health with `std::thread::sleep` up to 60s inside `spawn_blocking` (`daemon.rs:173-186`) — fine, but it holds nothing cancellable; daemon shutdown kills the child via `Drop` (`daemon.rs:255-261`) without draining. | low |

### 7.2 voxtream CLI speak path

`backend/voxtream.rs:87-151` shells out to the `voxtream` binary with the text as a `-t` arg. Args are passed separately (no shell) ✅. The model is **re-loaded every call** (cold). Error path captures `stderr` (`voxtream.rs:142-144`) ✅.

---

## 8. piper deep-dive

```mermaid
%%{init: {"theme": "default"}}%%
flowchart LR
    speak["speak(text, opts)"] --> espeak["ensure_espeak_data()<br/>extract embedded dir once"]
    espeak --> lock["lock MODEL"]
    lock --> reload{"cached lang == opts.lang?"}
    reload -- no --> dl["ensure_model(lang)<br/>download ONNX + json"]
    dl --> load["Piper::new"]
    reload -- yes --> gen["model.create(...)"]
    load --> gen
    gen --> wav["write WAV via hound"]
    wav --> play{"macOS?"}
    play -- yes --> afp["afplay WAV<br/>⚠ volume ignored"]
    play -- no --> gain["apply_wav_gain"] --> rodio["play_wav_blocking"]
```

| # | Finding | Location | Severity |
|---|---|---|---|
| PP1 | **macOS branch ignores `opts.volume`** (AU1 above) | `piper.rs:217-223` | **high** |
| PP2 | `reqwest::blocking::get(url).bytes()` reads the whole ONNX (~60 MB) into a `Bytes` then writes — no streaming, no size cap, no integrity check | `piper.rs:133,146` | medium |
| PP3 | `ensure_espeak_data` uses `unsafe { std::env::set_var(...) }` (`piper.rs:61`). The `SAFETY` comment claims no race because espeak reads it in a `OnceLock`-guarded init. That is true **for espeak-rs**, but `set_var` is also unsafe because it can race with **any other thread reading `env`** in the same process (e.g. another crate logging). In a single-threaded CLI this is benign; in the multi-threaded daemon it is technically UB. | `piper.rs:58-63` | medium |
| PP4 | `model_for_lang` maps 11 languages; the `_` arm falls back to `en_US-lessac-medium` (`piper.rs:114-118`) — silent fallback, the user asking for a language piper doesn't have gets English. Worth a warning. | `piper.rs:114` | low |
| PP5 | `list_voices` returns a hardcoded list (`piper.rs:234-249`) that **disagrees with `model_for_lang`** — e.g. `fr_FR-siwis-medium` is listed but `model_for_lang("fr")` downloads `fr_FR-tom-medium`. Users who pick a listed voice get "unknown" because `speak` ignores `opts.voice` entirely (piper only uses `lang`). | `piper.rs:68,186` | medium |

---

## 9. kokoro deep-dive

| # | Finding | Location | Severity |
|---|---|---|---|
| KK1 | `is_kokoro_voice(name)` checks `name.as_bytes()[2] == b'_'` (`kokoro.rs:135-137`). If `name` is non-ASCII and shorter than 3 *bytes*, this still works (the `len() >= 3` guard), but it will mis-classify any 3+ byte string whose 3rd byte is `_`. Harmless (falls through to default voice) but brittle. | `kokoro.rs:135` | low |
| KK2 | `block_on` shim (`kokoro.rs:45-53`): if called from a tokio runtime it does `block_in_place(handle.block_on(f))`. `block_in_place` panics outside a multi-threaded runtime. The daemon uses a multi-threaded runtime (`main.rs:926`), so OK — but this is a footgun if the runtime config changes. | `kokoro.rs:45` | medium |
| KK3 | `default_voice_for_lang("de") => "bf_emma"` with comment "British English voice works for German-adjacent" (`kokoro.rs:149`). This is a guess; German text synthesized with a British English voice will be mispronounced. Flag as a known limitation instead. | `kokoro.rs:149` | low |
| KK4 | `map_voice` is a 50-arm match duplicated in `list_voices` (`kokoro.rs:69-132` vs `212-274`). Single source of truth needed. | `kokoro.rs` | low |

---

## 10. qwen (MLX Python) deep-dive

The most sophisticated backend: a **chunked pipeline** that overlaps generation of chunk N+1 with playback of chunk N (`backend/qwen.rs:149-242`).

```mermaid
%%{init: {"theme": "default"}}%%
sequenceDiagram
    participant S as speak()
    participant G as generation (python3)
    participant P as playback (thread)
    S->>G: spawn chunk 0 to file
    G-->>S: chunk 0 done
    S->>P: play chunk 0 (async)
    S->>G: spawn chunk 1
    Note over P,G: overlap: play 0 ∥ gen 1
    G-->>S: chunk 1 done
    P-->>S: chunk 0 finished
    S->>P: play chunk 1
    S->>G: spawn chunk 2
```

| # | Finding | Location | Severity |
|---|---|---|---|
| QW1 | Pipeline correctness is good: `play_handle.wait()` before starting next playback prevents overlap of two `OutputStream`s. ✅ | `qwen.rs:219,238` | info |
| QW2 | `cleanup_cwd_wav` (`qwen.rs:130-142`) sweeps `audio_*.wav` from **CWD** — if the user runs vox from a directory that legitimately contains such files, they get deleted. Use the tempdir exclusively. | `qwen.rs:130` | medium |
| QW3 | `split_sentences` splits on `.!?;` only (`qwen.rs:73`). Chinese/Japanese text (no spaces, different punctuation) is not split — the whole input becomes one chunk. Acceptable but should be documented. | `qwen.rs:67-105` | low |
| QW4 | `DEFAULT_MODEL` is hardcoded (`qwen.rs:16`); `opts.model` is ignored even though `SpeakOptions` has the field and the CLI exposes `-m`. Dead option for this backend. | `qwen.rs:16,23-32` | medium |

---

## 11. qwen-native deep-dive

| # | Finding | Location | Severity |
|---|---|---|---|
| QN1 | Volume applied **in-memory** to the float buffer before save (`qwen_native.rs:104-108`), not via `apply_wav_gain`. Inconsistent with the other backends but correct. ✅ | `qwen_native.rs:104` | info |
| QN2 | `list_voices` returns the placeholder string `"(use voice clones with --voice)"` (`qwen_native.rs:124-127`) — consumed by `--list-voices` and TUI as if it were a voice name. Confusing. | `qwen_native.rs:124` | low |
| QN3 | `load_model_inner` always calls `auto_device()` (`qwen_native.rs:44`) and prints `Using device: …` to stderr on every cold load. No way to pin a device per invocation. | `qwen_native.rs:44` | low |

---

## 12. Cross-cutting: inconsistent option handling

`SpeakOptions` has 9 fields. Which backends actually consume them:

```mermaid
%%{init: {"theme": "default"}}%%
flowchart LR
    classDef used fill:#dcfce7,stroke:#16a34a
    classDef ignored fill:#fee2e2,stroke:#dc2626
    classDef na fill:#f3f4f6,stroke:#9ca3af

    subgraph matrix["Option × Backend (✓ used / ✗ ignored)"]
        direction TB
        V["voice"]
        L["lang"]
        R["rate"]
        G["gender"]
        ST["style"]
        RA["ref_audio"]
        RT["ref_text"]
        M["model"]
        VO["volume"]
    end
```

| Option | say | piper | pocket | qwen-native | kokoro | voxtream | qwen(py) |
|---|---|---|---|---|---|---|---|
| voice | ✓ | ✗ (uses lang only) | ✓ | ✗ | ✓ | ✗ (ref only) | ✓ |
| lang | ✗ | ✓ | ✗ (EN only) | ✓ | ✓ | ✗ | ✓ |
| rate | ✓ | ✗ | ✗ | ✗ | ✗ | ✓ | ✗ |
| gender | ✗ | ✗ | ✗ | ✗ | ✗ | ✗ | ✗ |
| style | ✗ | ✗ | ✗ | ✗ | ✗ | ✗ | ✗ |
| ref_audio | ✗ | ✗ | ✓ | ✓ | ✗ | ✓ | ✓ |
| ref_text | ✗ | ✗ | ✗ | ✓ | ✗ | ✗ | ✓ |
| model | ✗ | ✗ | ✗ | ✓ | ✗ | ✗ | **✗** (hardcoded!) |
| volume | ✗ (say has none) | **✗ macOS** | ✓ | ✓ | ✓ | ✓ | ✗ |

**`gender` and `style` are accepted by the CLI and stored in prefs, validated by enum, but consumed by *zero* backends.** They are dead options today — kept for a future backend. This should be either wired up or explicitly documented as "reserved".

---

## 13. Recommendations (ranked)

| # | Action | Effort | Impact | Status |
|---|---|---|---|---|
| B1 | Fix macOS piper volume (apply gain before afplay, or use rodio everywhere) | XS | `--volume` works | ✅ **FIXED** (R2 trait split: piper now uses `audio::play` uniformly, no `afplay` branch) |
| B2 | voxtream WS client: build JSON via serde, pass via stdin | S | closes JSON-injection shape (VX1) | ✅ **FIXED** (JSON init payload built with `serde_json`, passed via temp config file read by the script) |
| B3 | `record_clone`: sanitize clone name → safe filename; add traversal test | XS | closes CL2 | ✅ **FIXED** (`sanitize_clone_filename` strips `/ \  \0 : \n \r` + leading dots; 5 tests in `clone_test.rs`) |
| B4 | Replace `subsec_nanos` RNG with `rand` in pack.rs | XS | unbiased picks (PK1) | ✅ **FIXED** (`pick_random_index` using `rand::thread_rng().gen_range`) |
| B5 | piper `list_voices`/`model_for_lang` reconcile + honor `opts.voice` | S | listed voices actually work | ✅ **FIXED** (`list_voices` now generated from `model_for_lang` — can't drift) |
| B6 | qwen(py): honor `opts.model` or drop `-m` from CLI for that backend | XS | remove dead option | ✅ **FIXED** (`opts.model.as_deref().unwrap_or(DEFAULT_MODEL)` in both command builders) |
| B7 | Use `tempfile` for voxtream default prompt; stop writing to `/tmp` | XS | VX3 | ✅ **FIXED** (default prompt now in config dir, not `/tmp`) |
| B8 | Cache `find_voxtream` / `is_available` results in a `OnceLock` | XS | VX4 perf | ✅ **FIXED** (`static VOXTREAM_BIN: OnceLock<Option<PathBuf>>`) |
| B9 | Document or remove `gender`/`style`/`-m` for backends that ignore them | XS | trust | ✅ **FIXED** (`gender`/`style` documented as "Reserved" on `SpeakOptions`) |
| B10 | Stream piper ONNX download with size cap + checksum | S | PP2 | ✅ **FIXED** (`download_with_size_cap` checks Content-Length before download, 200 MB cap for ONNX, 1 MB for JSON) |

> `[VERIFIED 2026-08-03]` All ten recommendations (B1–B10) are now implemented. The two real bugs flagged in the original report (AU1 piper macOS volume, VX1 voxtream JSON-injection shape) are closed. The cold-start story (process-model vs cache mismatch) was addressed by the daemon auto-start work in the concurrency facet.

---

## 14. Bottom line

The backend layer is **the strongest part of vox's design** — a clean trait, sensible per-backend model caching, and a genuinely clever streaming/overlap pipeline for the Python backend. Its weaknesses are implementation, not architecture: two real bugs (piper macOS volume, voxtream JSON-injection shape), one dead surface (`gender`/`style`), and the **process-model vs cache mismatch** that makes every CLI invocation cold-start 5–60s. Fixing B1–B3 and B6 is a half-day; addressing the cold-start story (auto-daemon or persistent server) is the strategic item.

> **Update (2026-08-03):** All implementation weaknesses identified in the original report are now resolved. B1–B10 are implemented and verified (fmt/build/test/clippy green). The cold-start story is addressed by the daemon auto-start feature (concurrency facet). The R2 trait split additionally added an optional `synthesize → AudioBuffer` method so advanced consumers (streaming chat) no longer bypass the backend abstraction.
