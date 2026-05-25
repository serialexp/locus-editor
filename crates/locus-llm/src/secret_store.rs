//! Cross-platform API key storage backed by the OS keychain.
//!
//! On macOS this writes to the user keychain, on Windows to the Credential
//! Manager, and on Linux to the Secret Service (typically GNOME Keyring or
//! KWallet). The platform backends are wired through the `keyring` crate's
//! per-OS feature flags; see this crate's `Cargo.toml`.
//!
//! The wrapper exists to:
//! 1. Lock in a single (`SERVICE`, `ACCOUNT`) pair so the editor doesn't
//!    accidentally write under different identifiers from different call
//!    sites.
//! 2. Map `keyring::Error` into a `KeyStoreError` that distinguishes the
//!    cases the dialog actually treats differently — backend missing
//!    (offer in-memory fallback) vs. nothing stored yet (silent) vs.
//!    OS denied / corrupted (show the error).
//!
//! All three operations are blocking but cheap (typically <10 ms). Calling
//! them from the UI thread is fine.

use thiserror::Error;

/// Service identifier registered with the OS keychain. Must stay stable
/// across releases or stored keys become unfindable.
const SERVICE: &str = "locus-editor";
/// Account / username slot. We only ever store one key (the LLM API key)
/// so the account name is fixed; if we ever support multiple providers,
/// hash the `(provider, base_url)` pair into the account string.
const ACCOUNT: &str = "llm-api-key";

#[derive(Debug, Error)]
pub enum KeyStoreError {
    /// No platform credential store was available — typically a headless
    /// Linux session with no D-Bus, or a sandboxed environment that has
    /// stripped the keychain entitlement. The dialog uses this signal to
    /// offer the in-memory paste-key fallback.
    #[error("no platform credential store available: {0}")]
    NoBackend(String),

    /// No entry exists yet for this service/account. Returned by
    /// [`load_api_key`] on a fresh install — callers should treat this as
    /// "show the empty input box", not "show an error".
    #[error("no api key stored")]
    NotFound,

    /// The store rejected the operation — locked keychain, permission
    /// prompt declined, etc. The inner string is the platform-level
    /// message rendered for the user.
    #[error("keychain error: {0}")]
    Other(String),
}

impl From<keyring::Error> for KeyStoreError {
    fn from(e: keyring::Error) -> Self {
        match e {
            keyring::Error::NoEntry => KeyStoreError::NotFound,
            keyring::Error::PlatformFailure(ref err) => {
                // Heuristic: a missing D-Bus session or unimplemented
                // backend usually surfaces here. The dialog can offer
                // the paste-key fallback either way, so coalesce both
                // into NoBackend.
                KeyStoreError::NoBackend(err.to_string())
            }
            other => KeyStoreError::Other(other.to_string()),
        }
    }
}

fn entry() -> Result<keyring::Entry, KeyStoreError> {
    keyring::Entry::new(SERVICE, ACCOUNT).map_err(KeyStoreError::from)
}

/// Read the stored API key. Returns `Err(NotFound)` on a fresh install —
/// the dialog should silently surface its key-input row in that case
/// rather than showing the message as an error.
pub fn load_api_key() -> Result<String, KeyStoreError> {
    entry()?.get_password().map_err(KeyStoreError::from)
}

/// Persist `key` to the OS keychain, overwriting any prior value.
pub fn save_api_key(key: &str) -> Result<(), KeyStoreError> {
    entry()?.set_password(key).map_err(KeyStoreError::from)
}

/// Forget the stored API key. A subsequent [`load_api_key`] returns
/// `Err(NotFound)`. Returns `Ok(())` if no entry existed in the first
/// place — idempotent for the dialog's "Forget" button.
pub fn delete_api_key() -> Result<(), KeyStoreError> {
    match entry()?.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.into()),
    }
}
