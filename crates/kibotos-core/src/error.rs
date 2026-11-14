use std::fmt;
use std::io;

pub type Result<T> = std::result::Result<T, Error>;

/// Errors from opening or extracting an archive.
///
/// `Display` gives English text for logs and the console tool; the UI maps
/// each variant to its own localized message.
#[derive(Debug)]
pub enum Error {
    /// Reading the archive or writing the output failed at the OS level.
    Io(io::Error),
    /// The file is not an archive format Kibotos understands.
    UnknownFormat,
    /// Valid archive, but it uses something not supported (method, feature).
    Unsupported(String),
    /// The archive is damaged or truncated.
    Corrupt(String),
    /// The entry or the archive index is encrypted and no password was given.
    PasswordRequired,
    /// The password given is wrong (or the encrypted data is damaged).
    WrongPassword,
    /// A volume of a multi-part archive is missing.
    MissingVolume(String),
    /// A decompression-bomb guard stopped the extraction.
    LimitExceeded(LimitKind),
    /// The user cancelled the operation.
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimitKind {
    /// Output would exceed [`crate::Limits::max_total_bytes`].
    TotalSize,
    /// Output grew far beyond the size of the archive.
    Ratio,
    /// An entry produced more data than its header declares.
    EntrySize,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::UnknownFormat => f.write_str("not a supported archive format"),
            Error::Unsupported(s) => write!(f, "unsupported: {s}"),
            Error::Corrupt(s) => write!(f, "archive is damaged: {s}"),
            Error::PasswordRequired => f.write_str("a password is required"),
            Error::WrongPassword => f.write_str("wrong password or damaged encrypted data"),
            Error::MissingVolume(s) => write!(f, "missing archive volume: {s}"),
            Error::LimitExceeded(LimitKind::TotalSize) => f.write_str("output size limit exceeded"),
            Error::LimitExceeded(LimitKind::Ratio) => {
                f.write_str("compression ratio limit exceeded (possible decompression bomb)")
            }
            Error::LimitExceeded(LimitKind::EntrySize) => {
                f.write_str("an entry is larger than its header declares")
            }
            Error::Cancelled => f.write_str("cancelled"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

impl Error {
    /// Maps an error raised while *reading* archive data. Decoders report
    /// damaged data as `InvalidData` / `UnexpectedEof` I/O errors.
    pub(crate) fn from_read(e: io::Error) -> Error {
        match e.kind() {
            io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof | io::ErrorKind::InvalidInput => {
                Error::Corrupt(e.to_string())
            }
            _ => Error::Io(e),
        }
    }
}
