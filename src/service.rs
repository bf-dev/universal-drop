use crate::{
    config::Config,
    convert::convert_to_markdown,
    jobs::{Job, JobStore},
};
use anyhow::{Context, Result, bail};
use reqwest::Client;
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::SystemTime,
};
use tokio::{fs, sync::Notify, task::JoinHandle, time::sleep};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub jobs: JobStore,
    pub client: Client,
    queue: JobQueue,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueuePriority {
    Normal,
    High,
}

#[derive(Debug, Default)]
struct QueueInner {
    high_priority: VecDeque<Uuid>,
    normal_priority: VecDeque<Uuid>,
}

#[derive(Clone, Debug)]
struct JobQueue {
    inner: Arc<Mutex<QueueInner>>,
    notify: Arc<Notify>,
}

impl Default for JobQueue {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(QueueInner::default())),
            notify: Arc::new(Notify::new()),
        }
    }
}

impl JobQueue {
    fn push(&self, job_id: Uuid, priority: QueuePriority) {
        let mut queue = self.inner.lock().expect("job queue mutex poisoned");
        match priority {
            QueuePriority::High => queue.high_priority.push_back(job_id),
            QueuePriority::Normal => queue.normal_priority.push_back(job_id),
        }
        drop(queue);
        self.notify.notify_one();
    }

    async fn pop(&self) -> Uuid {
        loop {
            if let Some(job_id) = self.pop_now() {
                return job_id;
            }
            self.notify.notified().await;
        }
    }

    fn pop_now(&self) -> Option<Uuid> {
        let mut queue = self.inner.lock().expect("job queue mutex poisoned");
        queue
            .high_priority
            .pop_front()
            .or_else(|| queue.normal_priority.pop_front())
    }
}

pub fn build_state(config: Config) -> AppState {
    AppState {
        config: Arc::new(config),
        jobs: JobStore::new(),
        client: Client::new(),
        queue: JobQueue::default(),
    }
}

impl AppState {
    pub fn enqueue_path(&self, path: impl Into<PathBuf>) -> Result<Job> {
        self.enqueue_path_with_priority(path, QueuePriority::Normal)
    }

    pub fn enqueue_path_with_priority(
        &self,
        path: impl Into<PathBuf>,
        priority: QueuePriority,
    ) -> Result<Job> {
        let path = path.into();
        if should_ignore_input_path(&path) {
            bail!("ignored transient input path {}", path.display());
        }
        if !path.is_file() {
            bail!("input path is not a file: {}", path.display());
        }

        let input_path = absolute_path(&path)?;
        let result_path = self.config.result_path_for(&input_path);
        let (job, should_enqueue) = self.jobs.create_or_get(input_path.clone(), result_path);
        if should_enqueue {
            self.queue.push(job.id, priority);
            info!(
                job_id = %job.id,
                path = %input_path.display(),
                priority = ?priority,
                "queued conversion job"
            );
        } else {
            debug!(job_id = %job.id, path = %input_path.display(), "file already has an active job");
        }
        Ok(job)
    }
}

pub fn start_worker(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let job_id = state.queue.pop().await;
            if let Err(error) = process_job(state.clone(), job_id).await {
                error!(job_id = %job_id, error = %error, "conversion job failed unexpectedly");
                state.jobs.mark_failed(job_id, format!("{error:#}"));
            }
        }
    })
}

pub async fn scan_input_dir(state: &AppState) -> Result<Vec<Job>> {
    let mut jobs = Vec::new();
    let mut entries = fs::read_dir(&state.config.input_dir)
        .await
        .with_context(|| {
            format!(
                "failed to scan input dir {}",
                state.config.input_dir.display()
            )
        })?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if should_ignore_input_path(&path) {
            continue;
        }
        let metadata = entry.metadata().await?;
        if metadata.is_file() {
            match state.enqueue_path(path) {
                Ok(job) => jobs.push(job),
                Err(error) => error!(error = %error, "failed to queue discovered input file"),
            }
        }
    }
    Ok(jobs)
}

