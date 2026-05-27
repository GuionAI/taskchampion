use std::io;
use thiserror::Error;

#[derive(Debug, Error)]
#[non_exhaustive]
/// Errors returned from taskchampion operations
pub enum Error {
    /// A PostgreSQL error via pgwire
    #[cfg(feature = "storage-pgwire")]
    #[error("Database error: {}", format_pgwire_err(.0))]
    PgWire(sqlx::Error),
    /// PostgreSQL unique constraint violation via pgwire.
    #[cfg(feature = "storage-pgwire")]
    #[error("Database error: unique violation: {0}")]
    UniqueViolation(String),
    /// PostgreSQL foreign key constraint violation via pgwire.
    #[cfg(feature = "storage-pgwire")]
    #[error("Database error: foreign key violation: {0}")]
    ForeignKeyViolation(String),
    /// A PostgreSQL error via pgwire, with the operation that failed.
    #[cfg(feature = "storage-pgwire")]
    #[error("Database error: {context}: {}", format_pgwire_err(.source))]
    PgWireQuery {
        context: String,
        source: sqlx::Error,
    },
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

impl<T: Sync + Send + 'static> From<tokio::sync::mpsc::error::SendError<T>> for Error {
    fn from(err: tokio::sync::mpsc::error::SendError<T>) -> Self {
        Self::Other(err.into())
    }
}

#[cfg(any(feature = "storage-pgwire", feature = "storage-powersync"))]
impl From<sqlx::Error> for Error {
    #[inline]
    fn from(e: sqlx::Error) -> Self {
        #[cfg(feature = "storage-pgwire")]
        {
            classify_pg(e)
        }
        #[cfg(not(feature = "storage-pgwire"))]
        {
            Self::Database(format!("SQLx error: {e}"))
        }
    }
}

#[cfg(feature = "storage-pgwire")]
pub(crate) fn pgwire_context(context: impl Into<String>, source: sqlx::Error) -> Error {
    let context = context.into();
    if let Some(classified) = classify_pg_message(
        &source,
        format!("{context}: {}", format_pgwire_err(&source)),
    ) {
        return classified;
    }

    Error::PgWireQuery { context, source }
}

#[cfg(feature = "storage-pgwire")]
pub(crate) fn classify_pg(e: sqlx::Error) -> Error {
    classify_pg_message(&e, format_pgwire_err(&e)).unwrap_or(Error::PgWire(e))
}

#[cfg(feature = "storage-pgwire")]
fn classify_pg_message(e: &sqlx::Error, message: String) -> Option<Error> {
    match pg_sqlstate_kind(e)? {
        PgSqlStateKind::UniqueViolation => Some(Error::UniqueViolation(message)),
        PgSqlStateKind::ForeignKeyViolation => Some(Error::ForeignKeyViolation(message)),
    }
}

#[cfg(feature = "storage-pgwire")]
fn pg_sqlstate_kind(e: &sqlx::Error) -> Option<PgSqlStateKind> {
    let db = e.as_database_error()?;
    let code = db.code()?;
    match &*code {
        "23505" => Some(PgSqlStateKind::UniqueViolation),
        "23503" => Some(PgSqlStateKind::ForeignKeyViolation),
        _ => None,
    }
}

#[cfg(feature = "storage-pgwire")]
enum PgSqlStateKind {
    UniqueViolation,
    ForeignKeyViolation,
}

#[cfg(feature = "storage-pgwire")]
pub(crate) fn format_pgwire_err(e: &sqlx::Error) -> String {
    let Some(db) = e.as_database_error() else {
        return e.to_string();
    };

    let mut out = String::new();
    if let Some(code) = db.code() {
        out.push_str("SQLSTATE ");
        out.push_str(&code);
        out.push_str(": ");
    }
    out.push_str(db.message());

    if let Some(pg) = db.try_downcast_ref::<sqlx::postgres::PgDatabaseError>() {
        append_pg_field(&mut out, "schema", pg.schema());
        append_pg_field(&mut out, "table", pg.table());
        append_pg_field(&mut out, "column", pg.column());
        append_pg_field(&mut out, "constraint", pg.constraint());
        append_pg_field(&mut out, "detail", pg.detail());
        append_pg_field(&mut out, "hint", pg.hint());
    } else {
        append_pg_field(&mut out, "table", db.table());
        append_pg_field(&mut out, "constraint", db.constraint());
    }

    out
}

