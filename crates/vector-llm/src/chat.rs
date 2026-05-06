//! Blocking POST to `{base_url}/chat/completions`.
//!
//! This is the only place we touch the network. The function is sync —
//! callers should run it from a worker thread (the editor's dialog does
//! exactly that, mirroring the trace dialog). Holding the UI thread on a
//! multi-second HTTP call would freeze the canvas.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::LlmConfig;
use crate::conversation::{Conversation, Message};
use crate::error::LlmError;

/// Request body. `messages` borrows from the caller's `Conversation` —
/// no clone needed for serialisation since serde_json drives off the
/// `Serialize` impl on `&Message`.
#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [Message],
}

/// Minimal subset of the OpenAI chat-completions response we actually
/// read. Extra fields (usage, system_fingerprint, etc.) are ignored.
#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: ResponseMessage,
}

#[derive(Debug, Deserialize)]
struct ResponseMessage {
    /// We don't actually need the role on the response, but accepting it
    /// keeps the deserialiser happy and serves as a documentation hint.
    #[allow(dead_code)]
    role: String,
    content: String,
}

/// Send the conversation and return the assistant's reply text.
///
/// The function applies a 90-second total timeout — long enough for a
/// chunky SVG generation but short enough that a stalled connection
/// doesn't hold the worker thread open indefinitely.
pub fn complete(cfg: &LlmConfig, conv: &Conversation) -> Result<String, LlmError> {
    // Strip a single trailing slash so callers can paste either form
    // ("https://api.deepseek.com/v1" or ".../v1/") without 404s.
    let base = cfg.base_url.trim_end_matches('/');
    let url = format!("{base}/chat/completions");

    let body = ChatRequest {
        model: &cfg.model,
        messages: &conv.messages,
    };

    // A fresh client per call is fine — request rate here is
    // human-driven (one per chat turn), so we don't need to amortise
    // connection setup across calls.
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(90))
        .build()
        .map_err(|e| LlmError::Network(e.to_string()))?;

    let resp = client
        .post(&url)
        .bearer_auth(&cfg.api_key)
        .json(&body)
        .send()
        .map_err(|e| LlmError::Network(e.to_string()))?;

    let status = resp.status();
    if !status.is_success() {
        // Capture the body for diagnostics — provider error envelopes
        // here are usually short and the user can paste them at us.
        let body = resp
            .text()
            .unwrap_or_else(|e| format!("(could not read body: {e})"));
        return Err(LlmError::Http {
            status: status.as_u16(),
            body,
        });
    }

    let parsed: ChatResponse = resp.json().map_err(|e| LlmError::Decode(e.to_string()))?;

    parsed
        .choices
        .into_iter()
        .next()
        .map(|c| c.message.content)
        .ok_or(LlmError::NoChoices)
}
