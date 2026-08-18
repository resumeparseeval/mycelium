//! Task board REST API endpoints.

use std::sync::OnceLock;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json,
    routing::{get, post},
    Router,
};
use serde::Deserialize;
use sqlx::SqlitePool;

use crate::server::AppState;
use moltis_tasks::{AddResponseRequest, CreateTaskRequest, Task, TaskService, TaskStatus, UpdateTaskRequest};

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    status: Option<String>,
}

pub fn tasks_router() -> Router<AppState> {
    Router::new()
        .route("/", post(create_task).get(list_tasks))
        .route("/:id", get(get_task).put(update_task))
        .route("/:id/responses", post(add_task_response))
}

async fn get_service() -> Result<TaskService, StatusCode> {
    static POOL: OnceLock<SqlitePool> = OnceLock::new();

    let pool = match POOL.get() {
        Some(p) => p.clone(),
        None => {
            let data_dir = moltis_config::data_dir();
            let db_path = data_dir.join("moltis.db");
            let new_pool = SqlitePool::connect(&format!("sqlite:{}", db_path.display()))
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            let _ = POOL.set(new_pool.clone());
            new_pool
        }
    };

    Ok(TaskService::new(pool))
}

async fn create_task(
    State(_appstate): State<AppState>,
    Json(req): Json<CreateTaskRequest>,
) -> Result<Json<Task>, StatusCode> {
    let svc = get_service().await?;
    svc.create(req)
        .await
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn list_tasks(
    State(_appstate): State<AppState>,
    Query(params): Query<ListQuery>,
) -> Result<Json<Vec<Task>>, StatusCode> {
    let svc = get_service().await?;
    let status = params.status.and_then(|s| TaskStatus::from_str(&s));
    svc.list(status)
        .await
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn get_task(
    State(_appstate): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Task>, StatusCode> {
    let svc = get_service().await?;
    svc.get(&id)
        .await
        .map(Json)
        .map_err(|_| StatusCode::NOT_FOUND)
}

async fn update_task(
    State(_appstate): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateTaskRequest>,
) -> Result<Json<Task>, StatusCode> {
    let svc = get_service().await?;
    svc.update(&id, req)
        .await
        .map(Json)
        .map_err(|_| StatusCode::NOT_FOUND)
}

async fn add_task_response(
    State(_appstate): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<AddResponseRequest>,
) -> Result<Json<moltis_tasks::TaskResponse>, StatusCode> {
    let svc = get_service().await?;
    svc.add_response(&id, req)
        .await
        .map(Json)
        .map_err(|_| StatusCode::NOT_FOUND)
}
