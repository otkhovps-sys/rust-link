//! OneDrive (Microsoft Graph) backup of the audio cache.

pub mod auth;
pub mod client;
pub mod quickxor;
pub mod sync_worker;

pub use client::{DriveItem, OneDriveClient, UploadSession};
pub use sync_worker::SyncWorker;

#[derive(Debug, thiserror::Error)]
pub enum OneDriveError {
    #[error("onedrive authentication: {0}")]
    Auth(String),
    #[error("graph api error {status} {code}: {message}")]
    Api { status: u16, code: String, message: String },
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("upload session expired")]
    SessionExpired,
    #[error("integrity check failed: {0}")]
    Integrity(String),
}

impl OneDriveError {
    pub fn is_not_found(&self) -> bool {
        matches!(self, OneDriveError::Api { status: 404, .. })
    }
}