#[cfg(feature = "storage-pgwire")]
fn append_pg_field(out: &mut String, name: &str, value: Option<&str>) {
    if let Some(value) = value.filter(|v| !v.is_empty()) {
        out.push_str(" (");
        out.push_str(name);
        out.push_str(": ");
        out.push_str(value);
        out.push(')');
    }
}

#[cfg(all(test, feature = "storage-pgwire"))]
mod tests {
    use super::*;
    use sqlx::error::{DatabaseError, ErrorKind};
    use std::borrow::Cow;
    use std::error::Error as StdError;
    use std::fmt;

    #[derive(Debug)]
    struct FakeDatabaseError {
        code: Option<&'static str>,
        message: &'static str,
    }

    impl fmt::Display for FakeDatabaseError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.message)
        }
    }

    impl StdError for FakeDatabaseError {}

    impl DatabaseError for FakeDatabaseError {
        fn message(&self) -> &str {
            self.message
        }

        fn code(&self) -> Option<Cow<'_, str>> {
            self.code.map(Cow::Borrowed)
        }

        fn as_error(&self) -> &(dyn StdError + Send + Sync + 'static) {
            self
        }

        fn as_error_mut(&mut self) -> &mut (dyn StdError + Send + Sync + 'static) {
            self
        }

        fn into_error(self: Box<Self>) -> Box<dyn StdError + Send + Sync + 'static> {
            self
        }

        fn kind(&self) -> ErrorKind {
            ErrorKind::Other
        }
    }

    fn db_error(code: Option<&'static str>) -> sqlx::Error {
        sqlx::Error::Database(Box::new(FakeDatabaseError {
            code,
            message: "synthetic database error",
        }))
    }

    #[test]
    fn classify_pg_unique_violation() {
        match classify_pg(db_error(Some("23505"))) {
            Error::UniqueViolation(message) => {
                assert!(message.contains("SQLSTATE 23505"));
                assert!(message.contains("synthetic database error"));
            }
            other => panic!("expected unique violation, got {other:?}"),
        }
    }

    #[test]
    fn classify_pg_foreign_key_violation() {
        match classify_pg(db_error(Some("23503"))) {
            Error::ForeignKeyViolation(message) => {
                assert!(message.contains("SQLSTATE 23503"));
                assert!(message.contains("synthetic database error"));
            }
            other => panic!("expected foreign key violation, got {other:?}"),
        }
    }

    #[test]
    fn classify_pg_other_database_error_falls_through() {
        match classify_pg(db_error(Some("99999"))) {
            Error::PgWire(_) => {}
            other => panic!("expected pgwire fallback, got {other:?}"),
        }
    }

    #[test]
    fn classify_pg_non_database_error_falls_through() {
        match classify_pg(sqlx::Error::RowNotFound) {
            Error::PgWire(_) => {}
            other => panic!("expected pgwire fallback, got {other:?}"),
        }
    }

    #[test]
    fn pgwire_context_classifies_and_keeps_context() {
        match pgwire_context("task_exists query uuid=1234abcd", db_error(Some("23505"))) {
            Error::UniqueViolation(message) => {
                assert!(message.contains("task_exists query uuid=1234abcd"));
                assert!(message.contains("SQLSTATE 23505"));
            }
            other => panic!("expected unique violation with context, got {other:?}"),
        }
    }
}

pub(crate) type Result<T> = std::result::Result<T, Error>;
