use chrono::Utc;
use sqlx::SqlitePool;

use crate::error::{Error, Result};
use crate::types::{AddResponseRequest, CreateTaskRequest, Task, TaskResponse, TaskStatus, UpdateTaskRequest};

#[derive(Clone)]
pub struct TaskService {
    pool: SqlitePool,
}

impl TaskService {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn create(&self, req: CreateTaskRequest) -> Result<Task> {
        let task = Task::new(req.title, req.description);
        let status = task.status.as_str();
        let priority = req.priority.unwrap_or(0);

        sqlx::query(
            "INSERT INTO tasks (id, title, description, status, priority, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&task.id)
        .bind(&task.title)
        .bind(&task.description)
        .bind(status)
        .bind(priority)
        .bind(task.created_at)
        .bind(task.updated_at)
        .execute(&self.pool)
        .await
        .map_err(|e| Error::Database(e.to_string()))?;

        Ok(task)
    }

    pub async fn get(&self, task_id: &str) -> Result<Task> {
        let row = sqlx::query_as::<_, (String, String, Option<String>, String, Option<String>, i32, String, String, Option<String>)>(
            "SELECT id, title, description, status, assigned_to, priority, created_at, updated_at, completed_at FROM tasks WHERE id = ?",
        )
        .bind(task_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| Error::Database(e.to_string()))?
        .ok_or(Error::NotFound)?;

        let responses = self.get_task_responses(task_id).await?;

        Ok(Task {
            id: row.0,
            title: row.1,
            description: row.2,
            status: TaskStatus::from_str(&row.3).unwrap_or(TaskStatus::Open),
            assigned_to: row.4,
            priority: row.5,
            created_at: chrono::DateTime::parse_from_rfc3339(&row.6)
                .ok()
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(Utc::now),
            updated_at: chrono::DateTime::parse_from_rfc3339(&row.7)
                .ok()
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(Utc::now),
            completed_at: row.8.and_then(|s| {
                chrono::DateTime::parse_from_rfc3339(&s)
                    .ok()
                    .map(|dt| dt.with_timezone(&Utc))
            }),
            responses,
        })
    }

    pub async fn list(&self, status: Option<TaskStatus>) -> Result<Vec<Task>> {
        let rows = if let Some(s) = status {
            sqlx::query_as::<_, (String, String, Option<String>, String, Option<String>, i32, String, String, Option<String>)>(
                "SELECT id, title, description, status, assigned_to, priority, created_at, updated_at, completed_at FROM tasks WHERE status = ? ORDER BY priority DESC, created_at DESC",
            )
            .bind(s.as_str())
            .fetch_all(&self.pool)
            .await
        } else {
            sqlx::query_as::<_, (String, String, Option<String>, String, Option<String>, i32, String, String, Option<String>)>(
                "SELECT id, title, description, status, assigned_to, priority, created_at, updated_at, completed_at FROM tasks ORDER BY priority DESC, created_at DESC",
            )
            .fetch_all(&self.pool)
            .await
        }
        .map_err(|e| Error::Database(e.to_string()))?;

        let mut tasks = Vec::new();
        for row in rows {
            let responses = self.get_task_responses(&row.0).await?;
            tasks.push(Task {
                id: row.0,
                title: row.1,
                description: row.2,
                status: TaskStatus::from_str(&row.3).unwrap_or(TaskStatus::Open),
                assigned_to: row.4,
                priority: row.5,
                created_at: chrono::DateTime::parse_from_rfc3339(&row.6)
                    .ok()
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(Utc::now),
                updated_at: chrono::DateTime::parse_from_rfc3339(&row.7)
                    .ok()
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(Utc::now),
                completed_at: row.8.and_then(|s| {
                    chrono::DateTime::parse_from_rfc3339(&s)
                        .ok()
                        .map(|dt| dt.with_timezone(&Utc))
                }),
                responses,
            });
        }

        Ok(tasks)
    }

    pub async fn update(&self, task_id: &str, req: UpdateTaskRequest) -> Result<Task> {
        let current = self.get(task_id).await?;

        let title = req.title.unwrap_or(current.title);
        let description = req.description.or(current.description);
        let status = req.status.unwrap_or(current.status);
        let assigned_to = req.assigned_to.or(current.assigned_to);
        let priority = req.priority.unwrap_or(current.priority);
        let now = Utc::now();
        let completed_at = if status == TaskStatus::Completed {
            Some(now)
        } else {
            current.completed_at
        };

        sqlx::query(
            "UPDATE tasks SET title = ?, description = ?, status = ?, assigned_to = ?, priority = ?, updated_at = ?, completed_at = ?
             WHERE id = ?",
        )
        .bind(&title)
        .bind(&description)
        .bind(status.as_str())
        .bind(&assigned_to)
        .bind(priority)
        .bind(now)
        .bind(completed_at)
        .bind(task_id)
        .execute(&self.pool)
        .await
        .map_err(|e| Error::Database(e.to_string()))?;

        self.get(task_id).await
    }

    pub async fn add_response(&self, task_id: &str, req: AddResponseRequest) -> Result<TaskResponse> {
        let _task = self.get(task_id).await?;

        let response = TaskResponse::new(task_id.to_string(), req.agent_name, req.content);

        sqlx::query("INSERT INTO task_responses (id, task_id, agent_name, content, created_at) VALUES (?, ?, ?, ?, ?)")
            .bind(&response.id)
            .bind(task_id)
            .bind(&response.agent_name)
            .bind(&response.content)
            .bind(response.created_at)
            .execute(&self.pool)
            .await
            .map_err(|e| Error::Database(e.to_string()))?;

        Ok(response)
    }

    async fn get_task_responses(&self, task_id: &str) -> Result<Vec<TaskResponse>> {
        let rows = sqlx::query_as::<_, (String, String, String, String, String)>(
            "SELECT id, task_id, agent_name, content, created_at FROM task_responses WHERE task_id = ? ORDER BY created_at ASC",
        )
        .bind(task_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| Error::Database(e.to_string()))?;

        Ok(rows
            .into_iter()
            .map(|row| TaskResponse {
                id: row.0,
                task_id: row.1,
                agent_name: row.2,
                content: row.3,
                created_at: chrono::DateTime::parse_from_rfc3339(&row.4)
                    .ok()
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(Utc::now),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn create_and_get_task() {
        let db = sqlx::sqlite::SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();
        crate::run_migrations(&db).await.unwrap();

        let svc = TaskService::new(db);
        let task = svc
            .create(CreateTaskRequest {
                title: "Test task".to_string(),
                description: None,
                priority: None,
            })
            .await
            .unwrap();

        let fetched = svc.get(&task.id).await.unwrap();
        assert_eq!(fetched.title, "Test task");
        assert_eq!(fetched.status, TaskStatus::Open);
    }
}
