//! Error type for [`crate::chat::complete`]. Kept small on purpose — the
//! UI layer renders these straight into a status row, so the variants are
//! tuned for human readability rather than programmatic recovery.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum LlmError {
    /// The HTTP request failed before the server returned a status code
    /// (DNS, TLS, connection refused, timeout, etc.). The inner string is
    /// the underlying reqwest error rendered.
    #[error("network error: {0}")]
    Network(String),

    /// The server returned a non-2xx status. We capture the status and the
    /// raw response body so the caller can surface provider-specific
    /// error messages (e.g. DeepSeek's `{"error":{"message":...}}`).
    #[error("HTTP {status}: {body}")]
    Http { status: u16, body: String },

    /// The response was 2xx but the body didn't decode into our expected
    /// `ChatResponse` shape. Usually a sign the endpoint isn't actually
    /// OpenAI-compatible.
    #[error("could not decode response: {0}")]
    Decode(String),

    /// Decoded fine, but `choices` was empty. Some providers return this
    /// when content was filtered or truncated to zero.
    #[error("response had no choices")]
    NoChoices,
}
