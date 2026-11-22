//! Everything that can go wrong while fetching and unpacking a tool.

use std::{error::Error, fmt, io, path::PathBuf};

#[derive(Debug)]
pub enum InstallError {
    /// No connection, an HTTP error status, or the connection broke off
    /// during the download.
    Download { url: String, source: ureq::Error },
    /// The download is complete but not the file we pinned.
    Checksum {
        url: String,
        expected: [u8; 32],
        actual: [u8; 32],
    },
    /// Reading, writing, creating or moving a local file failed.
    Fs {
        action: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    /// The archive is broken or not laid out as expected. `source` is the
    /// error of whichever archive crate is used (zip, tar, flate2, …).
    Unpack {
        archive: String,
        source: Box<dyn Error + Send + Sync>,
    },
}

impl InstallError {
    pub fn download(url: impl Into<String>, source: impl Into<ureq::Error>) -> Self {
        Self::Download {
            url: url.into(),
            source: source.into(),
        }
    }

    /// `action` completes "Konnte … nicht …", e.g. "schreiben" or "anlegen".
    pub fn fs(action: &'static str, path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Fs {
            action,
            path: path.into(),
            source,
        }
    }

    pub fn unpack(
        archive: impl Into<String>,
        source: impl Into<Box<dyn Error + Send + Sync>>,
    ) -> Self {
        Self::Unpack {
            archive: archive.into(),
            source: source.into(),
        }
    }

    /// Short reason for the progress line (must fit "Fehler: …" into its column).
    pub fn short(&self) -> &'static str {
        match self {
            Self::Download { .. } => "Download",
            Self::Checksum { .. } => "Prüfsumme",
            Self::Fs { .. } => "Dateisystem",
            Self::Unpack { .. } => "Entpacken",
        }
    }
}

/// Fails with [`InstallError::Checksum`] unless `data` has the SHA-256 `expected`.
pub fn verify_sha256(url: &str, data: &[u8], expected: &[u8; 32]) -> Result<(), InstallError> {
    use sha2::{Digest, Sha256};

    let actual: [u8; 32] = Sha256::digest(data).into();
    if &actual == expected {
        Ok(())
    } else {
        Err(InstallError::Checksum {
            url: url.to_owned(),
            expected: *expected,
            actual,
        })
    }
}

impl fmt::Display for InstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Download {
                url,
                source: ureq::Error::StatusCode(status),
            } => write!(
                f,
                "Der Server hat den Download von {url} abgelehnt (HTTP {status})."
            ),
            Self::Download { url, source } => write!(
                f,
                "Der Download von {url} ist fehlgeschlagen. Besteht eine Internetverbindung?\n  ({source})"
            ),
            Self::Checksum {
                url,
                expected,
                actual,
            } => write!(
                f,
                "Die Datei von {url} ist nicht die erwartete (beschädigt oder verändert).\n  \
                 erwartet: {}\n  erhalten: {}",
                Hex(expected),
                Hex(actual)
            ),
            Self::Fs {
                action,
                path,
                source,
            } => write!(f, "Konnte {} nicht {action}.\n  ({source})", path.display()),
            Self::Unpack { archive, source } => {
                write!(f, "Konnte {archive} nicht entpacken.\n  ({source})")
            }
        }
    }
}

impl Error for InstallError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Download { source, .. } => Some(source),
            Self::Checksum { .. } => None,
            Self::Fs { source, .. } => Some(source),
            Self::Unpack { source, .. } => Some(source.as_ref()),
        }
    }
}

struct Hex<'a>(&'a [u8]);

impl fmt::Display for Hex<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}