async fn process_job(state: AppState, job_id: Uuid) -> Result<()> {
    let job = state
        .jobs
        .get(job_id)
        .ok_or_else(|| anyhow::anyhow!("job {job_id} disappeared before processing"))?;

    for attempt in 1..=state.config.max_job_attempts {
        state.jobs.mark_running_attempt(job_id, attempt);
        info!(
            job_id = %job_id,
            path = %job.input_path.display(),
            attempt,
            max_attempts = state.config.max_job_attempts,
            "starting conversion job"
        );

        match run_conversion_once(&state, &job).await {
            Ok(archive_path) => {
                state.jobs.mark_succeeded(job_id, archive_path.clone());
                info!(
                    job_id = %job_id,
                    result = %job.result_path.display(),
                    archive = %archive_path.display(),
                    "conversion job succeeded"
                );
                return Ok(());
            }
            Err(error) if attempt < state.config.max_job_attempts => {
                warn!(
                    job_id = %job_id,
                    attempt,
                    max_attempts = state.config.max_job_attempts,
                    error = %error,
                    "conversion job attempt failed; retrying"
                );
                sleep(state.config.job_retry_backoff).await;
            }
            Err(error) => {
                let mut error_text = format!("{error:#}");
                let failed_path =
                    match move_to_failed(&job.input_path, &state.config.failed_dir).await {
                        Ok(path) => Some(path),
                        Err(move_error) => {
                            error!(
                                job_id = %job_id,
                                path = %job.input_path.display(),
                                error = %move_error,
                                "failed to move failed input out of watched folder"
                            );
                            error_text.push_str(&format!(
                                "\nAlso failed to move source to failed dir: {move_error:#}"
                            ));
                            None
                        }
                    };
                state
                    .jobs
                    .mark_failed_with_path(job_id, error_text, failed_path.clone());
                let failed_path_display = failed_path
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "<not moved>".to_string());
                error!(
                    job_id = %job_id,
                    attempt,
                    max_attempts = state.config.max_job_attempts,
                    failed_path = %failed_path_display,
                    "conversion job failed after max attempts"
                );
                return Ok(());
            }
        }
    }

    Ok(())
}

async fn run_conversion_once(state: &AppState, job: &Job) -> Result<PathBuf> {
    wait_for_file_ready(&job.input_path, &state.config).await?;
    let markdown = convert_to_markdown(&job.input_path, &state.config, &state.client).await?;
    if let Some(parent) = job.result_path.parent() {
        fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create results directory {}", parent.display()))?;
    }
    fs::write(&job.result_path, markdown)
        .await
        .with_context(|| format!("failed to write result {}", job.result_path.display()))?;
    move_to_archive(&job.input_path, &state.config.archive_dir).await
}

pub async fn wait_for_file_ready(path: &Path, config: &Config) -> Result<()> {
    let needed_stable_checks = config.file_stability_checks.max(1);
    let max_attempts = needed_stable_checks.saturating_mul(20).max(20);
    let mut stable_checks = 0usize;
    let mut last_seen: Option<(u64, Option<SystemTime>)> = None;

    for _ in 0..max_attempts {
        let metadata = fs::metadata(path).await.with_context(|| {
            format!(
                "input file disappeared before conversion: {}",
                path.display()
            )
        })?;
        if !metadata.is_file() {
            bail!("input path is not a file: {}", path.display());
        }
        let current = (metadata.len(), metadata.modified().ok());
        if last_seen.as_ref() == Some(&current) {
            stable_checks += 1;
            if stable_checks >= needed_stable_checks {
                return Ok(());
            }
        } else {
            stable_checks = 0;
            last_seen = Some(current);
        }
        sleep(config.file_stability_delay).await;
    }

    bail!(
        "timed out waiting for file to become stable: {}",
        path.display()
    )
}

pub async fn move_to_archive(source: &Path, archive_dir: &Path) -> Result<PathBuf> {
    move_input_to_dir(source, archive_dir, "archive").await
}

pub async fn move_to_failed(source: &Path, failed_dir: &Path) -> Result<PathBuf> {
    move_input_to_dir(source, failed_dir, "failed").await
}

