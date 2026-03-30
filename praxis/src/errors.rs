use thiserror::Error;

#[derive(Debug, Error)]
pub enum RecurrenceError {
    #[error("failed to parse recurrence spec: {0}")]
    Parse(String),
}
