//! OpenAI-compatible chat completion client + SVG extraction + cross-platform
//! API-key storage.
//!
//! The crate is intentionally small and pure-logic: it speaks JSON over HTTP
//! and returns text. Translating that text into a `Scene` is the caller's job
//! — we don't depend on `vector-scene` or `vector-svg` so the LLM layer can
//! be reused or tested independently.
//!
//! ## Layout
//!
//! - [`config`] — endpoint + model + key, with sensible DeepSeek defaults.
//! - [`conversation`] — message log carried across turns; the system prompt
//!   that nudges the model toward "return only an SVG" lives here.
//! - [`chat`] — the actual blocking POST to `/chat/completions`. Spawn this
//!   on a worker thread; never call it from the UI thread.
//! - [`svg_extract`] — best-effort string-level extractor that tolerates the
//!   ways models like to wrap output (markdown fences, leading prose).
//! - [`secret_store`] — thin wrapper over the `keyring` crate, mapping its
//!   error variants to something the caller can render usefully.
//! - [`error`] — `LlmError`, the error type for [`chat::complete`].

pub mod chat;
pub mod config;
pub mod conversation;
pub mod error;
pub mod secret_store;
pub mod svg_extract;

pub use chat::complete;
pub use config::{DEFAULT_BASE_URL, DEFAULT_MODEL, LlmConfig};
pub use conversation::{Conversation, Message, Role, SYSTEM_PROMPT};
pub use error::LlmError;
pub use secret_store::{KeyStoreError, delete_api_key, load_api_key, save_api_key};
pub use svg_extract::extract_svg;
