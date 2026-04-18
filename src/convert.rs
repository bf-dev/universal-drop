use crate::config::Config;
use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use mime_guess::MimeGuess;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
};
use tempfile::tempdir;
use tokio::{fs, process::Command};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConversionRoute {
    PlainText,
    Markdown,
    Csv(u8),
    Pdf,
    Audio,
    Video,
    Office,
    Unsupported,
}

pub async fn convert_to_markdown(path: &Path, config: &Config, client: &Client) -> Result<String> {
    let route = detect_route(path);
    let markdown = match route {
        ConversionRoute::PlainText | ConversionRoute::Markdown => read_text_markdown(path).await?,
        ConversionRoute::Csv(delimiter) => {
            csv_to_markdown(path, delimiter, config.max_csv_rows).await?
        }
        ConversionRoute::Pdf => pdf_to_markdown(path, config, client).await?,
        ConversionRoute::Audio => audio_to_markdown(path, config).await?,
        ConversionRoute::Video => video_to_markdown(path, config, client).await?,
        ConversionRoute::Office => office_to_markdown(path).await?,
        ConversionRoute::Unsupported => bail!("unsupported file type for {}", path.display()),
    };
    Ok(normalize_markdown(&markdown))
}

pub fn detect_route(path: &Path) -> ConversionRoute {
    let ext = path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase());

    match ext.as_deref() {
        Some("md" | "markdown" | "mdown") => return ConversionRoute::Markdown,
        Some(
            "txt" | "text" | "log" | "json" | "jsonl" | "xml" | "yaml" | "yml" | "toml" | "html"
            | "htm" | "css" | "js" | "jsx" | "ts" | "tsx" | "rs" | "py" | "go" | "java" | "c"
            | "cc" | "cpp" | "h" | "hpp" | "sql" | "ini" | "conf",
        ) => {
            return ConversionRoute::PlainText;
        }
        Some("csv") => return ConversionRoute::Csv(b','),
        Some("tsv") => return ConversionRoute::Csv(b'\t'),
        Some("pdf") => return ConversionRoute::Pdf,
        Some("mp3" | "wav" | "m4a" | "flac" | "ogg" | "opus" | "aac" | "wma" | "aiff" | "aif") => {
            return ConversionRoute::Audio;
        }
        Some("mp4" | "m4v" | "mov" | "mkv" | "webm" | "avi" | "mpg" | "mpeg" | "3gp" | "flv") => {
            return ConversionRoute::Video;
        }
        Some("doc" | "docx" | "odt" | "rtf" | "ppt" | "pptx" | "odp" | "xls" | "xlsx" | "ods") => {
            return ConversionRoute::Office;
        }
        _ => {}
    }

    let guess = MimeGuess::from_path(path).first();
    match guess
        .as_ref()
        .map(|mime| (mime.type_().as_str(), mime.subtype().as_str()))
    {
        Some(("text", _)) => ConversionRoute::PlainText,
        Some(("application", "pdf")) => ConversionRoute::Pdf,
        Some(("audio", _)) => ConversionRoute::Audio,
        Some(("video", _)) => ConversionRoute::Video,
        _ => ConversionRoute::Unsupported,
    }
}

pub fn normalize_markdown(input: &str) -> String {
    let normalized = input.replace("\r\n", "\n").replace('\r', "\n");
    let mut output = String::new();
    for (index, line) in normalized.lines().enumerate() {
        if index > 0 {
            output.push('\n');
        }
        output.push_str(line.trim_end());
    }
    if !output.ends_with('\n') {
        output.push('\n');
    }
    output
}

pub async fn read_text_markdown(path: &Path) -> Result<String> {
    let bytes = fs::read(path)
        .await
        .with_context(|| format!("failed to read text file {}", path.display()))?;
    Ok(String::from_utf8_lossy(&bytes).to_string())
}

pub async fn csv_to_markdown(path: &Path, delimiter: u8, max_rows: usize) -> Result<String> {
    let bytes = fs::read(path)
        .await
        .with_context(|| format!("failed to read CSV/TSV file {}", path.display()))?;
    csv_bytes_to_markdown(&bytes, delimiter, max_rows)
}

