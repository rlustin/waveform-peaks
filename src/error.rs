use std::{fmt, io};

/// Errors returned while decoding audio or writing waveform data.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// Reading the input or writing the output failed.
    Io(io::Error),
    /// An option is out of range, such as a zoom below 2.
    InvalidOptions(&'static str),
    /// The input is malformed. The message describes the first problem found.
    InvalidInput(String),
    /// The input is well formed but uses a feature this crate does not handle.
    Unsupported(String),
    /// The input exceeds a bound set in [`Limits`](crate::Limits) or a
    /// decoder option. The message names the bound.
    LimitExceeded(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(err) => write!(f, "I/O error: {err}"),
            Error::InvalidOptions(message) => write!(f, "invalid options: {message}"),
            Error::InvalidInput(message) => write!(f, "invalid input: {message}"),
            Error::Unsupported(message) => write!(f, "unsupported input: {message}"),
            Error::LimitExceeded(limit) => write!(f, "limit exceeded: {limit}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Error::Io(err)
    }
}
