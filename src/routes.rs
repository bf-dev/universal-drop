use crate::{
    jobs::{Job, JobStatus},
    service::{AppState, sanitize_filename, unique_path_for_filename},
};
use axum::{
    Json, Router,
    extract::{Multipart, Path, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Serialize;
use tokio::{fs, fs::File, io::AsyncWriteExt};
use uuid::Uuid;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/files", post(upload_files))
        .route("/jobs", get(list_jobs))
        .route("/jobs/{id}", get(get_job))
        .route("/jobs/{id}/result", get(get_job_result))
        .with_state(state)
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

#[derive(Debug, Serialize)]
struct JobsResponse {
    jobs: Vec<Job>,
}

#[derive(Debug, Serialize)]
struct UploadResponse {
    jobs: Vec<Job>,
}

async fn upload_files(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<UploadResponse>), ApiError> {
    let mut jobs = Vec::new();

    while let Some(mut field) = multipart.next_field().await? {
        let Some(original_name) = field.file_name().map(ToString::to_string) else {
            continue;
        };
        let safe_name = sanitize_filename(&original_name);
        let final_path = unique_path_for_filename(&state.config.input_dir, &safe_name).await?;
        let temp_name = format!(".{}.{}.uploading", safe_name, Uuid::new_v4());
        let temp_path = state.config.input_dir.join(temp_name);

        let mut file = File::create(&temp_path).await?;
        while let Some(chunk) = field.chunk().await? {
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        drop(file);

        fs::rename(&temp_path, &final_path).await?;
        let job = state.enqueue_path(final_path)?;
        jobs.push(job);
    }

    if jobs.is_empty() {
        return Err(ApiError::bad_request(
            "multipart upload did not include any file fields",
        ));
    }

    Ok((StatusCode::CREATED, Json(UploadResponse { jobs })))
}

async fn list_jobs(State(state): State<AppState>) -> Json<JobsResponse> {
    Json(JobsResponse {
        jobs: state.jobs.list_recent(100),
    })
}

async fn get_job(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Job>, ApiError> {
    state
        .jobs
        .get(id)
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("job not found: {id}")))
}

async fn get_job_result(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    let job = state
        .jobs
        .get(id)
        .ok_or_else(|| ApiError::not_found(format!("job not found: {id}")))?;
    if job.status != JobStatus::Succeeded {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            format!("job {id} is not complete; status is {:?}", job.status),
        ));
    }
    let markdown = fs::read_to_string(&job.result_path)
        .await
        .map_err(|error| {
            ApiError::new(StatusCode::NOT_FOUND, format!("result not found: {error}"))
        })?;
    Ok((
        [(header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
        markdown,
    ))
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
}

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorResponse {
                error: self.message,
            }),
        )
            .into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}"))
    }
}

impl From<std::io::Error> for ApiError {
    fn from(error: std::io::Error) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
    }
}

impl From<axum::extract::multipart::MultipartError> for ApiError {
    fn from(error: axum::extract::multipart::MultipartError) -> Self {
        Self::new(StatusCode::BAD_REQUEST, error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::router;
    use crate::{config::Config, service::build_state};
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode, header},
    };
    use serde_json::Value;
    use std::{net::SocketAddr, path::Path, time::Duration};
    use tempfile::tempdir;
    use tokio::fs;
    use tower::ServiceExt;

    #[tokio::test]
    async fn health_endpoint_returns_ok() {
        let temp = tempdir().unwrap();
        let config = test_config(temp.path());
        config.ensure_dirs().unwrap();
        let (state, _rx) = build_state(config);
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["status"], "ok");
    }

    #[tokio::test]
    async fn upload_endpoint_stores_file_and_returns_job() {
        let temp = tempdir().unwrap();
        let config = test_config(temp.path());
        config.ensure_dirs().unwrap();
        let input_dir = config.input_dir.clone();
        let (state, _rx) = build_state(config);
        let app = router(state);

        let boundary = "universal-drop-test-boundary";
        let body = format!(
            "--{boundary}\r\n\
             Content-Disposition: form-data; name=\"file\"; filename=\"hello.txt\"\r\n\
             Content-Type: text/plain\r\n\r\n\
             hello world\r\n\
             --{boundary}--\r\n"
        );
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/files")
                    .header(
                        header::CONTENT_TYPE,
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::CREATED);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["jobs"][0]["filename"], "hello.txt");
        assert_eq!(
            fs::read_to_string(input_dir.join("hello.txt"))
                .await
                .unwrap(),
            "hello world"
        );
    }

    fn test_config(root: &Path) -> Config {
        Config {
            input_dir: root.join("input"),
            results_dir: root.join("results"),
            archive_dir: root.join("archive"),
            bind_addr: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
            ollama_base_url: "http://localhost:11434".to_string(),
            ollama_model: "glm-ocr".to_string(),
            ollama_keep_alive: "5m".to_string(),
            whisper_model_path: root.join("models/ggml-large-v3.bin"),
            whisper_cli: "whisper-cli".to_string(),
            video_min_frames: 3,
            video_max_frames: 24,
            video_scene_threshold: 0.35,
            max_csv_rows: 1_000,
            file_stability_checks: 1,
            file_stability_delay: Duration::from_millis(1),
        }
    }
}