pub fn csv_bytes_to_markdown(bytes: &[u8], delimiter: u8, max_rows: usize) -> Result<String> {
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(delimiter)
        .flexible(true)
        .from_reader(bytes);

    let headers = reader
        .headers()
        .context("failed to read CSV/TSV header row")?
        .clone();
    let width = headers.len().max(1);
    let mut output = String::new();
    output.push('|');
    for index in 0..width {
        output.push(' ');
        output.push_str(&escape_table_cell(headers.get(index).unwrap_or("")));
        output.push_str(" |");
    }
    output.push('\n');
    output.push('|');
    for _ in 0..width {
        output.push_str(" --- |");
    }
    output.push('\n');

    let mut truncated = false;
    for (row_count, record_result) in reader.records().enumerate() {
        let record = record_result.context("failed to parse CSV/TSV row")?;
        if row_count >= max_rows {
            truncated = true;
            break;
        }
        output.push('|');
        for index in 0..width {
            output.push(' ');
            output.push_str(&escape_table_cell(record.get(index).unwrap_or("")));
            output.push_str(" |");
        }
        output.push('\n');
    }

    if truncated {
        output.push_str(&format!(
            "\n_Only the first {max_rows} data rows are shown; the file contains additional rows._\n"
        ));
    }

    Ok(output)
}

fn escape_table_cell(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('|', "\\|")
        .replace('\n', "<br>")
        .trim()
        .to_string()
}

async fn pdf_to_markdown(path: &Path, config: &Config, client: &Client) -> Result<String> {
    let workdir = tempdir().context("failed to create temporary PDF render directory")?;
    let pages = render_pdf_pages(path, workdir.path()).await?;
    if pages.is_empty() {
        bail!("pdftoppm produced no images for {}", path.display());
    }

    let title = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| "document.pdf".into());
    let mut output = format!("# OCR: {title}\n\n");
    for (index, page_path) in pages.iter().enumerate() {
        let page_markdown = ocr_page_with_ollama(page_path, config, client)
            .await
            .with_context(|| format!("Ollama OCR failed for page {}", index + 1))?;
        output.push_str(&format!("## Page {}\n\n", index + 1));
        output.push_str(page_markdown.trim());
        output.push_str("\n\n");
    }
    Ok(output)
}

async fn render_pdf_pages(path: &Path, output_dir: &Path) -> Result<Vec<PathBuf>> {
    let prefix = output_dir.join("page");
    let output = Command::new("pdftoppm")
        .arg("-jpeg")
        .arg("-r")
        .arg("200")
        .arg(path)
        .arg(&prefix)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("failed to execute pdftoppm; ensure poppler-utils is installed")?;
    ensure_success("pdftoppm", &[], &output)?;

    let mut pages = Vec::new();
    let mut entries = fs::read_dir(output_dir)
        .await
        .context("failed to read PDF render output directory")?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let is_jpeg = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| matches!(ext.to_ascii_lowercase().as_str(), "jpg" | "jpeg"))
            .unwrap_or(false);
        if is_jpeg {
            pages.push(path);
        }
    }
    pages.sort();
    Ok(pages)
}

#[derive(Debug, Serialize)]
struct OllamaGenerateRequest<'a> {
    model: &'a str,
    prompt: &'a str,
    images: Vec<String>,
    stream: bool,
    keep_alive: &'a str,
}

#[derive(Debug, Deserialize)]
struct OllamaGenerateResponse {
    response: String,
}

async fn ocr_page_with_ollama(path: &Path, config: &Config, client: &Client) -> Result<String> {
    let prompt = "Read this document page using OCR. Return faithful Markdown only. Preserve tables, headings, lists, reading order, and visible text. Do not add commentary.";
    generate_with_ollama_images(&[path], prompt, config, client).await
}

