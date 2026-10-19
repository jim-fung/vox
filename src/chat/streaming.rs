use std::io::{self, Write};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use rodio::buffer::SamplesBuffer;
use rodio::{OutputStream, Sink};

use super::claude_api::{ClaudeStreamer, StreamEvent};
use super::sentence::{STREAMING_MIN_CHUNK_CHARS, SentenceAccumulator};
use super::{ChatConfig, Message, is_exit, record_until_enter, speak_text};
use crate::backend::qwen_native;
use crate::stt;

enum TtsCommand {
    Speak(String),
    Done,
    Cancel,
}

/// Listen for Ctrl-C once and notify the active response loop without terminating chat.
fn install_cancellation_listener() -> Result<mpsc::Receiver<()>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let result = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed to create cancellation runtime")
            .and_then(|rt| {
                rt.block_on(async move {
                    loop {
                        tokio::signal::ctrl_c().await?;
                        if tx.send(()).is_err() {
                            return Ok(());
                        }
                    }
                })
            });
        if let Err(error) = result {
            eprintln!("Warning: unable to listen for cancellation: {error}");
        }
    });
    Ok(rx)
}

/// Crossfade duration in milliseconds between consecutive TTS sentences.
/// 20ms is enough to eliminate clicks without audible blending artifacts.
const CROSSFADE_MS: usize = 20;

/// Apply cosine fade-in to the first `n` samples.
fn fade_in(samples: &mut [f32], n: usize) {
    let n = n.min(samples.len());
    for (i, sample) in samples[..n].iter_mut().enumerate() {
        let t = i as f32 / n as f32;
        *sample *= (t * std::f32::consts::FRAC_PI_2).sin();
    }
}

/// Apply cosine fade-out to the last `n` samples.
fn fade_out(samples: &mut [f32], n: usize) {
    let len = samples.len();
    let n = n.min(len);
    for i in 0..n {
        let t = i as f32 / n as f32;
        samples[len - n + i] *= (t * std::f32::consts::FRAC_PI_2).cos();
    }
}

/// Crossfade previous sentence's tail into current sentence's head (in-place).
/// Blends `prev_tail` (fading out) with the head of `samples` (fading in).
fn crossfade_into(prev_tail: &[f32], samples: &mut [f32], overlap: usize) {
    let overlap = overlap.min(prev_tail.len()).min(samples.len());
    for i in 0..overlap {
        let t = i as f32 / overlap as f32;
        let fade_out_val = (t * std::f32::consts::FRAC_PI_2).cos();
        let fade_in_val = (t * std::f32::consts::FRAC_PI_2).sin();
        samples[i] = prev_tail[i] * fade_out_val + samples[i] * fade_in_val;
    }
}

/// Which TTS strategy to use in the streaming loop.
enum TtsStrategy {
    /// macOS `say` command — instant, no model loading.
    Say { voice: Option<String> },
    /// qwen-native streaming — neural voice, needs model load.
    QwenNative {
        voice: Option<String>,
        lang: Option<String>,
    },
    /// qwen-native voice cloning — blocking per sentence.
    VoiceClone {
        voice_clone: crate::db::VoiceClone,
        lang: Option<String>,
    },
}

