// unified error type for plump
//
// single Copy type carrying ErrorKind (what) and a &'static str
// source tag (where); smol-epub boundary converts via From impls

use core::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    // storage / sd card
    NoCard,
    OpenDir,
    OpenFile,
    ReadFailed,
    WriteFailed,
    SeekFailed,
    DeleteFailed,
    NotFound,

    // data / parsing
    ParseFailed,
    InvalidData,
    BadEncoding,

    // resources
    OutOfMemory,

    // network (upload)
    NetworkIo,
    Protocol,

    // catch-all
    Other,
}

impl ErrorKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoCard => "no sd card",
            Self::OpenDir => "open dir failed",
            Self::OpenFile => "open file failed",
            Self::ReadFailed => "read failed",
            Self::WriteFailed => "write failed",
            Self::SeekFailed => "seek failed",
            Self::DeleteFailed => "delete failed",
            Self::NotFound => "not found",
            Self::ParseFailed => "parse failed",
            Self::InvalidData => "invalid data",
            Self::BadEncoding => "bad encoding",
            Self::OutOfMemory => "out of memory",
            Self::NetworkIo => "network error",
            Self::Protocol => "protocol error",
            Self::Other => "error",
        }
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// one discriminant byte + one &'static str pointer; cheap to copy
// source is module_path!() or a short caller-supplied tag
#[derive(Clone, Copy)]
pub struct Error {
    kind: ErrorKind,
    source: &'static str,
}

impl Error {
    #[inline]
    pub const fn new(kind: ErrorKind, source: &'static str) -> Self {
        Self { kind, source }
    }

}

impl Error {
    #[inline]
    pub const fn kind(&self) -> ErrorKind {
        self.kind
    }

    #[inline]
    pub const fn source_tag(&self) -> &'static str {
        self.source
    }

    #[inline]
    pub const fn with_source(self, source: &'static str) -> Self {
        Self {
            kind: self.kind,
            source,
        }
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.source.is_empty() {
            write!(f, "Error({:?})", self.kind)
        } else {
            write!(f, "Error({:?} @ {:?})", self.kind, self.source)
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.source.is_empty() {
            f.write_str(self.kind.as_str())
        } else {
            write!(f, "{} [{}]", self.kind.as_str(), self.source)
        }
    }
}

// equality is semantic: kind only, source is diagnostic
impl PartialEq for Error {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind
    }
}

impl Eq for Error {}

// wrap &'static str (smol-epub returns) into Error; well-known
// strings map to the appropriate kind, rest becomes Other
impl From<&'static str> for Error {
    #[inline]
    fn from(msg: &'static str) -> Self {
        let kind = match msg {
            "read failed" | "read local header failed" => ErrorKind::ReadFailed,
            "write failed" => ErrorKind::WriteFailed,
            "read error" | "read error during upload" => ErrorKind::NetworkIo,
            "no sd card" => ErrorKind::NoCard,
            "not found" | "OPF not found" | "no filename in upload" => ErrorKind::NotFound,
            "too small" | "CD truncated" | "cache file too small" => ErrorKind::InvalidData,
            "CD too large" | "OOM for cached image" => ErrorKind::OutOfMemory,
            "bad OPF path" | "bad encoding" | "filename encoding error" => ErrorKind::BadEncoding,
            "parse failed" | "no title in OPF" => ErrorKind::ParseFailed,
            "boundary too long"
            | "part headers too large"
            | "invalid filename"
            | "upload incomplete"
            | "connection closed during headers" => ErrorKind::Protocol,
            // the image decoders name the buffer that could not be
            // reserved ("jpeg: OOM for output", "png: OOM for output
            // bitmap", ...); match the shape rather than each string,
            // so callers can tell a budget failure from a bad file
            m if m.contains("OOM") => ErrorKind::OutOfMemory,
            _ => ErrorKind::Other,
        };
        Self { kind, source: msg }
    }
}

// project back to &'static str for the smol-epub trait boundary
impl From<Error> for &'static str {
    #[inline]
    fn from(e: Error) -> &'static str {
        if e.source.is_empty() {
            e.kind.as_str()
        } else {
            e.source
        }
    }
}

pub type Result<T> = core::result::Result<T, Error>;