async fn generate_with_ollama_images(
    paths: &[&Path],
    prompt: &str,
    config: &Config,
    client: &Client,
) -> Result<String> {
    let mut images = Vec::with_capacity(paths.len());
    for path in paths {
        let bytes = fs::read(path)
            .await
            .with_context(|| format!("failed to read image {}", path.display()))?;
        images.push(BASE64.encode(bytes));
    }
    let request = OllamaGenerateRequest {
        model: &config.ollama_model,
        prompt,
        images,
        stream: false,
        keep_alive: &config.ollama_keep_alive,
    };
    let url = format!(
        "{}/api/generate",
        config.ollama_base_url.trim_end_matches('/')
    );
    let response = client
        .post(url)
        .json(&request)
        .send()
        .await
        .context("failed to call Ollama generate API")?;
    let status = response.status();
    let body = response
        .text()
        .await
        .context("failed to read Ollama response body")?;
    if !status.is_success() {
        bail!("Ollama returned HTTP {status}: {body}");
    }
    let parsed: OllamaGenerateResponse = serde_json::from_str(&body)
        .with_context(|| format!("failed to parse Ollama JSON response: {body}"))?;
    Ok(parsed.response)
}

async fn audio_to_markdown(path: &Path, config: &Config) -> Result<String> {
    let transcript = transcribe_audio_source(path, config).await?;
    let title = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| "audio".into());
    Ok(format!("# Transcript: {title}\n\n{}\n", transcript.trim()))
}

async fn transcribe_audio_source(path: &Path, config: &Config) -> Result<String> {
    let workdir = tempdir().context("failed to create temporary audio directory")?;
    let wav_path = workdir.path().join("audio.wav");
    let ffmpeg_args = vec![
        OsString::from("-y"),
        OsString::from("-i"),
        path.as_os_str().to_os_string(),
        OsString::from("-ar"),
        OsString::from("16000"),
        OsString::from("-ac"),
        OsString::from("1"),
        wav_path.as_os_str().to_os_string(),
    ];
    run_command("ffmpeg", &ffmpeg_args)
        .await
        .context("failed to normalize audio with ffmpeg")?;

    let output_prefix = workdir.path().join("transcript");
    let whisper_args = vec![
        OsString::from("-m"),
        config.whisper_model_path.as_os_str().to_os_string(),
        OsString::from("-f"),
        wav_path.as_os_str().to_os_string(),
        OsString::from("-otxt"),
        OsString::from("-of"),
        output_prefix.as_os_str().to_os_string(),
    ];
    run_command(&config.whisper_cli, &whisper_args)
        .await
        .with_context(|| format!("failed to run {} for transcription", config.whisper_cli))?;

    let transcript_path = output_prefix.with_extension("txt");
    let transcript = fs::read_to_string(&transcript_path)
        .await
        .with_context(|| format!("failed to read transcript {}", transcript_path.display()))?;
    Ok(transcript.trim().to_string())
}

#[derive(Debug)]
struct VideoFrame {
    path: PathBuf,
    label: String,
}

async fn video_to_markdown(path: &Path, config: &Config, client: &Client) -> Result<String> {
    let title = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| "video".into());
    let workdir = tempdir().context("failed to create temporary video frame directory")?;
    let frames = select_video_frames(path, workdir.path(), config).await?;
    if frames.is_empty() {
        bail!("ffmpeg produced no video frames for {}", path.display());
    }

    let transcript = transcribe_audio_source(path, config).await;
    let mut output = format!("# Video analysis: {title}\n\n");
    output.push_str("## Selection strategy\n\n");
    output.push_str(&format!(
        "- Visual frames analyzed: {}\n- Scene-change threshold: `{:.2}`\n- Minimum frames: `{}`\n- Maximum frames: `{}`\n\n",
        frames.len(),
        config.video_scene_threshold,
        config.video_min_frames,
        config.video_max_frames
    ));
    output.push_str("_Frames are selected from significant visual changes plus sparse fallback samples. The service does not OCR or describe every frame, which keeps output and memory use bounded._\n\n");

    output.push_str("## Audio transcript\n\n");
    match transcript {
        Ok(text) if !text.trim().is_empty() => {
            output.push_str(text.trim());
            output.push_str("\n\n");
        }
        Ok(_) => output.push_str("_Whisper large-v3 produced an empty transcript._\n\n"),
        Err(error) => output.push_str(&format!(
            "_No usable audio transcript was produced: {error:#}_\n\n"
        )),
    }

    output.push_str("## Visual key frames\n\n");
    for (index, frame) in frames.iter().enumerate() {
        let previous = index
            .checked_sub(1)
            .and_then(|previous_index| frames.get(previous_index));
        let analysis = analyze_video_frame(frame, previous, index, frames.len(), config, client)
            .await
            .with_context(|| format!("Ollama frame analysis failed for frame {}", index + 1))?;
        output.push_str(&format!("### Frame {} — {}\n\n", index + 1, frame.label));
        output.push_str(analysis.trim());
        output.push_str("\n\n");
    }

    Ok(output)
}

