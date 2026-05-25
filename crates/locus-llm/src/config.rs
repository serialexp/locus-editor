//! Endpoint + model + key. The defaults point at DeepSeek (cheap, OpenAI-
//! compatible), but every field is just a string — point the dialog's
//! Settings panel at any provider that exposes `/chat/completions`.

/// DeepSeek's OpenAI-compatible base URL. The chat endpoint is appended as
/// `/chat/completions` by [`crate::chat::complete`], so this string should
/// NOT include a trailing slash and should NOT include `/chat/completions`.
pub const DEFAULT_BASE_URL: &str = "https://api.deepseek.com/v1";

/// DeepSeek's general-purpose chat model. Cheap, fast, capable enough for
/// SVG generation in our experience.
pub const DEFAULT_MODEL: &str = "deepseek-chat";

/// Connection + model + auth bundle. Cloned cheaply into worker threads.
#[derive(Debug, Clone)]
pub struct LlmConfig {
    /// Base URL of an OpenAI-compatible API. The chat completions path is
    /// appended automatically; trailing slashes are tolerated.
    pub base_url: String,
    /// Model identifier, passed through verbatim in the `model` field of
    /// the request body.
    pub model: String,
    /// Bearer token. Stored as plain text in memory; the editor never
    /// writes it to disk except via the OS keychain (see
    /// [`crate::secret_store`]).
    pub api_key: String,
}

impl LlmConfig {
    /// Build a config seeded with DeepSeek defaults and the given key.
    pub fn deepseek(api_key: impl Into<String>) -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            model: DEFAULT_MODEL.to_string(),
            api_key: api_key.into(),
        }
    }
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self::deepseek(String::new())
    }
}
