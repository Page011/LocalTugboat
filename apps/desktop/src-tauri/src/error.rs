//! The one error type every Tauri command returns: a stable `kind` the UI can branch on and a
//! one-line `message` it can show. Errors are never swallowed: they reach the UI or the log.

use serde::{Deserialize, Serialize};
use tugboat_core::CoreError;
use ts_rs::TS;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct CmdError {
    pub kind: String,
    pub message: String,
}

impl CmdError {
    pub fn new(kind: &str, message: impl Into<String>) -> CmdError {
        CmdError { kind: kind.to_string(), message: message.into() }
    }

    /// The command came from a window that may not use it.
    pub fn forbidden(label: &str) -> CmdError {
        CmdError::new("forbidden", format!("NOT ALLOWED from window '{label}'"))
    }
}

impl std::fmt::Display for CmdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

impl std::error::Error for CmdError {}

impl From<CoreError> for CmdError {
    fn from(e: CoreError) -> CmdError {
        CmdError::new(e.kind(), e.to_string())
    }
}

impl From<std::io::Error> for CmdError {
    fn from(e: std::io::Error) -> CmdError {
        CmdError::new("io", e.to_string())
    }
}

pub type CmdResult<T> = Result<T, CmdError>;