async fn select_video_frames(
    path: &Path,
    output_dir: &Path,
    config: &Config,
) -> Result<Vec<VideoFrame>> {
    let duration = probe_video_duration(path).await.unwrap_or(None);
    let mut frames = extract_scene_change_frames(path, output_dir, config).await?;

    if frames.len() < config.video_min_frames && frames.len() < config.video_max_frames {
        let needed =
            (config.video_min_frames - frames.len()).min(config.video_max_frames - frames.len());
        for (index, timestamp) in fallback_frame_timestamps(duration, needed)
            .into_iter()
            .enumerate()
        {
            let output_path = output_dir.join(format!("fallback-{index:06}.jpg"));
            extract_frame_at_timestamp(path, timestamp, &output_path).await?;
            if fs::try_exists(&output_path).await? {
                frames.push(VideoFrame {
                    path: output_path,
                    label: format!("fallback sample at {timestamp:.2}s"),
                });
            }
        }
    }

    frames.truncate(config.video_max_frames);
    Ok(frames)
}

async fn extract_scene_change_frames(
    path: &Path,
    output_dir: &Path,
    config: &Config,
) -> Result<Vec<VideoFrame>> {
    let pattern = output_dir.join("scene-%06d.jpg");
    let filter = format!(
        "select=eq(n\\,0)+gt(scene\\,{:.3}),scale=1280:-2",
        config.video_scene_threshold
    );
    let args = vec![
        OsString::from("-hide_banner"),
        OsString::from("-y"),
        OsString::from("-i"),
        path.as_os_str().to_os_string(),
        OsString::from("-map"),
        OsString::from("0:v:0"),
        OsString::from("-an"),
        OsString::from("-vf"),
        OsString::from(filter),
        OsString::from("-fps_mode"),
        OsString::from("vfr"),
        OsString::from("-frames:v"),
        OsString::from(config.video_max_frames.to_string()),
        OsString::from("-q:v"),
        OsString::from("3"),
        pattern.as_os_str().to_os_string(),
    ];
    run_command("ffmpeg", &args)
        .await
        .context("failed to extract scene-change video frames with ffmpeg")?;

    let paths = collect_jpegs_with_prefix(output_dir, "scene-").await?;
    Ok(paths
        .into_iter()
        .enumerate()
        .map(|(index, path)| VideoFrame {
            path,
            label: format!("scene-change key frame {}", index + 1),
        })
        .collect())
}

async fn extract_frame_at_timestamp(path: &Path, timestamp: f64, output_path: &Path) -> Result<()> {
    let args = vec![
        OsString::from("-hide_banner"),
        OsString::from("-y"),
        OsString::from("-ss"),
        OsString::from(format!("{timestamp:.3}")),
        OsString::from("-i"),
        path.as_os_str().to_os_string(),
        OsString::from("-map"),
        OsString::from("0:v:0"),
        OsString::from("-an"),
        OsString::from("-frames:v"),
        OsString::from("1"),
        OsString::from("-vf"),
        OsString::from("scale=1280:-2"),
        OsString::from("-q:v"),
        OsString::from("3"),
        output_path.as_os_str().to_os_string(),
    ];
    run_command("ffmpeg", &args)
        .await
        .with_context(|| format!("failed to extract fallback video frame at {timestamp:.2}s"))
}

