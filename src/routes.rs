use crate::{
    jobs::{Job, JobStatus},
    service::{AppState, QueuePriority, sanitize_filename, unique_path_for_filename},
};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{Multipart, Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::path::Path as StdPath;
use tokio::{fs, fs::File, io::AsyncWriteExt};
use uuid::Uuid;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/files", post(upload_files))
        .route("/text", post(upload_text))
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

#[derive(Debug, Deserialize)]
struct TextUploadRequest {
    text: String,
    filename: Option<String>,
}

async fn upload_files(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<UploadResponse>), ApiError> {
    let priority = queue_priority_from_headers(&headers);
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
        let job = state.enqueue_path_with_priority(final_path, priority)?;
        jobs.push(job);
    }

    if jobs.is_empty() {
        return Err(ApiError::bad_request(
            "multipart upload did not include any file fields",
        ));
    }

    Ok((StatusCode::CREATED, Json(UploadResponse { jobs })))
}

async fn upload_text(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<UploadResponse>), ApiError> {
    let (text, filename) = parse_text_upload(&headers, &body)?;
    if text.trim().is_empty() {
        return Err(ApiError::bad_request("text upload body is empty"));
    }

    let safe_name = text_upload_filename(filename.as_deref());
    let final_path = unique_path_for_filename(&state.config.input_dir, &safe_name).await?;
    let temp_name = format!(".{}.{}.uploading", safe_name, Uuid::new_v4());
    let temp_path = state.config.input_dir.join(temp_name);

    fs::write(&temp_path, text.as_bytes()).await?;
    fs::rename(&temp_path, &final_path).await?;
    let priority = queue_priority_from_headers(&headers);
    let job = state.enqueue_path_with_priority(final_path, priority)?;

    Ok((
        StatusCode::CREATED,
        Json(UploadResponse { jobs: vec![job] }),
    ))
}

const PRIORITY_HEADER: &str = "x-universal-drop-priority";
const SOURCE_HEADER: &str = "x-universal-drop-source";

fn queue_priority_from_headers(headers: &HeaderMap) -> QueuePriority {
    let priority = headers
        .get(PRIORITY_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .unwrap_or("");
    let source = headers
        .get(SOURCE_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .unwrap_or("");

    if matches_priority_value(priority) || source.eq_ignore_ascii_case("cli") {
        QueuePriority::High
    } else {
        QueuePriority::Normal
    }
}

fn matches_priority_value(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "high" | "priority" | "prioritized" | "cli" | "front" | "first"
    )
}

fn parse_text_upload(
    headers: &HeaderMap,
    body: &[u8],
) -> Result<(String, Option<String>), ApiError> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");

    if content_type
        .split(';')
        .next()
        .map(|value| value.trim().eq_ignore_ascii_case("application/json"))
        .unwrap_or(false)
    {
        let request: TextUploadRequest = serde_json::from_slice(body)
            .map_err(|error| ApiError::bad_request(format!("invalid JSON text upload: {error}")))?;
        return Ok((request.text, request.filename));
    }

    let text = std::str::from_utf8(body)
        .map_err(|error| ApiError::bad_request(format!("text upload must be UTF-8: {error}")))?
        .to_string();
    Ok((text, None))
}

fn text_upload_filename(filename: Option<&str>) -> String {
    let fallback = format!("text-drop-{}.txt", Utc::now().format("%Y%m%d%H%M%S"));
    let safe_name = sanitize_filename(filename.unwrap_or(&fallback));
    let extension = StdPath::new(&safe_name)
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase());
    if matches!(extension.as_deref(), Some("txt" | "text" | "url" | "urls")) {
        safe_name
    } else {
        format!("{safe_name}.txt")
    }
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
        let state = build_state(config);
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
        let state = build_state(config);
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

    #[tokio::test]
    async fn text_endpoint_stores_plain_text_and_returns_job() {
        let temp = tempdir().unwrap();
        let config = test_config(temp.path());
        config.ensure_dirs().unwrap();
        let input_dir = config.input_dir.clone();
        let state = build_state(config);
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/text")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"text":"https://example.com/watch\n","filename":"links.txt"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::CREATED);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["jobs"][0]["filename"], "links.txt");
        assert_eq!(
            fs::read_to_string(input_dir.join("links.txt"))
                .await
                .unwrap(),
            "https://example.com/watch\n"
        );
    }

    fn test_config(root: &Path) -> Config {
        Config {
            input_dir: root.join("input"),
            results_dir: root.join("results"),
            archive_dir: root.join("archive"),
            failed_dir: root.join("failed"),
            bind_addr: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
            ollama_base_url: "http://localhost:11434".to_string(),
            ollama_model: "glm-ocr".to_string(),
            ollama_keep_alive: "30m".to_string(),
            ollama_num_thread: 8,
            ollama_timeout_seconds: 600,
            gemini_ocr_enabled: true,
            gemini_api_key: None,
            gemini_api_key_header: "api-key".to_string(),
            gemini_api_endpoint:
                "https://api.hku.hk/gemini/student/{deployment-id}:generateContent".to_string(),
            gemini_deployment_id: "gemini-3-flash-preview".to_string(),
            gemini_thinking_budget: None,
            gemini_timeout_seconds: 180,
            gemini_min_interval_seconds: 21,
            whisper_model_path: root.join("models/ggml-large-v3.bin"),
            whisper_cli: "whisper-cli".to_string(),
            whisper_threads: 8,
            whisper_processors: 1,
            whisper_beam_size: 1,
            whisper_best_of: 1,
            whisper_no_fallback: true,
            pdf_render_dpi: 150,
            pdf_auto_orient: true,
            pdf_auto_orient_cli: "pdf-page-auto-orient".to_string(),
            pdf_orient_ocr_confirm: true,
            pdf_orient_ocr_cli: "tesseract".to_string(),
            pdf_orient_ocr_lang: "eng".to_string(),
            pdf_orient_ocr_min_confidence: 0.60,
            pdf_orient_ocr_min_score: 20.0,
            url_max_per_text: 8,
            yt_dlp_cli: "yt-dlp".to_string(),
            headless_browser_cli: "chromium".to_string(),
            webpage_capture_virtual_time_ms: 5_000,
            video_min_frames: 3,
            video_max_frames: 24,
            video_scene_threshold: 0.35,
            max_csv_rows: 1_000,
            max_job_attempts: 1,
            job_retry_backoff: Duration::from_millis(1),
            file_stability_checks: 1,
            file_stability_delay: Duration::from_millis(1),
        }
    }
}
