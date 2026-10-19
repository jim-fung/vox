//! MCP (Model Context Protocol) server — JSON-RPC 2.0 over stdio.
//!
//! Exposes 14 tools for AI assistants: speak, hear, voice cloning, config, stats, packs.
//! Launched via `vox serve` and auto-configured by `vox init` for 14 AI tools.
//!
//! The tool implementations live in `tools.rs` (R4 split); this module owns the
//! protocol framing, JSON-RPC dispatch, and the language-aware server instructions.

use std::io::{self, BufRead, Write};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::tools::{call_tool, tool_definitions};
use crate::db;

const SERVER_NAME: &str = "vox";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
const PROTOCOL_VERSION: &str = "2024-11-05";

const VOX_INSTRUCTIONS_TEMPLATE: &str = "\
You have access to vox, a text-to-speech tool. Use it to give spoken feedback to the user.\n\
\n\
WHEN TO SPEAK (vox_speak):\n\
- After completing a significant task (feature, bug fix, refactor): summarize what was done\n\
- When the user asks you to explain something verbally\n\
- For important warnings or status updates the user should hear\n\
\n\
WHEN NOT TO SPEAK:\n\
- Trivial operations (formatting, single-line fixes, file reads)\n\
- When the user is clearly reading the output already\n\
- Rapid back-and-forth conversation\n\
\n\
GUIDELINES:\n\
- Keep summaries under 2 sentences\n\
{language_guideline}\n\
- Use vox_config_show to check the user's preferred voice/backend before first use\n\
- For longer explanations, use vox_speak with a concise summary, not the full text\n\
\n\
VOICE CONVERSATION (vox_hear + vox_speak):\n\
You can have a voice conversation with the user — you ARE the brain, no API key needed.\n\
Loop: vox_hear (listen) → you think → vox_speak (respond). Repeat until the user says goodbye.\n\
When the user asks to \"chat\" or \"talk\" or \"parler\", start this loop.";

fn configured_lang() -> Option<String> {
    db::open()
        .ok()
        .and_then(|conn| db::get_preferences(&conn).ok())
        .and_then(|prefs| prefs.lang)
        .filter(|lang| !lang.is_empty())
}

fn system_lang() -> Option<String> {
    crate::config::system_lang()
}

fn default_lang() -> Option<String> {
    configured_lang().or_else(system_lang)
}

fn vox_instructions(lang: Option<&str>) -> String {
    let language_guideline = match lang {
        Some(lang) => format!(
            "- Speak in the user's language ({lang}); honour any language they explicitly ask for"
        ),
        None => "- Match the language the user is writing in".to_string(),
    };
    VOX_INSTRUCTIONS_TEMPLATE.replace("{language_guideline}", &language_guideline)
}

#[derive(Deserialize)]
struct JsonRpcMessage {
    id: Option<Value>,
    method: Option<String>,
    params: Option<Value>,
}

#[derive(Serialize)]
struct JsonRpcResponse {
    jsonrpc: String,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<Value>,
}

impl JsonRpcResponse {
    fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: Some(result),
            error: None,
        }
    }

    fn err(id: Value, code: i64, message: String) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(json!({"code": code, "message": message})),
        }
    }
}

/// Run the vox MCP server on stdio.
pub fn run_server() -> Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let msg: JsonRpcMessage = match serde_json::from_str(line) {
            Ok(m) => m,
            Err(e) => {
                let resp = JsonRpcResponse::err(Value::Null, -32700, format!("parse error: {e}"));
                write_response(&mut stdout, &resp)?;
                continue;
            }
        };

        let method = msg.method.as_deref().unwrap_or("");

        // Notifications have no id
        let id = match msg.id {
            Some(id) => id,
            None => continue,
        };

        let response = match method {
            "initialize" => handle_initialize(id),
            "ping" => JsonRpcResponse::ok(id, json!({})),
            "tools/list" => handle_tools_list(id),
            "tools/call" => handle_tools_call(id, &msg.params),
            _ => JsonRpcResponse::err(id, -32601, format!("method not found: {method}")),
        };

        write_response(&mut stdout, &response)?;
    }

    Ok(())
}

fn write_response(stdout: &mut io::Stdout, resp: &JsonRpcResponse) -> Result<()> {
    let json = serde_json::to_string(resp)?;
    writeln!(stdout, "{json}")?;
    stdout.flush()?;
    Ok(())
}

fn handle_initialize(id: Value) -> JsonRpcResponse {
    JsonRpcResponse::ok(
        id,
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": {
                "name": SERVER_NAME,
                "version": SERVER_VERSION
            },
            "instructions": vox_instructions(default_lang().as_deref())
        }),
    )
}

fn handle_tools_list(id: Value) -> JsonRpcResponse {
    JsonRpcResponse::ok(id, json!({ "tools": tool_definitions() }))
}

fn handle_tools_call(id: Value, params: &Option<Value>) -> JsonRpcResponse {
    let params = match params {
        Some(p) => p,
        None => return JsonRpcResponse::err(id, -32602, "missing params".into()),
    };

    let tool_name = match params.get("name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return JsonRpcResponse::err(id, -32602, "missing tool name".into()),
    };

    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    let result = call_tool(tool_name, &args);

    JsonRpcResponse::ok(id, json!(result))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instructions_default_to_user_language_when_unset() {
        let instructions = vox_instructions(None);
        assert!(instructions.contains("Match the language the user is writing in"));
        assert!(!instructions.contains("Use French by default"));
    }

    #[test]
    fn instructions_follow_configured_language() {
        assert!(vox_instructions(Some("fr")).contains("Speak in the user's language (fr)"));
        assert!(vox_instructions(Some("es")).contains("Speak in the user's language (es)"));
        assert!(vox_instructions(Some("ja")).contains("Speak in the user's language (ja)"));
    }

    #[test]
    fn normalize_locale_maps_primary_subtag_when_supported() {
        assert_eq!(
            crate::config::normalize_locale("fr-FR"),
            Some("fr".to_string())
        );
        assert_eq!(
            crate::config::normalize_locale("en_US"),
            Some("en".to_string())
        );
        assert_eq!(
            crate::config::normalize_locale("de-AT"),
            Some("de".to_string())
        );
        assert_eq!(
            crate::config::normalize_locale("FR"),
            Some("fr".to_string())
        );
        assert_eq!(
            crate::config::normalize_locale("zh-Hant"),
            Some("zh".to_string())
        );
    }

    #[test]
    fn normalize_locale_rejects_unsupported_or_empty() {
        assert_eq!(crate::config::normalize_locale("cy-GB"), None);
        assert_eq!(crate::config::normalize_locale("C"), None);
        assert_eq!(crate::config::normalize_locale(""), None);
    }

    #[test]
    fn system_lang_is_supported_or_none() {
        assert!(
            system_lang()
                .is_none_or(|lang| crate::config::SUPPORTED_LANGS.contains(&lang.as_str()))
        );
    }
}