async fn collect_jpegs_with_prefix(dir: &Path, prefix: &str) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    let mut entries = fs::read_dir(dir)
        .await
        .with_context(|| format!("failed to read video frame directory {}", dir.display()))?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let file_name_matches = path
            .file_name()
            .and_then(|value| value.to_str())
            .map(|value| value.starts_with(prefix))
            .unwrap_or(false);
        let is_jpeg = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| matches!(ext.to_ascii_lowercase().as_str(), "jpg" | "jpeg"))
            .unwrap_or(false);
        if file_name_matches && is_jpeg {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

async fn analyze_video_frame(
    frame: &VideoFrame,
    previous: Option<&VideoFrame>,
    index: usize,
    total: usize,
    config: &Config,
    client: &Client,
) -> Result<String> {
    let prompt = if previous.is_some() {
        format!(
            "You are analyzing selected video key frames locally. Image 1 is the previous selected frame; image 2 is frame {} of {}. Compare them and return concise Markdown bullets focused on meaningful visual changes, newly visible OCR text, UI/document state changes, scene changes, people/objects that changed, and information a reader needs. Avoid repeating unchanged details. If the change is minor, say so briefly. Do not hallucinate invisible details.",
            index + 1,
            total
        )
    } else {
        format!(
            "You are analyzing frame 1 of {} selected video key frames locally. Return concise Markdown bullets with visible OCR text, important objects/people/UI/document state, scene context, and why this first selected frame matters. Do not hallucinate invisible details.",
            total
        )
    };
    match previous {
        Some(previous) => {
            generate_with_ollama_images(&[&previous.path, &frame.path], &prompt, config, client)
                .await
        }
        None => generate_with_ollama_images(&[&frame.path], &prompt, config, client).await,
    }
}

async fn probe_video_duration(path: &Path) -> Result<Option<f64>> {
    let args = vec![
        OsString::from("-v"),
        OsString::from("error"),
        OsString::from("-show_entries"),
        OsString::from("format=duration"),
        OsString::from("-of"),
        OsString::from("default=noprint_wrappers=1:nokey=1"),
        path.as_os_str().to_os_string(),
    ];
    let output = run_command_output("ffprobe", &args).await?;
    let trimmed = output.trim();
    if trimmed.is_empty() || trimmed == "N/A" {
        return Ok(None);
    }
    Ok(trimmed.parse::<f64>().ok().filter(|value| *value > 0.0))
}

fn fallback_frame_timestamps(duration_seconds: Option<f64>, count: usize) -> Vec<f64> {
    if count == 0 {
        return Vec::new();
    }
    let Some(duration) = duration_seconds.filter(|value| value.is_finite() && *value > 0.0) else {
        return (0..count).map(|index| index as f64).collect();
    };
    (0..count)
        .map(|index| {
            let timestamp = (index + 1) as f64 * duration / (count + 1) as f64;
            timestamp.min((duration - 0.05).max(0.0))
        })
        .collect()
}

async fn office_to_markdown(path: &Path) -> Result<String> {
    match pandoc_to_markdown(path).await {
        Ok(markdown) => Ok(markdown),
        Err(pandoc_error) => match libreoffice_to_text(path).await {
            Ok(text) => Ok(text),
            Err(libreoffice_error) => Err(anyhow!(
                "office conversion failed; pandoc error: {pandoc_error:#}; libreoffice error: {libreoffice_error:#}"
            )),
        },
    }
}

async fn pandoc_to_markdown(path: &Path) -> Result<String> {
    let workdir = tempdir().context("failed to create temporary pandoc directory")?;
    let output_path = workdir.path().join("output.md");
    let args = vec![
        path.as_os_str().to_os_string(),
        OsString::from("-t"),
        OsString::from("gfm"),
        OsString::from("--wrap=none"),
        OsString::from("-o"),
        output_path.as_os_str().to_os_string(),
    ];
    run_command("pandoc", &args).await?;
    fs::read_to_string(&output_path)
        .await
        .with_context(|| format!("failed to read pandoc output {}", output_path.display()))
}

async fn libreoffice_to_text(path: &Path) -> Result<String> {
    let workdir = tempdir().context("failed to create temporary LibreOffice directory")?;
    let args = vec![
        OsString::from("--headless"),
        OsString::from("--convert-to"),
        OsString::from("txt:Text"),
        OsString::from("--outdir"),
        workdir.path().as_os_str().to_os_string(),
        path.as_os_str().to_os_string(),
    ];
    run_command("libreoffice", &args).await?;

    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| anyhow!("office file has no usable stem: {}", path.display()))?;
    let output_path = workdir.path().join(format!("{stem}.txt"));
    let text = fs::read_to_string(&output_path).await.with_context(|| {
        format!(
            "failed to read LibreOffice output {}",
            output_path.display()
        )
    })?;
    Ok(format!("```text\n{}\n```\n", text.trim()))
}

