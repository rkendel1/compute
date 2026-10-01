//! A value that must never be printed, logged, serialized, or persisted.

use std::fmt;

/// What a scrubbed secret is replaced with.
pub const REDACTED: &str = "[REDACTED]";

/// A credential held in memory only. It has no `Display` and no `Serialize`,
/// and its `Debug` is redacted, so a stray `{:?}` or a derived serializer
/// cannot leak it.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The only way to read the value. Call it where the secret is handed to
    /// its consumer, never to format it.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Secret(<redacted>)")
    }
}

/// `text` with every occurrence of every secret replaced.
pub fn scrub(text: &str, secrets: &[&Secret]) -> String {
    let mut scrubbed = text.to_owned();
    for secret in secrets.iter().filter(|secret| !secret.is_empty()) {
        scrubbed = scrubbed.replace(secret.expose(), REDACTED);
    }
    scrubbed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_shows_the_value() {
        let secret = Secret::new("ghp_supersecret");
        assert!(!format!("{secret:?}").contains("supersecret"));
        assert!(!format!("{:?}", Some(&secret)).contains("supersecret"));
    }

    #[test]
    fn scrub_replaces_every_occurrence_and_ignores_empty_secrets() {
        let secret = Secret::new("tok");
        let empty = Secret::new("");
        assert_eq!(
            scrub("tok and tok", &[&secret, &empty]),
            "[REDACTED] and [REDACTED]"
        );
    }
}