async fn move_input_to_dir(source: &Path, target_dir: &Path, label: &str) -> Result<PathBuf> {
    fs::create_dir_all(target_dir)
        .await
        .with_context(|| format!("failed to create {label} dir {}", target_dir.display()))?;
    let file_name = source
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            anyhow::anyhow!("source path has no valid file name: {}", source.display())
        })?;
    let target = unique_path_for_filename(target_dir, file_name).await?;

    match fs::rename(source, &target).await {
        Ok(()) => Ok(target),
        Err(rename_error) => {
            fs::copy(source, &target).await.with_context(|| {
                format!(
                    "failed to copy {} to {} after rename failed: {rename_error}",
                    source.display(),
                    target.display()
                )
            })?;
            fs::remove_file(source)
                .await
                .with_context(|| format!("failed to remove {label} source {}", source.display()))?;
            Ok(target)
        }
    }
}

pub async fn unique_path_for_filename(dir: &Path, file_name: &str) -> Result<PathBuf> {
    let safe_name = sanitize_filename(file_name);
    let candidate = dir.join(&safe_name);
    if !fs::try_exists(&candidate).await? {
        return Ok(candidate);
    }

    let safe_path = Path::new(&safe_name);
    let stem = safe_path
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("file");
    let extension = safe_path.extension().and_then(|value| value.to_str());
    loop {
        let suffix = Uuid::new_v4().to_string();
        let short = &suffix[..8];
        let name = match extension {
            Some(extension) if !extension.is_empty() => format!("{stem}-{short}.{extension}"),
            _ => format!("{stem}-{short}"),
        };
        let candidate = dir.join(name);
        if !fs::try_exists(&candidate).await? {
            return Ok(candidate);
        }
    }
}

pub fn sanitize_filename(file_name: &str) -> String {
    let mut output = String::with_capacity(file_name.len());
    for ch in file_name.chars() {
        let allowed = ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_' | ' ');
        if allowed {
            output.push(ch);
        } else if matches!(ch, '/' | '\\') {
            output.push('_');
        }
    }

    let output = output.trim().trim_matches('.').to_string();
    if output.is_empty() {
        "upload".to_string()
    } else {
        output
    }
}