async fn run_command(program: &str, args: &[OsString]) -> Result<()> {
    let output = Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .with_context(|| format!("failed to execute {program}"))?;
    ensure_success(program, args, &output)
}

async fn run_command_output(program: &str, args: &[OsString]) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .with_context(|| format!("failed to execute {program}"))?;
    ensure_success(program, args, &output)?;
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn ensure_success(program: &str, args: &[OsString], output: &std::process::Output) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    let rendered_args = args
        .iter()
        .map(|arg| arg.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!(
        "{program} {rendered_args} exited with {}; stdout: {}; stderr: {}",
        output.status,
        stdout.trim(),
        stderr.trim()
    )
}

#[cfg(test)]
mod tests {
    use super::{
        ConversionRoute, csv_bytes_to_markdown, detect_route, fallback_frame_timestamps,
        normalize_markdown,
    };
    use std::path::Path;

    #[test]
    fn detects_routes_by_extension() {
        assert_eq!(detect_route(Path::new("paper.pdf")), ConversionRoute::Pdf);
        assert_eq!(
            detect_route(Path::new("notes.md")),
            ConversionRoute::Markdown
        );
        assert_eq!(
            detect_route(Path::new("data.csv")),
            ConversionRoute::Csv(b',')
        );
        assert_eq!(
            detect_route(Path::new("data.tsv")),
            ConversionRoute::Csv(b'\t')
        );
        assert_eq!(detect_route(Path::new("voice.mp3")), ConversionRoute::Audio);
        assert_eq!(
            detect_route(Path::new("screen.mp4")),
            ConversionRoute::Video
        );
        assert_eq!(
            detect_route(Path::new("deck.pptx")),
            ConversionRoute::Office
        );
        assert_eq!(
            detect_route(Path::new("blob.bin")),
            ConversionRoute::Unsupported
        );
    }

    #[test]
    fn normalizes_line_endings_and_trailing_spaces() {
        assert_eq!(normalize_markdown("a  \r\nb\r\n"), "a\nb\n");
    }

    #[test]
    fn converts_csv_to_markdown_and_escapes_cells() {
        let markdown = csv_bytes_to_markdown(b"name,notes\nAda,uses | pipes\n", b',', 10).unwrap();
        assert_eq!(
            markdown,
            "| name | notes |\n| --- | --- |\n| Ada | uses \\| pipes |\n"
        );
    }

    #[test]
    fn csv_conversion_reports_truncation() {
        let markdown = csv_bytes_to_markdown(b"a\n1\n2\n", b',', 1).unwrap();
        assert!(markdown.contains("Only the first 1 data rows"));
    }

    #[test]
    fn fallback_timestamps_are_evenly_spaced_inside_duration() {
        let timestamps = fallback_frame_timestamps(Some(12.0), 3);
        assert_eq!(timestamps, vec![3.0, 6.0, 9.0]);
    }
}
