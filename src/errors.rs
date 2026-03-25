use std::io;
use thiserror::Error;

#[derive(Debug, Error)]
#[non_exhaustive]
/// Errors returned from taskchampion operations
pub enum Error {
    /// A task-database-related error
    #[error("Task Database Error: {0}")]
    Database(String),
    /// A usage error
    #[error("Usage Error: {0}")]
    Usage(String),
    /// A referenced task was not found
    #[error("Task not found: {0}")]
    TaskNotFound(uuid::Uuid),
    /// A task already exists with this UUID
    #[error("Task already exists: {0}")]
    TaskAlreadyExists(uuid::Uuid),
    /// A general error.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Convert private and third party errors into Error::Other.
macro_rules! other_error {
    ( $error:ty ) => {
        impl From<$error> for Error {
            fn from(err: $error) -> Self {
                Self::Other(err.into())
            }
        }
    };
}
other_error!(io::Error);
other_error!(serde_json::Error);
other_error!(tokio::sync::oneshot::error::RecvError);

#[cfg(feature = "storage-powersync")]
other_error!(rusqlite::Error);

impl<T: Sync + Send + 'static> From<tokio::sync::mpsc::error::SendError<T>> for Error {
    fn from(err: tokio::sync::mpsc::error::SendError<T>) -> Self {
        Self::Other(err.into())
    }
}

pub(crate) type Result<T> = std::result::Result<T, Error>;
