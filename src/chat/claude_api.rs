use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};

use anyhow::{Context, Result};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};

use super::{API_URL, API_VERSION, MAX_TOKENS, Message, system_prompt};

/// Upper bound for an incomplete SSE line. Anthropic events are small; anything
/// larger without a line terminator is malformed and must not grow unbounded.
const MAX_SSE_LINE_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub enum StreamEvent {
    TextDelta(String),
    Done,
}

#[derive(Serialize)]
struct StreamRequest {
    model: String,
    max_tokens: u32,
    system: String,
    messages: Vec<Message>,
    stream: bool,
}

#[derive(Deserialize)]
struct SseData {
    #[serde(rename = "type")]
    type_field: String,
    delta: Option<SseDelta>,
}

#[derive(Deserialize)]
struct SseDelta {
    #[serde(rename = "type")]
    type_field: Option<String>,
    text: Option<String>,
}

#[derive(Serialize)]
pub struct ClaudeRequest {
    pub model: String,
    pub max_tokens: u32,
    pub system: String,
    pub messages: Vec<Message>,
}

#[derive(Deserialize)]
pub struct ClaudeResponse {
    pub content: Vec<ContentBlock>,
}

#[derive(Deserialize)]
pub struct ContentBlock {
    pub text: String,
}

pub fn build_claude_request(model: &str, messages: &[Message]) -> ClaudeRequest {
    build_claude_request_for_lang(model, messages, None)
}

pub fn build_claude_request_for_lang(
    model: &str,
    messages: &[Message],
    lang: Option<&str>,
) -> ClaudeRequest {
    ClaudeRequest {
        model: model.to_string(),
        max_tokens: MAX_TOKENS,
        system: system_prompt(lang).to_string(),
        messages: messages.to_vec(),
    }
}

pub fn parse_claude_response(body: &str) -> Result<String> {
    let resp: ClaudeResponse =
        serde_json::from_str(body).context("Failed to parse Claude API response")?;
    resp.content
        .first()
        .map(|b| b.text.clone())
        .ok_or_else(|| anyhow::anyhow!("Empty response from Claude"))
}

/// Blocking Claude API call (kept for compatibility).
pub fn call_claude(api_key: &str, model: &str, messages: &[Message]) -> Result<String> {
    let request = build_claude_request(model, messages);
    let client = reqwest::blocking::Client::new();
    let resp = client
        .post(API_URL)
        .header("x-api-key", api_key)
        .header("anthropic-version", API_VERSION)
        .header("content-type", "application/json")
        .json(&request)
        .send()
        .context("Failed to call Claude API")?;

    let status = resp.status();
    let body = resp.text().context("Failed to read Claude API response")?;
    if !status.is_success() {
        anyhow::bail!("Claude API error ({status}): {body}");
    }
    parse_claude_response(&body)
}

/// Reusable async Claude client for a chat session.
pub struct ClaudeStreamer {
    client: reqwest::Client,
}

impl ClaudeStreamer {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }

    pub async fn stream(
        &self,
        api_key: &str,
        model: &str,
        messages: &[Message],
        lang: Option<&str>,
        tx: mpsc::Sender<StreamEvent>,
        cancelled: Arc<AtomicBool>,
    ) -> Result<()> {
        let request = StreamRequest {
            model: model.to_string(),
            max_tokens: MAX_TOKENS,
            system: system_prompt(lang).to_string(),
            messages: messages.to_vec(),
            stream: true,
        };
        let request = self
            .client
            .post(API_URL)
            .header("x-api-key", api_key)
            .header("anthropic-version", API_VERSION)
            .header("content-type", "application/json")
            .json(&request)
            .send();
        tokio::pin!(request);
        let resp = tokio::select! {
            response = &mut request => response.context("Failed to call Claude API")?,
            _ = wait_for_cancellation(cancelled.clone()) => return Ok(()),
        };
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Claude API error: {body}");
        }

        let mut stream = resp.bytes_stream();
        let mut decoder = SseDecoder::default();
        loop {
            let chunk = tokio::select! {
                chunk = stream.next() => chunk,
                _ = wait_for_cancellation(cancelled.clone()) => return Ok(()),
            };
            let Some(chunk) = chunk else {
                break;
            };
            for line in decoder.push(&chunk.context("stream read error")?)? {
                if dispatch_sse_line(&line, &tx) {
                    return Ok(());
                }
            }
        }
        let _ = tx.send(StreamEvent::Done);
        Ok(())
    }
}

