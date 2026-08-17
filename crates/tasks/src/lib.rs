//! Task board service — parallel work queue for agents and users.

pub mod error;
pub mod service;
pub mod types;

pub use error::{Error, Result};
pub use service::TaskService;
pub use types::{AddResponseRequest, CreateTaskRequest, Task, TaskResponse, TaskStatus, UpdateTaskRequest};

pub async fn run_migrations(pool: &sqlx::SqlitePool) -> Result<()> {
    sqlx::migrate!("./migrations")
        .run(pool)
        .await
        .map_err(|e| Error::Database(e.to_string()))
}