pub fn should_ignore_input_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| {
            name.ends_with(".uploading")
                || name.ends_with(".part")
                || name.ends_with(".swp")
                || name == ".DS_Store"
        })
        .unwrap_or(true)
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()
            .context("failed to get current directory")?
            .join(path))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        JobQueue, QueuePriority, build_state, move_to_archive, sanitize_filename,
        should_ignore_input_path, start_worker, unique_path_for_filename,
    };
    use crate::{config::Config, jobs::JobStatus};
    use std::{net::SocketAddr, path::Path};
    use tempfile::tempdir;
    use tokio::fs;
    use tokio::time::{Duration, sleep};

    #[test]
    fn sanitizes_uploaded_filenames() {
        assert_eq!(sanitize_filename("../secret.txt"), "_secret.txt");
        assert_eq!(sanitize_filename("résumé.pdf"), "rsum.pdf");
        assert_eq!(sanitize_filename("..."), "upload");
    }

    #[test]
    fn ignores_transient_input_files() {
        assert!(should_ignore_input_path(Path::new("file.uploading")));
        assert!(should_ignore_input_path(Path::new(".DS_Store")));
        assert!(!should_ignore_input_path(Path::new("real.txt")));
    }

    #[tokio::test]
    async fn priority_queue_pops_high_priority_before_normal_priority() {
        let queue = JobQueue::default();
        let normal_one = uuid::Uuid::new_v4();
        let normal_two = uuid::Uuid::new_v4();
        let high_one = uuid::Uuid::new_v4();
        let high_two = uuid::Uuid::new_v4();

        queue.push(normal_one, QueuePriority::Normal);
        queue.push(normal_two, QueuePriority::Normal);
        queue.push(high_one, QueuePriority::High);
        queue.push(high_two, QueuePriority::High);

        assert_eq!(queue.pop().await, high_one);
        assert_eq!(queue.pop().await, high_two);
        assert_eq!(queue.pop().await, normal_one);
        assert_eq!(queue.pop().await, normal_two);
    }

    #[tokio::test]
    async fn archive_move_uses_unique_name_when_collision_exists() {
        let temp = tempdir().unwrap();
        let input_dir = temp.path().join("input");
        let archive_dir = temp.path().join("archive");
        fs::create_dir_all(&input_dir).await.unwrap();
        fs::create_dir_all(&archive_dir).await.unwrap();
        let source = input_dir.join("a.txt");
        fs::write(&source, "new").await.unwrap();
        fs::write(archive_dir.join("a.txt"), "old").await.unwrap();

        let archived = move_to_archive(&source, &archive_dir).await.unwrap();
        assert!(!fs::try_exists(&source).await.unwrap());
        assert!(
            archived
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("a-")
        );
        assert_eq!(
            fs::read_to_string(archive_dir.join("a.txt")).await.unwrap(),
            "old"
        );
        assert_eq!(fs::read_to_string(archived).await.unwrap(), "new");
    }

    #[tokio::test]
    async fn unique_path_returns_plain_name_when_available() {
        let temp = tempdir().unwrap();
        let candidate = unique_path_for_filename(temp.path(), "hello.txt")
            .await
            .unwrap();
        assert_eq!(candidate, temp.path().join("hello.txt"));
    }

    #[tokio::test]
    async fn worker_converts_text_file_writes_result_and_archives_source() {
        let temp = tempdir().unwrap();
        let config = test_config(temp.path());
        config.ensure_dirs().unwrap();
        let input_path = config.input_dir.join("note.txt");
        fs::write(&input_path, "hello\r\nworld  \r\n")
            .await
            .unwrap();

        let state = build_state(config);
        let worker = start_worker(state.clone());
        let job = state.enqueue_path(&input_path).unwrap();

        let mut final_job = None;
        for _ in 0..50 {
            let current = state.jobs.get(job.id).unwrap();
            if matches!(current.status, JobStatus::Succeeded | JobStatus::Failed) {
                final_job = Some(current);
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
        worker.abort();

        let finished = final_job.expect("job did not finish in time");
        assert_eq!(finished.status, JobStatus::Succeeded);
        assert_eq!(
            fs::read_to_string(&finished.result_path).await.unwrap(),
            "hello\nworld\n"
        );
        assert!(!fs::try_exists(&input_path).await.unwrap());
        assert!(
            fs::try_exists(finished.archive_path.unwrap())
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn worker_moves_failed_input_out_of_watched_folder() {
        let temp = tempdir().unwrap();
        let config = test_config(temp.path());
        config.ensure_dirs().unwrap();
        let input_path = config.input_dir.join("bad.bin");
        fs::write(&input_path, b"not supported").await.unwrap();

        let state = build_state(config);
        let worker = start_worker(state.clone());
        let job = state.enqueue_path(&input_path).unwrap();

        let mut final_job = None;
        for _ in 0..50 {
            let current = state.jobs.get(job.id).unwrap();
            if matches!(current.status, JobStatus::Succeeded | JobStatus::Failed) {
                final_job = Some(current);
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
        worker.abort();

        let finished = final_job.expect("job did not finish in time");
        assert_eq!(finished.status, JobStatus::Failed);
        assert_eq!(finished.attempts, 1);
        assert!(!fs::try_exists(&input_path).await.unwrap());
        assert!(fs::try_exists(finished.failed_path.unwrap()).await.unwrap());
    }

    fn test_config(root: &Path) -> Config {
        Config {
            input_dir: root.join("input"),
            results_dir: root.join("results"),
            archive_dir: root.join("archive"),
            failed_dir: root.join("failed"),
            bind_addr: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
            qianfan_ocr_base_url: "http://localhost:9361/v1".to_string(),
            qianfan_ocr_model: "baidu/Qianfan-OCR".to_string(),
            qianfan_ocr_timeout_seconds: 600,
            qianfan_ocr_max_tokens: 4096,
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
