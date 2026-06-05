use anyhow::{Context, Result, bail};
use std::{
    env,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Clone, Debug)]
pub struct Config {
    pub input_dir: PathBuf,
    pub results_dir: PathBuf,
    pub archive_dir: PathBuf,
    pub failed_dir: PathBuf,
    pub bind_addr: SocketAddr,
    pub qianfan_ocr_base_url: String,
    pub qianfan_ocr_model: String,
    pub qianfan_ocr_timeout_seconds: usize,
    pub qianfan_ocr_max_tokens: usize,
    pub whisper_model_path: PathBuf,
    pub whisper_cli: String,
    pub whisper_threads: usize,
    pub whisper_processors: usize,
    pub whisper_beam_size: usize,
    pub whisper_best_of: usize,
    pub whisper_no_fallback: bool,
    pub pdf_render_dpi: usize,
    pub pdf_auto_orient: bool,
    pub pdf_auto_orient_cli: String,
    pub pdf_orient_ocr_confirm: bool,
    pub pdf_orient_ocr_cli: String,
    pub pdf_orient_ocr_lang: String,
    pub pdf_orient_ocr_min_confidence: f32,
    pub pdf_orient_ocr_min_score: f32,
    pub url_max_per_text: usize,
    pub yt_dlp_cli: String,
    pub headless_browser_cli: String,
    pub webpage_capture_virtual_time_ms: usize,
    pub video_min_frames: usize,
    pub video_max_frames: usize,
    pub video_scene_threshold: f32,
    pub max_csv_rows: usize,
    pub max_job_attempts: usize,
    pub job_retry_backoff: Duration,
    pub file_stability_checks: usize,
    pub file_stability_delay: Duration,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let bind_addr = env::var("BIND_ADDR")
            .unwrap_or_else(|_| "0.0.0.0:8080".to_string())
            .parse::<SocketAddr>()
            .context("BIND_ADDR must be a socket address such as 0.0.0.0:8080")?;
        let video_min_frames = parse_env_usize("VIDEO_MIN_FRAMES", 3)?;
        let video_max_frames = parse_env_usize("VIDEO_MAX_FRAMES", 24)?;
        if video_min_frames == 0 {
            bail!("VIDEO_MIN_FRAMES must be at least 1");
        }
        if video_max_frames == 0 {
            bail!("VIDEO_MAX_FRAMES must be at least 1");
        }
        if video_max_frames < video_min_frames {
            bail!("VIDEO_MAX_FRAMES must be greater than or equal to VIDEO_MIN_FRAMES");
        }
        let video_scene_threshold = parse_env_f32("VIDEO_SCENE_THRESHOLD", 0.35)?;
        if !(0.0..=1.0).contains(&video_scene_threshold) {
            bail!("VIDEO_SCENE_THRESHOLD must be between 0.0 and 1.0");
        }
        let max_job_attempts = parse_env_usize("MAX_JOB_ATTEMPTS", 1)?;
        if max_job_attempts == 0 {
            bail!("MAX_JOB_ATTEMPTS must be at least 1");
        }

        let pdf_orient_ocr_min_confidence = parse_env_f32("PDF_ORIENT_OCR_MIN_CONFIDENCE", 0.60)?;
        if !(0.0..=1.0).contains(&pdf_orient_ocr_min_confidence) {
            bail!("PDF_ORIENT_OCR_MIN_CONFIDENCE must be between 0.0 and 1.0");
        }
        let pdf_orient_ocr_min_score = parse_env_f32("PDF_ORIENT_OCR_MIN_SCORE", 20.0)?;
        if pdf_orient_ocr_min_score < 0.0 {
            bail!("PDF_ORIENT_OCR_MIN_SCORE must be zero or greater");
        }

