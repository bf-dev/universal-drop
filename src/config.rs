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
    pub bind_addr: SocketAddr,
    pub ollama_base_url: String,
    pub ollama_model: String,
    pub ollama_keep_alive: String,
    pub ollama_num_thread: usize,
    pub gemini_ocr_enabled: bool,
    pub gemini_api_key: Option<String>,
    pub gemini_api_key_header: String,
    pub gemini_api_endpoint: String,
    pub gemini_deployment_id: String,
    pub gemini_thinking_budget: Option<usize>,
    pub gemini_timeout_seconds: usize,
    pub gemini_min_interval_seconds: usize,
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
            bind_addr,
            ollama_base_url: env::var("OLLAMA_BASE_URL")
                .unwrap_or_else(|_| "http://ollama:11434".to_string()),
            ollama_model: env::var("OLLAMA_MODEL").unwrap_or_else(|_| "glm-ocr".to_string()),
            ollama_keep_alive: env::var("OLLAMA_KEEP_ALIVE").unwrap_or_else(|_| "30m".to_string()),
            ollama_num_thread: parse_env_usize("OLLAMA_NUM_THREAD", 8)?,
            gemini_ocr_enabled: parse_env_bool("GEMINI_OCR_ENABLED", true)?,
            gemini_api_key: env_nonempty("GEMINI_API_KEY")
                .or_else(|| env_nonempty("HKU_GEMINI_API_KEY")),
            gemini_api_key_header: env_nonempty("GEMINI_API_KEY_HEADER")
                .unwrap_or_else(|| "api-key".to_string()),
            gemini_api_endpoint: env_nonempty("GEMINI_API_ENDPOINT")
                .or_else(|| env_nonempty("GEMINI_ENDPOINT"))
                .unwrap_or_else(|| {
                    "https://api.hku.hk/gemini/student/{deployment-id}:generateContent".to_string()
                }),
            gemini_deployment_id: env_nonempty("GEMINI_DEPLOYMENT_ID")
                .unwrap_or_else(|| "gemini-3-flash-preview".to_string()),
            gemini_thinking_budget: parse_optional_env_usize("GEMINI_THINKING_BUDGET")?,
            gemini_timeout_seconds: parse_env_usize("GEMINI_TIMEOUT_SECONDS", 45)?,
            gemini_min_interval_seconds: parse_env_usize("GEMINI_MIN_INTERVAL_SECONDS", 21)?,
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

fn parse_optional_env_usize(key: &str) -> Result<Option<usize>> {
    match env_nonempty(key) {
        Some(value) => value
            .parse::<usize>()
            .map(Some)
            .with_context(|| format!("{key} must be a positive integer")),
        None => Ok(None),
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
            bind_addr: "127.0.0.1:8080".parse::<SocketAddr>().unwrap(),
            ollama_base_url: "http://localhost:11434".to_string(),
            ollama_model: "glm-ocr".to_string(),
            ollama_keep_alive: "30m".to_string(),
            ollama_num_thread: 8,
            gemini_ocr_enabled: true,
            gemini_api_key: None,
            gemini_api_key_header: "api-key".to_string(),
            gemini_api_endpoint:
                "https://api.hku.hk/gemini/student/{deployment-id}:generateContent".to_string(),
            gemini_deployment_id: "gemini-3-flash-preview".to_string(),
            gemini_thinking_budget: None,
            gemini_timeout_seconds: 45,
            gemini_min_interval_seconds: 21,
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