/// Run the streaming chat loop: STT -> Claude streaming -> TTS pipelining.
pub fn run_chat_loop(config: ChatConfig) -> Result<()> {
    let cancel_rx = install_cancellation_listener()?;
    let mut messages: Vec<Message> = Vec::new();
    let (greeting, record_prompt, empty_input, thinking, farewell) =
        super::chat_strings(config.lang.as_deref());

    // Determine TTS strategy: voice clone > say (macOS, fast) > qwen-native
    let strategy = if let Some(vc) = config.voice_clone.clone() {
        TtsStrategy::VoiceClone {
            voice_clone: vc,
            lang: config.lang.clone(),
        }
    } else if cfg!(target_os = "macos") {
        // Use say backend on macOS for instant TTS.
        TtsStrategy::Say {
            voice: config.voice.clone(),
        }
    } else {
        TtsStrategy::QwenNative {
            voice: config.voice.clone(),
            lang: config.lang.clone(),
        }
    };

    // Pre-load qwen-native model in background while greeting plays.
    let needs_qwen = matches!(
        strategy,
        TtsStrategy::QwenNative { .. } | TtsStrategy::VoiceClone { .. }
    );
    let preload_handle = if needs_qwen {
        Some(thread::spawn(|| qwen_native::preload_model(None)))
    } else {
        None
    };

    eprintln!("{greeting}");
    speak_text(greeting, &config)?;

    // Wait for model to finish loading (usually done by now).
    if let Some(handle) = preload_handle
        && let Err(e) = handle
            .join()
            .map_err(|_| anyhow::anyhow!("preload thread panicked"))?
    {
        eprintln!("Warning: model preload failed: {e}");
    }

    let tmp_dir = std::env::temp_dir();
    let audio_path = tmp_dir.join("vox_chat_input.wav");
    let audio_str = audio_path.to_string_lossy().to_string();

    loop {
        eprintln!("\n[{record_prompt}]");
        io::stderr().flush()?;

        record_until_enter(&audio_str)?;

        eprint!("Transcription...");
        io::stderr().flush()?;
        let user_text = stt::transcribe(&audio_str, config.lang.as_deref())?;
        eprintln!(" \"{user_text}\"");

        if user_text.is_empty() {
            eprintln!("{empty_input}");
            continue;
        }

        if is_exit(&user_text) {
            eprintln!("{farewell}");
            speak_text(farewell, &config)?;
            break;
        }

        messages.push(Message {
            role: "user".to_string(),
            content: user_text,
        });

        // --- Streaming response ---
        eprint!("{thinking}");
        io::stderr().flush()?;

        // Shared state used by the response loop, Claude request, and playback.
        let cancelled = Arc::new(AtomicBool::new(false));

        // Channel: main thread -> TTS thread (sentences to speak)
        let (tts_tx, tts_rx) = mpsc::channel::<TtsCommand>();

        // Spawn TTS thread with the chosen strategy
        let tts_handle = match &strategy {
            TtsStrategy::Say { voice } => {
                let voice = voice.clone();
                let cancelled = cancelled.clone();
                thread::spawn(move || -> Result<()> {
                    run_tts_say_loop(tts_rx, voice.as_deref(), cancelled)
                })
            }
            TtsStrategy::QwenNative { voice, lang } => {
                let voice = voice.clone();
                let lang = lang.clone();
                let cancelled = cancelled.clone();
                thread::spawn(move || -> Result<()> {
                    run_tts_streaming_loop(tts_rx, voice, lang, cancelled)
                })
            }
            TtsStrategy::VoiceClone { voice_clone, lang } => {
                let vc = voice_clone.clone();
                let lang = lang.clone();
                let cancelled = cancelled.clone();
                thread::spawn(move || -> Result<()> {
                    run_tts_clone_loop(tts_rx, Some(vc), lang, cancelled)
                })
            }
        };

        // Channel: Claude stream -> main thread (text deltas)
        let (claude_tx, claude_rx) = mpsc::channel::<StreamEvent>();

        // Spawn Claude streaming in a thread
        let api_key = config.api_key.clone();
        let model = config.model.clone();
        let lang = config.lang.clone();
        let msgs = messages.clone();
        let claude_cancelled = cancelled.clone();
        let claude_handle = thread::spawn(move || -> Result<()> {
            let cancelled = claude_cancelled;
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("failed to create tokio runtime")?;
            let streamer = ClaudeStreamer::new();
            rt.block_on(streamer.stream(
                &api_key,
                &model,
                &msgs,
                lang.as_deref(),
                claude_tx,
                cancelled,
            ))
        });

        // Accumulate sentences from Claude stream and send to TTS
        // Use lower threshold (60 chars) for faster first-sentence delivery.
        let mut accumulator = SentenceAccumulator::with_min_chars(STREAMING_MIN_CHUNK_CHARS);
        let mut full_reply = String::new();
        let mut first_token = true;

        let mut response_cancelled = false;
        loop {
            if cancel_rx.try_recv().is_ok() {
                cancelled.store(true, Ordering::Release);
                let _ = tts_tx.send(TtsCommand::Cancel);
                response_cancelled = true;
                break;
            }
            match claude_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(StreamEvent::TextDelta(text)) => {
                    if first_token {
                        eprintln!(" OK");
                        first_token = false;
                    }
                    full_reply.push_str(&text);
                    eprint!("{text}");
                    io::stderr().flush()?;
                    for sentence in accumulator.push(&text) {
                        let _ = tts_tx.send(TtsCommand::Speak(sentence));
                    }
                }
                Ok(StreamEvent::Done) => {
                    if let Some(remaining) = accumulator.flush() {
                        let _ = tts_tx.send(TtsCommand::Speak(remaining));
                    }
                    let _ = tts_tx.send(TtsCommand::Done);
                    break;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        eprintln!(); // newline after streamed text

        // Wait for Claude thread to finish
        if let Err(e) = claude_handle
            .join()
            .map_err(|_| anyhow::anyhow!("Claude thread panicked"))?
        {
            if !response_cancelled {
                eprintln!("Claude API error: {e}");
                let _ = tts_tx.send(TtsCommand::Done);
            }
        }

        // Wait for TTS to finish playback
        if let Err(e) = tts_handle
            .join()
            .map_err(|_| anyhow::anyhow!("TTS thread panicked"))?
        {
            eprintln!("TTS error: {e}");
        }

        if response_cancelled {
            eprintln!("Response cancelled.");
        } else {
            messages.push(Message {
                role: "assistant".to_string(),
                content: full_reply,
            });
        }
    }

    let _ = std::fs::remove_file(&audio_path);
    Ok(())
}

/// TTS thread using macOS `say` command — instant, sentence by sentence.
fn run_tts_say_loop(
    rx: mpsc::Receiver<TtsCommand>,
    voice: Option<&str>,
    cancelled: Arc<AtomicBool>,
) -> Result<()> {
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Ok(());
        }
        let cmd = match rx.recv_timeout(Duration::from_millis(25)) {
            Ok(cmd) => cmd,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        };
        match cmd {
            TtsCommand::Speak(text) => {
                let mut say = Command::new("/usr/bin/say");
                if let Some(v) = voice {
                    say.arg("-v").arg(v);
                }
                let mut child = say.arg(&text).spawn().context("failed to start say")?;
                loop {
                    if cancelled.load(Ordering::Acquire) {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Ok(());
                    }
                    if child.try_wait()?.is_some() {
                        break;
                    }
                    thread::sleep(Duration::from_millis(25));
                }
            }
            TtsCommand::Done => return Ok(()),
            TtsCommand::Cancel => return Ok(()),
        }
    }
}

