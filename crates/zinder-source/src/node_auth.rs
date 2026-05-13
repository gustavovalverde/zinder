//! Node authentication configuration.

use std::{fmt, fs, path::PathBuf};

use secrecy::{ExposeSecret, SecretString};
use thiserror::Error;

/// Authentication mode for an upstream node source.
#[derive(Clone, Default)]
pub enum NodeAuth {
    /// No node authentication.
    #[default]
    None,
    /// Cookie-based authentication. The credentials may live in a file on
    /// disk or be provided inline as a configuration value (typically through
    /// a `PaaS` environment variable).
    Cookie(CookieSource),
    /// HTTP Basic authentication.
    Basic {
        /// RPC username.
        username: String,
        /// RPC password.
        password: SecretString,
    },
}

/// Where cookie credentials come from.
///
/// `File` is the canonical Zebra/zcashd shape: the node writes a rotating
/// cookie file and Zinder reads it on connection setup. `Inline` is for
/// `PaaS`-style deployments where the secret is injected as a configuration
/// value (typically through `ZINDER_NODE__AUTH__COOKIE`) and is held in
/// memory only.
#[derive(Clone)]
pub enum CookieSource {
    /// Path to a node-rotated cookie file.
    File(PathBuf),
    /// Cookie credentials supplied inline.
    Inline(SecretString),
}

/// Error returned while reading cookie credentials.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CookieSourceError {
    /// The cookie file could not be opened or read.
    #[error("cookie file at {path} could not be read")]
    Unreadable {
        /// Path that failed to open.
        path: PathBuf,
    },
    /// The cookie credentials are empty after trimming whitespace.
    #[error("cookie credentials are empty")]
    Empty,
}

impl CookieSource {
    /// Reads the trimmed cookie credentials.
    ///
    /// Returns [`CookieSourceError::Unreadable`] when the file is missing or
    /// not readable and [`CookieSourceError::Empty`] when the credentials are
    /// blank after trimming.
    pub fn read_credentials(&self) -> Result<SecretString, CookieSourceError> {
        let raw = match self {
            Self::File(path) => fs::read_to_string(path)
                .map_err(|_| CookieSourceError::Unreadable { path: path.clone() })?,
            Self::Inline(content) => content.expose_secret().to_owned(),
        };
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(CookieSourceError::Empty);
        }
        Ok(SecretString::from(trimmed.to_owned()))
    }
}

impl fmt::Debug for CookieSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File(_) => formatter.debug_tuple("File").field(&"[REDACTED]").finish(),
            Self::Inline(_) => formatter
                .debug_tuple("Inline")
                .field(&"[REDACTED]")
                .finish(),
        }
    }
}

impl fmt::Debug for NodeAuth {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => formatter.debug_tuple("None").finish(),
            Self::Cookie(source) => formatter
                .debug_struct("Cookie")
                .field("source", &source.scheme_label())
                .field(
                    match source {
                        CookieSource::File(_) => "path",
                        CookieSource::Inline(_) => "credentials",
                    },
                    &"[REDACTED]",
                )
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

    /// Creates cookie authentication backed by a file on disk.
    #[must_use]
    pub fn cookie_file(path: impl Into<PathBuf>) -> Self {
        Self::Cookie(CookieSource::File(path.into()))
    }

    /// Creates cookie authentication backed by inline credentials.
    #[must_use]
    pub fn cookie_inline(credentials: impl Into<SecretString>) -> Self {
        Self::Cookie(CookieSource::Inline(credentials.into()))
    }

    /// Stable diagnostic name for this authentication scheme.
    #[must_use]
    pub const fn scheme_name(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Cookie(_) => "cookie",
            Self::Basic { .. } => "basic",
        }
    }
}

impl CookieSource {
    /// Stable diagnostic label naming the credential source ("file" or
    /// "inline"). Safe to include in structured logs.
    #[must_use]
    pub const fn scheme_label(&self) -> &'static str {
        match self {
            Self::File(_) => "file",
            Self::Inline(_) => "inline",
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use secrecy::SecretString;
    use tempfile::NamedTempFile;

    use super::{CookieSource, CookieSourceError, NodeAuth};

    #[test]
    fn debug_redacts_basic_auth_username_and_password() {
        let debug_output = format!("{:?}", NodeAuth::basic("zebra", "secret"));

        assert!(debug_output.contains("[REDACTED]"));
        assert!(!debug_output.contains("zebra"));
        assert!(!debug_output.contains("secret"));
    }

    #[test]
    fn debug_redacts_cookie_file_path() {
        let debug_output = format!("{:?}", NodeAuth::cookie_file("/var/run/auth/.cookie"));

        assert!(debug_output.contains("[REDACTED]"));
        assert!(!debug_output.contains("/var/run/auth/.cookie"));
        assert!(debug_output.contains("file"));
    }

    #[test]
    fn debug_redacts_cookie_inline_content() {
        let debug_output = format!(
            "{:?}",
            NodeAuth::cookie_inline(SecretString::from("user:topsecret"))
        );

        assert!(debug_output.contains("[REDACTED]"));
        assert!(!debug_output.contains("topsecret"));
        assert!(debug_output.contains("inline"));
    }

    #[test]
    fn read_credentials_from_file_trims_whitespace() -> Result<(), eyre::Report> {
        let mut file = NamedTempFile::new()?;
        writeln!(file, "user:cookie-secret")?;

        let source = CookieSource::File(file.path().to_path_buf());
        let credentials = source.read_credentials()?;

        assert_eq!(
            secrecy::ExposeSecret::expose_secret(&credentials),
            "user:cookie-secret"
        );
        Ok(())
    }

    #[test]
    fn read_credentials_from_inline_trims_whitespace() -> Result<(), eyre::Report> {
        let source = CookieSource::Inline(SecretString::from("  user:cookie-secret\n"));
        let credentials = source.read_credentials()?;

        assert_eq!(
            secrecy::ExposeSecret::expose_secret(&credentials),
            "user:cookie-secret"
        );
        Ok(())
    }

    #[test]
    fn read_credentials_rejects_empty_file() -> Result<(), eyre::Report> {
        let file = NamedTempFile::new()?;

        let source = CookieSource::File(file.path().to_path_buf());

        assert!(matches!(
            source.read_credentials(),
            Err(CookieSourceError::Empty)
        ));
        Ok(())
    }

    #[test]
    fn read_credentials_rejects_missing_file() {
        let source = CookieSource::File("/nonexistent/path/.cookie".into());

        assert!(matches!(
            source.read_credentials(),
            Err(CookieSourceError::Unreadable { .. })
        ));
    }
}
