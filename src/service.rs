use crate::{
    config::Config,
    convert::convert_to_markdown,
    jobs::{Job, JobStore},
};
use anyhow::{Context, Result, bail};
use reqwest::Client;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};
use tokio::{
    fs,
    sync::mpsc::{self, UnboundedReceiver, UnboundedSender},
    task::JoinHandle,
    time::sleep,
};
use tracing::{debug, error, info};
use uuid::Uuid;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub jobs: JobStore,
    pub client: Client,
    queue: UnboundedSender<Uuid>,
}

pub fn build_state(config: Config) -> (AppState, UnboundedReceiver<Uuid>) {
    let (queue, rx) = mpsc::unbounded_channel();
    let state = AppState {
        config: Arc::new(config),
        jobs: JobStore::new(),
        client: Client::new(),
        queue,
    };
    (state, rx)
}

impl AppState {
    pub fn enqueue_path(&self, path: impl Into<PathBuf>) -> Result<Job> {
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
            if let Err(error) = self.queue.send(job.id) {
                self.jobs.mark_failed(job.id, "worker queue is closed");
                bail!("failed to queue {}: {error}", input_path.display());
            }
            info!(job_id = %job.id, path = %input_path.display(), "queued conversion job");
        } else {
            debug!(job_id = %job.id, path = %input_path.display(), "file already has an active job");
        }
        Ok(job)
    }
}

pub fn start_worker(state: AppState, mut rx: UnboundedReceiver<Uuid>) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(job_id) = rx.recv().await {
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
    state.jobs.mark_running(job_id);
    let job = state
        .jobs
        .get(job_id)
        .ok_or_else(|| anyhow::anyhow!("job {job_id} disappeared before processing"))?;
    info!(job_id = %job_id, path = %job.input_path.display(), "starting conversion job");

    let result = async {
        wait_for_file_ready(&job.input_path, &state.config).await?;
        let markdown = convert_to_markdown(&job.input_path, &state.config, &state.client).await?;
        if let Some(parent) = job.result_path.parent() {
            fs::create_dir_all(parent).await.with_context(|| {
                format!("failed to create results directory {}", parent.display())
            })?;
        }
        fs::write(&job.result_path, markdown)
            .await
            .with_context(|| format!("failed to write result {}", job.result_path.display()))?;
        let archive_path = move_to_archive(&job.input_path, &state.config.archive_dir).await?;
        Result::<PathBuf>::Ok(archive_path)
    }
    .await;

    match result {
        Ok(archive_path) => {
            state.jobs.mark_succeeded(job_id, archive_path.clone());
            info!(
                job_id = %job_id,
                result = %job.result_path.display(),
                archive = %archive_path.display(),
                "conversion job succeeded"
            );
            Ok(())
        }
        Err(error) => {
            state.jobs.mark_failed(job_id, format!("{error:#}"));
            Err(error)
        }
    }
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
    fs::create_dir_all(archive_dir)
        .await
        .with_context(|| format!("failed to create archive dir {}", archive_dir.display()))?;
    let file_name = source
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            anyhow::anyhow!("source path has no valid file name: {}", source.display())
        })?;
    let target = unique_path_for_filename(archive_dir, file_name).await?;

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
            fs::remove_file(source).await.with_context(|| {
                format!("failed to remove archived source {}", source.display())
            })?;
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
        build_state, move_to_archive, sanitize_filename, should_ignore_input_path, start_worker,
        unique_path_for_filename,
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

        let (state, rx) = build_state(config);
        let worker = start_worker(state.clone(), rx);
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

    fn test_config(root: &Path) -> Config {
        Config {
            input_dir: root.join("input"),
            results_dir: root.join("results"),
            archive_dir: root.join("archive"),
            bind_addr: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
            ollama_base_url: "http://localhost:11434".to_string(),
            ollama_model: "glm-ocr".to_string(),
            ollama_keep_alive: "5m".to_string(),
            whisper_model_path: root.join("models/ggml-small.bin"),
            whisper_cli: "whisper-cli".to_string(),
            max_csv_rows: 1_000,
            file_stability_checks: 1,
            file_stability_delay: Duration::from_millis(1),
        }
    }
}
