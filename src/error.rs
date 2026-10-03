use std::fmt;

/// Error returned when a document cannot be read.
#[derive(Debug)]
pub enum Error {
    /// The file could not be read from disk.
    Io(std::io::Error),
    /// The data is not a valid or supported PSD/PSB document.
    Format(String),
}

/// Result alias used throughout the crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Format(m) => write!(f, "invalid PSD: {m}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            Error::Format(_) => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

macro_rules! bail {
    ($($arg:tt)*) => {
        return Err($crate::error::Error::Format(format!($($arg)*)))
    };
}
pub(crate) use bail;

pub(crate) trait OptionExt<T> {
    fn or_format(self, msg: &str) -> Result<T>;
}

impl<T> OptionExt<T> for Option<T> {
    fn or_format(self, msg: &str) -> Result<T> {
        self.ok_or_else(|| Error::Format(msg.to_string()))
    }
}
