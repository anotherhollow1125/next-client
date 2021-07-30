use thiserror::Error;

#[derive(Error, Debug)]
pub enum NcsError {
    #[error("Failed Locking.")]
    LockError,
    #[error("Bad status {0}.")]
    BadStatusError(u16),
    #[error("Invalid XML.")]
    InvalidXMLError,
    #[error("Failed upgrade Weak")]
    WeakUpgradeError,
    #[error("Invalid path. {0}")]
    InvalidPathError(String),
}
