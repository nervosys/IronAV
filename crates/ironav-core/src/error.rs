#[cfg(feature = "serde")]
use serde::Serialize;
use std::fmt;

/// Stable, machine-readable error classes. Agents branch on these, not on message text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "kebab-case"))]
pub enum ErrorCode {
    Io,
    InvalidInput,
    InvalidArgument,
    UnsupportedFormat,
    UnsupportedCodec,
    UnsupportedFilter,
    OutputExists,
    HardwareUnavailable,
    NotFound,
    /// The caller asked for the work to stop.
    Cancelled,
    /// A limit on work running at once was reached; retry after something finishes.
    Busy,
    /// The operator's settings forbid this: a path outside IRONAV_ROOTS, read-only mode, a device or
    /// a browser's cookies that were not enabled.
    PermissionDenied,
    Internal,
}

impl ErrorCode {
    pub const ALL: [ErrorCode; 13] = [
        Self::Io,
        Self::InvalidInput,
        Self::InvalidArgument,
        Self::UnsupportedFormat,
        Self::UnsupportedCodec,
        Self::UnsupportedFilter,
        Self::OutputExists,
        Self::HardwareUnavailable,
        Self::NotFound,
        Self::Cancelled,
        Self::Busy,
        Self::PermissionDenied,
        Self::Internal,
    ];

    /// Process exit code used by the CLI.
    pub fn exit_code(self) -> i32 {
        match self {
            ErrorCode::InvalidArgument => 2,
            ErrorCode::NotFound => 3,
            ErrorCode::Io => 4,
            ErrorCode::InvalidInput => 5,
            ErrorCode::UnsupportedFormat | ErrorCode::UnsupportedCodec | ErrorCode::UnsupportedFilter => 6,
            ErrorCode::OutputExists => 7,
            ErrorCode::HardwareUnavailable => 8,
            ErrorCode::Cancelled => 130,
            ErrorCode::Busy => 75,
            ErrorCode::PermissionDenied => 77,
            ErrorCode::Internal => 70,
        }
    }
}

/// An error that tells the caller how to recover: `hint` is a sentence of advice and
/// `alternatives` lists concrete valid values the caller could retry with.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct Error {
    pub code: ErrorCode,
    pub message: String,
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub hint: Option<String>,
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Vec::is_empty"))]
    pub alternatives: Vec<String>,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), hint: None, alternatives: Vec::new() }
    }
    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
    pub fn alternatives<I, S>(mut self, alts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.alternatives = alts.into_iter().map(Into::into).collect();
        self
    }
    pub fn invalid_input(msg: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidInput, msg)
    }
    pub fn invalid_arg(msg: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidArgument, msg)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)?;
        if let Some(h) = &self.hint {
            write!(f, " (hint: {h})")?;
        }
        Ok(())
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        let code = if e.kind() == std::io::ErrorKind::NotFound { ErrorCode::NotFound } else { ErrorCode::Io };
        Error::new(code, e.to_string())
    }
}

#[cfg(feature = "serde")]
impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::invalid_arg(format!("invalid JSON: {e}"))
    }
}
