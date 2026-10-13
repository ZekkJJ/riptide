// SPDX-FileCopyrightText: Riptide contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Engine error type.

use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("network error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("io error: {0}")]
    PlainIo(#[from] std::io::Error),

    #[error("invalid url: {0}")]
    Url(#[from] url::ParseError),

    #[error("state file is not usable: {0}")]
    CorruptState(String),

    #[error("server replied {status} for {url}")]
    BadStatus { status: u16, url: String },

    /// The server answered with a web page where a file was expected, which is what a signed
    /// or time-limited link looks like once it has expired. The download needs a fresh URL,
    /// not a retry.
    #[error(
        "the server returned a web page instead of the file; the link has most likely expired"
    )]
    LinkExpired,

    /// The file on the server is not the one the saved state describes, so the partial data on
    /// disk cannot be reused.
    #[error("the file on the server changed ({reason}); the partial download cannot be resumed")]
    ContentChanged { reason: String },

    #[error("the server does not support resuming this download")]
    ResumeUnsupported,

    #[error("could not parse {kind} manifest: {detail}")]
    Manifest { kind: &'static str, detail: String },

    #[error("download was cancelled")]
    Cancelled,

    #[error("{0}")]
    Other(String),
}

impl Error {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io { path: path.into(), source }
    }

    pub fn other(msg: impl Into<String>) -> Self {
        Self::Other(msg.into())
    }

    /// Whether retrying the same request unchanged could plausibly succeed.
    ///
    /// Connection faults and 5xx/429 replies are worth another attempt; a 404, an expired
    /// link, or a changed file are not, and the caller should surface them instead of looping.
    pub fn is_retryable(&self) -> bool {
        match self {
            Error::Http(e) => {
                e.is_timeout()
                    || e.is_connect()
                    || e.is_request()
                    || e.status().is_some_and(|s| s.is_server_error() || s.as_u16() == 429)
            }
            Error::PlainIo(e) | Error::Io { source: e, .. } => matches!(
                e.kind(),
                std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::Interrupted
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::UnexpectedEof
            ),
            Error::BadStatus { status, .. } => *status >= 500 || *status == 429 || *status == 408,
            Error::LinkExpired
            | Error::ContentChanged { .. }
            | Error::ResumeUnsupported
            | Error::Cancelled
            | Error::CorruptState(_)
            | Error::Manifest { .. }
            | Error::Url(_)
            | Error::Other(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_server_faults_are_retryable() {
        assert!(Error::BadStatus { status: 503, url: "u".into() }.is_retryable());
        assert!(Error::BadStatus { status: 429, url: "u".into() }.is_retryable());
    }

    #[test]
    fn permanent_conditions_are_not_retried() {
        assert!(!Error::BadStatus { status: 404, url: "u".into() }.is_retryable());
        assert!(!Error::LinkExpired.is_retryable());
        assert!(!Error::ContentChanged { reason: "etag".into() }.is_retryable());
        assert!(!Error::Cancelled.is_retryable());
    }
}
