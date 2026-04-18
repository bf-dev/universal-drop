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
    pub whisper_model_path: PathBuf,
    pub whisper_cli: String,
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

        Ok(Self {
            input_dir: env_path("INPUT_DIR", "/data/input"),
            results_dir: env_path("RESULTS_DIR", "/data/results"),
            archive_dir: env_path("ARCHIVE_DIR", "/data/archive"),
            bind_addr,
            ollama_base_url: env::var("OLLAMA_BASE_URL")
                .unwrap_or_else(|_| "http://ollama:11434".to_string()),
            ollama_model: env::var("OLLAMA_MODEL").unwrap_or_else(|_| "glm-ocr".to_string()),
            ollama_keep_alive: env::var("OLLAMA_KEEP_ALIVE").unwrap_or_else(|_| "5m".to_string()),
            whisper_model_path: env_path("WHISPER_MODEL_PATH", "/models/whisper/ggml-large-v3.bin"),
            whisper_cli: env::var("WHISPER_CLI").unwrap_or_else(|_| "whisper-cli".to_string()),
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
            ollama_keep_alive: "5m".to_string(),
            whisper_model_path: PathBuf::from("/models/whisper/ggml-large-v3.bin"),
            whisper_cli: "whisper-cli".to_string(),
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