impl Default for ClaudeStreamer {
    fn default() -> Self {
        Self::new()
    }
}

async fn wait_for_cancellation(cancelled: Arc<AtomicBool>) {
    while !cancelled.load(Ordering::Acquire) {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

/// Stream Claude API response via SSE, sending text deltas through the channel.
pub fn stream_claude(
    api_key: &str,
    model: &str,
    messages: &[Message],
    tx: mpsc::Sender<StreamEvent>,
) -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to create tokio runtime")?;
    let streamer = ClaudeStreamer::new();
    rt.block_on(streamer.stream(
        api_key,
        model,
        messages,
        None,
        tx,
        Arc::new(AtomicBool::new(false)),
    ))
}

fn dispatch_sse_line(line: &str, tx: &mpsc::Sender<StreamEvent>) -> bool {
    let Some(data) = line.strip_prefix("data: ") else {
        return false;
    };
    if data == "[DONE]" {
        let _ = tx.send(StreamEvent::Done);
        return true;
    }
    let Ok(event) = serde_json::from_str::<SseData>(data) else {
        return false;
    };
    match event.type_field.as_str() {
        "content_block_delta" => {
            if let Some(delta) = event.delta
                && delta.type_field.as_deref() == Some("text_delta")
                && let Some(text) = delta.text
            {
                let _ = tx.send(StreamEvent::TextDelta(text));
            }
            false
        }
        "message_stop" => {
            let _ = tx.send(StreamEvent::Done);
            true
        }
        _ => false,
    }
}

#[derive(Default)]
struct SseDecoder {
    pending: Vec<u8>,
}

impl SseDecoder {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>> {
        self.pending.extend_from_slice(bytes);
        let mut lines = Vec::new();
        while let Some(pos) = self.pending.iter().position(|byte| *byte == b'\n') {
            let mut line = self.pending.drain(..=pos).collect::<Vec<_>>();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            lines.push(String::from_utf8(line).context("invalid UTF-8 in SSE response")?);
        }
        if self.pending.len() > MAX_SSE_LINE_BYTES {
            anyhow::bail!("SSE response line exceeds {MAX_SSE_LINE_BYTES} bytes");
        }
        Ok(lines)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_decoder_handles_fragmented_utf8_and_crlf() {
        let mut decoder = SseDecoder::default();
        assert!(decoder.push(b"data: bonjour ").unwrap().is_empty());
        let mut bytes = "\u{00e9}\r\n".as_bytes().to_vec();
        let last = bytes.pop().unwrap();
        assert!(decoder.push(&bytes).unwrap().is_empty());
        assert_eq!(decoder.push(&[last]).unwrap(), vec!["data: bonjour é"]);
    }

    #[test]
    fn sse_decoder_returns_multiple_lines() {
        let mut decoder = SseDecoder::default();
        assert_eq!(
            decoder.push(b"data: one\ndata: two\n").unwrap(),
            ["data: one", "data: two"]
        );
    }

    #[test]
    fn sse_decoder_rejects_oversized_incomplete_line() {
        let mut decoder = SseDecoder::default();
        let err = decoder
            .push(&vec![b'x'; MAX_SSE_LINE_BYTES + 1])
            .unwrap_err();
        assert!(err.to_string().contains("exceeds"));
    }

    #[test]
    fn sse_parsing() {
        let data = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Bonjour"}}"#;
        let event: SseData = serde_json::from_str(data).unwrap();
        assert_eq!(event.type_field, "content_block_delta");
        assert_eq!(event.delta.unwrap().text.as_deref(), Some("Bonjour"));
    }
}
