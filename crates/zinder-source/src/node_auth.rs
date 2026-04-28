//! Node authentication configuration.

use std::{fmt, path::PathBuf};

use secrecy::SecretString;

/// Authentication mode for an upstream node source.
#[derive(Clone, Default)]
pub enum NodeAuth {
    /// No node authentication.
    #[default]
    None,
    /// Cookie-file authentication.
    Cookie {
        /// Path to the node cookie file.
        path: PathBuf,
    },
    /// HTTP Basic authentication.
    Basic {
        /// RPC username.
        username: String,
        /// RPC password.
        password: SecretString,
    },
}

impl fmt::Debug for NodeAuth {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => formatter.debug_tuple("None").finish(),
            Self::Cookie { .. } => formatter
                .debug_struct("Cookie")
                .field("path", &"[REDACTED]")
                .finish(),
            Self::Basic { .. } => formatter
                .debug_struct("Basic")
                .field("username", &"[REDACTED]")
                .field("password", &"[REDACTED]")
                .finish(),
        }
    }
}

impl NodeAuth {
    /// Creates HTTP Basic authentication.
    #[must_use]
    pub fn basic(username: impl Into<String>, password: impl Into<SecretString>) -> Self {
        Self::Basic {
            username: username.into(),
            password: password.into(),
        }
    }

    /// Stable diagnostic name for this authentication scheme.
    #[must_use]
    pub const fn scheme_name(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Cookie { .. } => "cookie",
            Self::Basic { .. } => "basic",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::NodeAuth;

    #[test]
    fn debug_redacts_basic_auth_username_and_password() {
        let debug_output = format!("{:?}", NodeAuth::basic("zebra", "secret"));

        assert!(debug_output.contains("[REDACTED]"));
        assert!(!debug_output.contains("zebra"));
        assert!(!debug_output.contains("secret"));
    }
}