fn wait_for_sink(sink: &Sink, cancelled: &AtomicBool) {
    while !sink.empty() && !cancelled.load(Ordering::Acquire) {
        thread::sleep(Duration::from_millis(25));
    }
    if cancelled.load(Ordering::Acquire) {
        sink.stop();
    }
}

/// TTS thread using qwen-native: blocking synthesis per sentence with crossfade.
///
/// Uses `synthesize_with_voice()` to generate each sentence as a complete audio
/// buffer. Each sentence plays smoothly (no intra-sentence gaps). While the sink
/// plays sentence N, synthesis of sentence N+1 starts immediately, minimizing
/// inter-sentence gaps. Cosine crossfade eliminates clicks at boundaries.
fn run_tts_streaming_loop(
    rx: mpsc::Receiver<TtsCommand>,
    voice: Option<String>,
    lang: Option<String>,
    cancelled: Arc<AtomicBool>,
) -> Result<()> {
    use crate::backend::{self, SpeakOptions};

    let (_stream, stream_handle) =
        OutputStream::try_default().context("Failed to open audio output device")?;
    let sink = Sink::try_new(&stream_handle).context("Failed to create audio sink")?;
    let backend = backend::get_backend("qwen-native")?;
    let mut prev_tail: Option<(Vec<f32>, u32)> = None;
    let mut is_first_sentence = true;

    loop {
        if cancelled.load(Ordering::Acquire) {
            sink.stop();
            return Ok(());
        }
        let cmd = match rx.recv_timeout(Duration::from_millis(25)) {
            Ok(cmd) => cmd,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        };
        match cmd {
            TtsCommand::Speak(text) => {
                let opts = SpeakOptions {
                    voice: voice.clone(),
                    lang: lang.clone(),
                    ..Default::default()
                };
                let audio = backend.synthesize(&text, &opts)?;
                if cancelled.load(Ordering::Acquire) {
                    sink.stop();
                    return Ok(());
                }
                append_crossfaded_audio(
                    &sink,
                    &mut prev_tail,
                    &mut is_first_sentence,
                    audio.samples,
                    audio.sample_rate,
                );
            }
            TtsCommand::Done => {
                flush_tail(&sink, &mut prev_tail);
                wait_for_sink(&sink, &cancelled);
                return Ok(());
            }
            TtsCommand::Cancel => {
                sink.stop();
                return Ok(());
            }
        }
    }
}

