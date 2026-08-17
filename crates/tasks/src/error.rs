use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Task not found")]
    NotFound,

    #[error("Database error: {0}")]
    Database(String),

    #[error("Invalid status: {0}")]
    InvalidStatus(String),

    #[error("Invalid request: {0}")]
    InvalidRequest(String),
}
