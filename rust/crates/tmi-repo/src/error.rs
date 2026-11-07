//! Errors of the read side, by the Python exception they stand for.
//!
//! The CLI prints `tmiki: error: {message}` for the first three, exactly as
//! `tmi.cli.main` does. `Forward` means "not reproduced exactly here": the
//! command is handed to Python, which then does all of it.

use tmi_store::StoreError;

#[derive(Debug, Clone)]
pub enum RepoError {
    /// A `RuntimeError` with this message.
    Runtime(String),
    /// A `ValueError` (object hash mismatch, wrong object type, ...).
    Value(String),
    /// A `FileNotFoundError` (a missing object).
    NotFound(String),
    /// Behaviour the Rust side does not reproduce; run the command in Python.
    Forward(String),
}

impl RepoError {
    pub fn runtime(msg: impl Into<String>) -> Self {
        RepoError::Runtime(msg.into())
    }

    pub fn forward(why: impl Into<String>) -> Self {
        RepoError::Forward(why.into())
    }

    /// The message Python prints after "tmiki: error: ", if any.
    pub fn message(&self) -> Option<&str> {
        match self {
            RepoError::Runtime(m) | RepoError::Value(m) | RepoError::NotFound(m) => Some(m),
            RepoError::Forward(_) => None,
        }
    }

    pub fn is_runtime(&self) -> bool {
        matches!(self, RepoError::Runtime(_))
    }

    pub fn is_forward(&self) -> bool {
        matches!(self, RepoError::Forward(_))
    }
}

impl std::fmt::Display for RepoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RepoError::Runtime(m) | RepoError::Value(m) | RepoError::NotFound(m) => f.write_str(m),
            RepoError::Forward(why) => write!(f, "(forward to Python: {why})"),
        }
    }
}

impl std::error::Error for RepoError {}

impl From<StoreError> for RepoError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::Missing(_) => RepoError::NotFound(e.to_string()),
            StoreError::HashMismatch { .. } | StoreError::WrongType { .. } | StoreError::BadChunked(_) => {
                RepoError::Value(e.to_string())
            }
            StoreError::Other(msg) => RepoError::Forward(msg),
        }
    }
}

pub type Result<T> = std::result::Result<T, RepoError>;