fn append_crossfaded_audio(
    sink: &Sink,
    prev_tail: &mut Option<(Vec<f32>, u32)>,
    is_first_sentence: &mut bool,
    mut samples: Vec<f32>,
    sample_rate: u32,
) {
    let overlap = ((sample_rate as usize * CROSSFADE_MS) / 1000).min(samples.len());
    if let Some((tail, _)) = prev_tail.take() {
        crossfade_into(&tail, &mut samples, overlap);
    } else if *is_first_sentence {
        fade_in(&mut samples, overlap);
        *is_first_sentence = false;
    }
    if samples.len() > overlap {
        let tail = samples.split_off(samples.len() - overlap);
        sink.append(SamplesBuffer::new(1, sample_rate, samples));
        *prev_tail = Some((tail, sample_rate));
    } else {
        *prev_tail = Some((samples, sample_rate));
    }
}

fn flush_tail(sink: &Sink, prev_tail: &mut Option<(Vec<f32>, u32)>) {
    if let Some((mut tail, sample_rate)) = prev_tail.take() {
        let len = tail.len();
        fade_out(&mut tail, len);
        sink.append(SamplesBuffer::new(1, sample_rate, tail));
    }
}

/// TTS thread for voice cloning: blocking synthesis per sentence, with crossfade.
fn run_tts_clone_loop(
    rx: mpsc::Receiver<TtsCommand>,
    voice_clone: Option<crate::db::VoiceClone>,
    lang: Option<String>,
    cancelled: Arc<AtomicBool>,
) -> Result<()> {
    use crate::backend::{self, SpeakOptions};

    let vc = voice_clone.context("voice clone config missing")?;
    let (_stream, stream_handle) =
        OutputStream::try_default().context("Failed to open audio output device")?;
    let sink = Sink::try_new(&stream_handle).context("Failed to create audio sink")?;
    let backend = backend::get_backend("qwen-native")?;
    let mut prev_tail: Option<(Vec<f32>, u32)> = None;
    let mut is_first_sentence = true;

    loop {
        if cancelled.load(Ordering::Acquire) {
            sink.stop();
            return Ok(());
        }
        let cmd = match rx.recv_timeout(Duration::from_millis(25)) {
            Ok(cmd) => cmd,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        };
        match cmd {
            TtsCommand::Speak(text) => {
                let opts = SpeakOptions {
                    ref_audio: Some(vc.ref_audio.clone()),
                    ref_text: vc.ref_text.clone(),
                    lang: lang.clone(),
                    ..Default::default()
                };
                let audio = backend.synthesize(&text, &opts)?;
                if cancelled.load(Ordering::Acquire) {
                    sink.stop();
                    return Ok(());
                }
                append_crossfaded_audio(
                    &sink,
                    &mut prev_tail,
                    &mut is_first_sentence,
                    audio.samples,
                    audio.sample_rate,
                );
            }
            TtsCommand::Done => {
                flush_tail(&sink, &mut prev_tail);
                wait_for_sink(&sink, &cancelled);
                return Ok(());
            }
            TtsCommand::Cancel => {
                sink.stop();
                return Ok(());
            }
        }
    }
}
