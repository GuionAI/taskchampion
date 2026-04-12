use std::io;
use thiserror::Error;

#[derive(Debug, Error)]
#[non_exhaustive]
/// Errors returned from taskchampion operations
pub enum Error {
    /// A PostgreSQL error via pgwire
    #[cfg(feature = "storage-pgwire")]
    #[error("Database error: {0}")]
    PgWire(sqlx_core::Error),
    /// A task-database-related error
    #[error("Task Database Error: {0}")]
    Database(String),
    /// A usage error
    #[error("Usage Error: {0}")]
    Usage(String),
    /// A tag was not found in tc_config when trying to add it to a task
    #[error("Tag not registered in tc_config: {0}")]
    TagNotRegistered(String),
    /// A referenced task was not found
    #[error("Task not found: {0}")]
    TaskNotFound(uuid::Uuid),
    /// A task already exists with this UUID
    #[error("Task already exists: {0}")]
    TaskAlreadyExists(uuid::Uuid),
    /// A project name could not be resolved to a UUID
    #[error("Project not found: {0}")]
    ProjectNotFound(String),
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

#[cfg(feature = "storage-pgwire")]
impl From<sqlx_core::Error> for Error {
    #[inline]
    fn from(e: sqlx_core::Error) -> Self {
        Self::PgWire(e)
    }
}

pub(crate) type Result<T> = std::result::Result<T, Error>;