        Ok(Self {
            input_dir: env_path("INPUT_DIR", "/data/input"),
            results_dir: env_path("RESULTS_DIR", "/data/results"),
            archive_dir: env_path("ARCHIVE_DIR", "/data/archive"),
            failed_dir: env_path("FAILED_DIR", "/data/failed"),
            bind_addr,
            qianfan_ocr_base_url: env_nonempty("QIANFAN_OCR_BASE_URL")
                .unwrap_or_else(|| "http://host.docker.internal:9361/v1".to_string()),
            qianfan_ocr_model: env_nonempty("QIANFAN_OCR_MODEL")
                .unwrap_or_else(|| "baidu/Qianfan-OCR".to_string()),
            qianfan_ocr_timeout_seconds: parse_env_usize("QIANFAN_OCR_TIMEOUT_SECONDS", 600)?,
            qianfan_ocr_max_tokens: parse_env_usize("QIANFAN_OCR_MAX_TOKENS", 4096)?,
            whisper_model_path: env_path("WHISPER_MODEL_PATH", "/models/whisper/ggml-large-v3.bin"),
            whisper_cli: env::var("WHISPER_CLI").unwrap_or_else(|_| "whisper-cli".to_string()),
            whisper_threads: parse_env_usize("WHISPER_THREADS", 8)?,
            whisper_processors: parse_env_usize("WHISPER_PROCESSORS", 1)?,
            whisper_beam_size: parse_env_usize("WHISPER_BEAM_SIZE", 1)?,
            whisper_best_of: parse_env_usize("WHISPER_BEST_OF", 1)?,
            whisper_no_fallback: parse_env_bool("WHISPER_NO_FALLBACK", true)?,
            pdf_render_dpi: parse_env_usize("PDF_RENDER_DPI", 150)?,
            pdf_auto_orient: parse_env_bool("PDF_AUTO_ORIENT", true)?,
            pdf_auto_orient_cli: env::var("PDF_AUTO_ORIENT_CLI")
                .unwrap_or_else(|_| "pdf-page-auto-orient".to_string()),
            pdf_orient_ocr_confirm: parse_env_bool("PDF_ORIENT_OCR_CONFIRM", true)?,
            pdf_orient_ocr_cli: env::var("PDF_ORIENT_OCR_CLI")
                .unwrap_or_else(|_| "tesseract".to_string()),
            pdf_orient_ocr_lang: env::var("PDF_ORIENT_OCR_LANG")
                .unwrap_or_else(|_| "eng".to_string()),
            pdf_orient_ocr_min_confidence,
            pdf_orient_ocr_min_score,
            url_max_per_text: parse_env_usize("URL_MAX_PER_TEXT", 8)?,
            yt_dlp_cli: env::var("YT_DLP_CLI").unwrap_or_else(|_| "yt-dlp".to_string()),
            headless_browser_cli: env::var("HEADLESS_BROWSER_CLI")
                .unwrap_or_else(|_| "chromium".to_string()),
            webpage_capture_virtual_time_ms: parse_env_usize(
                "WEBPAGE_CAPTURE_VIRTUAL_TIME_MS",
                5_000,
            )?,
            video_min_frames,
            video_max_frames,
            video_scene_threshold,
            max_csv_rows: parse_env_usize("MAX_CSV_ROWS", 1_000)?,
            max_job_attempts,
            job_retry_backoff: Duration::from_secs(
                parse_env_usize("JOB_RETRY_BACKOFF_SECONDS", 30)? as u64,
            ),
            file_stability_checks: parse_env_usize("FILE_STABILITY_CHECKS", 3)?,
            file_stability_delay: Duration::from_millis(parse_env_usize(
                "FILE_STABILITY_DELAY_MS",
                500,
            )? as u64),
        })
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.input_dir)
            .with_context(|| format!("failed to create input dir {}", self.input_dir.display()))?;
        std::fs::create_dir_all(&self.results_dir).with_context(|| {
            format!(
                "failed to create results dir {}",
                self.results_dir.display()
            )
        })?;
        std::fs::create_dir_all(&self.archive_dir).with_context(|| {
            format!(
                "failed to create archive dir {}",
                self.archive_dir.display()
            )
        })?;
        std::fs::create_dir_all(&self.failed_dir).with_context(|| {
            format!("failed to create failed dir {}", self.failed_dir.display())
        })?;
        Ok(())
    }

    pub fn result_path_for(&self, input_path: &Path) -> PathBuf {
        let file_name = input_path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| "result".to_string());
        self.results_dir.join(format!("{file_name}.md"))
    }
}

fn env_path(key: &str, default: &str) -> PathBuf {
    env::var_os(key)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(default))
}

fn env_nonempty(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn parse_env_usize(key: &str, default: usize) -> Result<usize> {
    match env::var(key) {
        Ok(value) => value
            .parse::<usize>()
            .with_context(|| format!("{key} must be a positive integer")),
        Err(_) => Ok(default),
    }
}

fn parse_env_f32(key: &str, default: f32) -> Result<f32> {
    match env::var(key) {
        Ok(value) => value
            .parse::<f32>()
            .with_context(|| format!("{key} must be a decimal number")),
        Err(_) => Ok(default),
    }
}

fn parse_env_bool(key: &str, default: bool) -> Result<bool> {
    match env::var(key) {
        Ok(value) => match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(true),
            "0" | "false" | "no" | "off" => Ok(false),
            _ => bail!("{key} must be a boolean such as true/false or 1/0"),
        },
        Err(_) => Ok(default),
    }
}

#[cfg(test)]
mod tests {
    use super::Config;
    use std::{net::SocketAddr, path::PathBuf, time::Duration};

    fn test_config() -> Config {
        Config {
            input_dir: PathBuf::from("/tmp/input"),
            results_dir: PathBuf::from("/tmp/results"),
            archive_dir: PathBuf::from("/tmp/archive"),
            failed_dir: PathBuf::from("/tmp/failed"),
            bind_addr: "127.0.0.1:8080".parse::<SocketAddr>().unwrap(),
            qianfan_ocr_base_url: "http://localhost:9361/v1".to_string(),
            qianfan_ocr_model: "baidu/Qianfan-OCR".to_string(),
            qianfan_ocr_timeout_seconds: 600,
            qianfan_ocr_max_tokens: 4096,
            whisper_model_path: PathBuf::from("/models/whisper/ggml-large-v3.bin"),
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

    #[test]
    fn result_path_keeps_original_name_and_adds_markdown_extension() {
        let config = test_config();
        assert_eq!(
            config.result_path_for(&PathBuf::from("/tmp/input/report.pdf")),
            PathBuf::from("/tmp/results/report.pdf.md")
        );
    }
}
