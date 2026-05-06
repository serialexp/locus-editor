//! Multi-turn message log. Every turn appends; tweaks are just additional
//! user messages. The system prompt is injected automatically at the head
//! of the log when the conversation is built — callers should never push
//! their own system message.

use serde::{Deserialize, Serialize};

/// The instructions we send as the first message of every conversation.
/// This is the single most important knob in the feature: if the model
/// wraps its output in markdown fences or adds a chatty preamble, the
/// SVG extractor has to deal with it. The phrasing below has been kept
/// short on purpose — long instructions consistently make models add
/// "Here you go!" preambles.
pub const SYSTEM_PROMPT: &str = "\
You are an SVG generation assistant for a vector editor.\n\
- Reply with a single, complete, well-formed SVG document and nothing else.\n\
- Use a viewBox attribute so the result scales cleanly.\n\
- No markdown fences, no prose, no <?xml prolog unless strictly required.\n\
- When the user asks for a change, return the FULL updated SVG, not a diff.";

/// Message role. Serialised lowercase to match the OpenAI wire format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

/// A single turn in the conversation. Mirrors the wire shape directly so
/// the request struct in `chat.rs` can serialise `&[Message]` without an
/// intermediate copy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
        }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
        }
    }
}

/// Ordered message log. Construct via [`Conversation::new`] (the system
/// message is injected for you), then push user / assistant turns as the
/// dialog runs.
#[derive(Debug, Clone, Default)]
pub struct Conversation {
    pub messages: Vec<Message>,
}

impl Conversation {
    /// Build a fresh conversation with the SVG-assistant system prompt
    /// already in place. Use [`Conversation::with_system`] if you want a
    /// custom prompt (e.g. for testing).
    pub fn new() -> Self {
        Self::with_system(SYSTEM_PROMPT)
    }

    /// Build a fresh conversation with the given system prompt.
    pub fn with_system(prompt: impl Into<String>) -> Self {
        Self {
            messages: vec![Message::system(prompt)],
        }
    }

    pub fn push_user(&mut self, content: impl Into<String>) {
        self.messages.push(Message::user(content));
    }

    pub fn push_assistant(&mut self, content: impl Into<String>) {
        self.messages.push(Message::assistant(content));
    }

    /// True if at least one user turn has been recorded — convenient for
    /// "is there anything to send?" checks.
    pub fn has_user_turn(&self) -> bool {
        self.messages.iter().any(|m| m.role == Role::User)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_injects_system_prompt() {
        let conv = Conversation::new();
        assert_eq!(conv.messages.len(), 1);
        assert_eq!(conv.messages[0].role, Role::System);
        assert_eq!(conv.messages[0].content, SYSTEM_PROMPT);
    }

    #[test]
    fn with_system_uses_custom_prompt() {
        let conv = Conversation::with_system("be brief");
        assert_eq!(conv.messages[0].content, "be brief");
    }

    #[test]
    fn push_helpers_track_roles() {
        let mut conv = Conversation::new();
        conv.push_user("hi");
        conv.push_assistant("hello");
        assert_eq!(conv.messages.len(), 3);
        assert_eq!(conv.messages[1].role, Role::User);
        assert_eq!(conv.messages[2].role, Role::Assistant);
    }

    #[test]
    fn has_user_turn_only_after_user_push() {
        let mut conv = Conversation::new();
        assert!(!conv.has_user_turn());
        conv.push_user("hi");
        assert!(conv.has_user_turn());
    }
}
