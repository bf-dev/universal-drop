use crate::config::Config;
use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use mime_guess::MimeGuess;
use reqwest::{
    Client,
    header::{HeaderName, HeaderValue},
};
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsString,
    io,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tempfile::tempdir;
use tokio::{fs, process::Command};
use tracing::{debug, info, warn};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConversionRoute {
    PlainText,
    Markdown,
    Csv(u8),
    Image,
    Pdf,
    Audio,
    Video,
    Office,
    Unsupported,
}

pub async fn convert_to_markdown(path: &Path, config: &Config, client: &Client) -> Result<String> {
    let route = detect_route(path);
    let markdown = match route {
        ConversionRoute::PlainText if should_autodetect_urls(path) => {
            plain_text_to_markdown(path, config, client).await?
        }
        ConversionRoute::PlainText | ConversionRoute::Markdown => read_text_markdown(path).await?,
        ConversionRoute::Csv(delimiter) => {
            csv_to_markdown(path, delimiter, config.max_csv_rows).await?
        }
        ConversionRoute::Image => image_to_markdown(path, config, client).await?,
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
        Some(
            "jpg" | "jpeg" | "png" | "webp" | "gif" | "bmp" | "tif" | "tiff" | "heic" | "heif",
        ) => return ConversionRoute::Image,
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
        Some(("image", _)) => ConversionRoute::Image,
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

pub fn strip_response_markdown_fence(input: &str) -> String {
    let mut current = input.trim().to_string();
    loop {
        let lines = current.lines().collect::<Vec<_>>();
        if lines.len() < 2 {
            return current;
        }
        let (Some(first), Some(last)) = (lines.first(), lines.last()) else {
            return current;
        };
        if !is_response_fence_start(first) || !is_fence_end(last) {
            return current;
        }
        let next = lines[1..lines.len() - 1].join("\n").trim().to_string();
        if next == current {
            return current;
        }
        current = next;
    }
}

fn is_response_fence_start(line: &str) -> bool {
    let trimmed = line.trim();
    let Some(label) = trimmed.strip_prefix("```") else {
        return false;
    };
    matches!(
        label.trim().to_ascii_lowercase().as_str(),
        "" | "md" | "markdown" | "mdown" | "text" | "txt" | "plain" | "plaintext"
    )
}

fn is_fence_end(line: &str) -> bool {
    line.trim() == "```"
}

pub async fn read_text_markdown(path: &Path) -> Result<String> {
    let bytes = fs::read(path)
        .await
        .with_context(|| format!("failed to read text file {}", path.display()))?;
    Ok(String::from_utf8_lossy(&bytes).to_string())
}

fn should_autodetect_urls(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .map(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "txt" | "text" | "url" | "urls"
            )
        })
        .unwrap_or(false)
}

async fn plain_text_to_markdown(path: &Path, config: &Config, client: &Client) -> Result<String> {
    let text = read_text_markdown(path).await?;
    let urls = extract_urls(&text);
    if urls.is_empty() || config.url_max_per_text == 0 {
        return Ok(text);
    }

    let mut output = String::new();
    output.push_str("# Text drop\n\n");
    if !text.trim().is_empty() {
        output.push_str("## Original text\n\n");
        output.push_str(text.trim());
        output.push_str("\n\n");
    }

    let total_urls = urls.len();
    let selected_urls = urls.into_iter().take(config.url_max_per_text);
    for (index, url) in selected_urls.enumerate() {
        output.push_str(&format!("## Detected URL {}\n\n", index + 1));
        match url_to_markdown(&url, config, client).await {
            Ok(markdown) => {
                output.push_str(markdown.trim());
                output.push_str("\n\n");
            }
            Err(error) => {
                output.push_str(&format!(
                    "_URL conversion failed; source URL omitted from output: {error:#}_\n\n"
                ));
            }
        }
    }

    if total_urls > config.url_max_per_text {
        output.push_str(&format!(
            "_Skipped {} additional detected URLs because URL_MAX_PER_TEXT is {}._\n\n",
            total_urls - config.url_max_per_text,
            config.url_max_per_text
        ));
    }

    Ok(output)
}

fn extract_urls(text: &str) -> Vec<String> {
    let mut urls = Vec::new();
    for token in text.split_whitespace() {
        let Some(url) = normalize_url_token(token) else {
            continue;
        };
        if !urls.iter().any(|existing| existing == &url) {
            urls.push(url);
        }
    }
    urls
}

fn normalize_url_token(token: &str) -> Option<String> {
    let trimmed = token
        .trim_matches(|ch| matches!(ch, '<' | '>' | '"' | '\'' | '(' | '[' | '{'))
        .trim_end_matches(|ch| matches!(ch, '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}'));
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        Some(trimmed.to_string())
    } else {
        None
    }
}

async fn url_to_markdown(url: &str, config: &Config, client: &Client) -> Result<String> {
    if let Some(markdown) = try_yt_dlp_to_markdown(url, config, client).await? {
        return Ok(markdown);
    }

    webpage_to_markdown(url, config, client).await
}

async fn try_yt_dlp_to_markdown(
    url: &str,
    config: &Config,
    client: &Client,
) -> Result<Option<String>> {
    let workdir = tempdir().context("failed to create temporary URL download directory")?;
    let output_template = workdir.path().join("download.%(ext)s");
    let args = vec![
        OsString::from("--no-progress"),
        OsString::from("--no-warnings"),
        OsString::from("--no-playlist"),
        OsString::from("--merge-output-format"),
        OsString::from("mp4"),
        OsString::from("-o"),
        output_template.as_os_str().to_os_string(),
        OsString::from(url),
    ];

    match run_private_command_status(&config.yt_dlp_cli, &args, "yt-dlp media download").await? {
        PrivateCommandStatus::Success => {}
        PrivateCommandStatus::NotFound => {
            warn!(
                program = %config.yt_dlp_cli,
                "yt-dlp executable is unavailable; falling back to webpage capture"
            );
            return Ok(None);
        }
        PrivateCommandStatus::Failed => return Ok(None),
    }

    let downloaded_files = collect_downloaded_files(workdir.path()).await?;
    for downloaded_file in downloaded_files {
        match detect_route(&downloaded_file) {
            ConversionRoute::Image => {
                return image_to_markdown(&downloaded_file, config, client)
                    .await
                    .map(Some);
            }
            ConversionRoute::Video => {
                return video_to_markdown(&downloaded_file, config, client)
                    .await
                    .map(Some);
            }
            ConversionRoute::Audio => {
                return audio_to_markdown(&downloaded_file, config).await.map(Some);
            }
            ConversionRoute::Pdf => {
                return pdf_to_markdown(&downloaded_file, config, client)
                    .await
                    .map(Some);
            }
            ConversionRoute::PlainText | ConversionRoute::Markdown => {
                return read_text_markdown(&downloaded_file).await.map(Some);
            }
            ConversionRoute::Csv(_) | ConversionRoute::Office | ConversionRoute::Unsupported => {}
        }
    }

    Ok(None)
}

async fn collect_downloaded_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files: Vec<(u64, PathBuf)> = Vec::new();
    let mut entries = fs::read_dir(dir)
        .await
        .with_context(|| format!("failed to read URL download directory {}", dir.display()))?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let metadata = entry.metadata().await?;
        if !metadata.is_file() || is_partial_or_sidecar_download(&path) {
            continue;
        }
        files.push((metadata.len(), path));
    }
    files.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    Ok(files.into_iter().map(|(_, path)| path).collect())
}

fn is_partial_or_sidecar_download(path: &Path) -> bool {
    let Some(file_name) = path.file_name().and_then(|value| value.to_str()) else {
        return true;
    };
    if file_name.starts_with('.') {
        return true;
    }
    let ext = path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase());
    matches!(
        ext.as_deref(),
        Some("part" | "ytdl" | "tmp" | "json" | "description" | "vtt" | "srt")
    )
}

async fn webpage_to_markdown(url: &str, config: &Config, client: &Client) -> Result<String> {
    let workdir = tempdir().context("failed to create temporary webpage capture directory")?;
    let pdf_path = workdir.path().join("webpage.pdf");
    capture_webpage_to_pdf(url, &pdf_path, config).await?;
    if !fs::try_exists(&pdf_path).await? {
        bail!("headless browser did not create a webpage capture PDF");
    }
    pdf_to_markdown(&pdf_path, config, client).await
}

async fn capture_webpage_to_pdf(url: &str, output_path: &Path, config: &Config) -> Result<()> {
    let args = vec![
        OsString::from("--headless=new"),
        OsString::from("--disable-gpu"),
        OsString::from("--disable-dev-shm-usage"),
        OsString::from("--disable-extensions"),
        OsString::from("--no-first-run"),
        OsString::from("--no-default-browser-check"),
        OsString::from("--no-sandbox"),
        OsString::from("--hide-scrollbars"),
        OsString::from("--run-all-compositor-stages-before-draw"),
        OsString::from(format!(
            "--virtual-time-budget={}",
            config.webpage_capture_virtual_time_ms
        )),
        OsString::from("--no-pdf-header-footer"),
        OsString::from(format!("--print-to-pdf={}", output_path.display())),
        OsString::from(url),
    ];

    for candidate in headless_browser_candidates(&config.headless_browser_cli) {
        match run_private_command_status(&candidate, &args, "headless webpage capture").await? {
            PrivateCommandStatus::Success => return Ok(()),
            PrivateCommandStatus::NotFound => continue,
            PrivateCommandStatus::Failed => {
                bail!("headless browser failed to capture webpage; source URL omitted")
            }
        }
    }

    bail!(
        "headless browser executable not found; set HEADLESS_BROWSER_CLI to chromium or chromium-browser"
    )
}

fn headless_browser_candidates(configured: &str) -> Vec<String> {
    let configured = configured.trim();
    let mut candidates = Vec::new();
    if !configured.is_empty() {
        candidates.push(configured.to_string());
    }
    for fallback in [
        "chromium",
        "chromium-browser",
        "google-chrome",
        "google-chrome-stable",
    ] {
        if !candidates.iter().any(|candidate| candidate == fallback) {
            candidates.push(fallback.to_string());
        }
    }
    candidates
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

async fn image_to_markdown(path: &Path, config: &Config, client: &Client) -> Result<String> {
    let title = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| "image".into());
    let mut gemini_disabled_for_job = false;
    let mut last_gemini_attempt = None;
    let page_markdown = ocr_document_image_to_markdown(
        path,
        config,
        client,
        &mut gemini_disabled_for_job,
        &mut last_gemini_attempt,
    )
    .await
    .with_context(|| format!("OCR failed for image {}", path.display()))?;
    Ok(format!("# OCR: {title}\n\n{}\n", page_markdown.trim()))
}

async fn pdf_to_markdown(path: &Path, config: &Config, client: &Client) -> Result<String> {
    let workdir = tempdir().context("failed to create temporary PDF render directory")?;
    let pages = render_pdf_pages(path, workdir.path(), config.pdf_render_dpi).await?;
    if pages.is_empty() {
        bail!("pdftoppm produced no images for {}", path.display());
    }
    auto_orient_pdf_pages(&pages, config).await?;

    let title = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| "document.pdf".into());
    let mut output = format!("# OCR: {title}\n\n");
    let mut gemini_disabled_for_job = false;
    let mut last_gemini_attempt = None;
    let mut succeeded_pages = 0usize;
    let mut failed_pages = 0usize;
    for (index, page_path) in pages.iter().enumerate() {
        output.push_str(&format!("## Page {}\n\n", index + 1));
        match ocr_document_image_to_markdown(
            page_path,
            config,
            client,
            &mut gemini_disabled_for_job,
            &mut last_gemini_attempt,
        )
        .await
        {
            Ok(page_markdown) => {
                succeeded_pages += 1;
                output.push_str(page_markdown.trim());
            }
            Err(error) => {
                failed_pages += 1;
                warn!(
                    path = %path.display(),
                    page = index + 1,
                    error = %error,
                    "PDF page OCR failed; keeping partial output"
                );
                output.push_str(&format!("_OCR failed for this page: {error:#}_"));
            }
        }
        output.push_str("\n\n");
    }
    if succeeded_pages == 0 {
        bail!("OCR failed for every page in {}", path.display());
    }
    if failed_pages > 0 {
        output.push_str("## Conversion warnings\n\n");
        output.push_str(&format!(
            "- OCR failed for {failed_pages} of {} rendered pages; the rest of the document was preserved.\n\n",
            pages.len()
        ));
    }
    Ok(output)
}

async fn render_pdf_pages(path: &Path, output_dir: &Path, dpi: usize) -> Result<Vec<PathBuf>> {
    let prefix = output_dir.join("page");
    let output = Command::new("pdftoppm")
        .arg("-jpeg")
        .arg("-r")
        .arg(dpi.to_string())
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

#[derive(Debug, Deserialize)]
struct PageOrientationResult {
    rotation: i32,
    confidence: Option<f64>,
    reason: Option<String>,
    line_model_best_rotation: Option<i32>,
    line_model_score_margin: Option<f64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OrientationOcrChoice {
    Original,
    Candidate,
    Tie,
}

#[derive(Debug)]
struct OrientationOcrConfirmation {
    choice: OrientationOcrChoice,
    confidence: f32,
    original_score: f64,
    candidate_score: f64,
}

async fn auto_orient_pdf_pages(pages: &[PathBuf], config: &Config) -> Result<()> {
    if !config.pdf_auto_orient {
        return Ok(());
    }

    for (index, page_path) in pages.iter().enumerate() {
        auto_orient_pdf_page(page_path, index + 1, config).await?;
    }
    Ok(())
}

async fn auto_orient_pdf_page(page_path: &Path, page_number: usize, config: &Config) -> Result<()> {
    let stem = page_path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("page");
    let oriented_path = page_path.with_file_name(format!("{stem}.oriented.jpg"));
    let args = vec![
        page_path.as_os_str().to_os_string(),
        oriented_path.as_os_str().to_os_string(),
    ];

    let stdout = match run_command_output(&config.pdf_auto_orient_cli, &args).await {
        Ok(stdout) => stdout,
        Err(error) => {
            warn!(
                page = page_number,
                path = %page_path.display(),
                error = %error,
                "PDF page auto-orientation failed; keeping original render"
            );
            let _ = fs::remove_file(&oriented_path).await;
            return Ok(());
        }
    };

    let result = match serde_json::from_str::<PageOrientationResult>(stdout.trim()) {
        Ok(result) => result,
        Err(error) => {
            warn!(
                page = page_number,
                path = %page_path.display(),
                stdout = %stdout.trim(),
                error = %error,
                "PDF page auto-orientation returned invalid metadata; keeping original render"
            );
            let _ = fs::remove_file(&oriented_path).await;
            return Ok(());
        }
    };

    if result.rotation == 0 {
        debug!(
            page = page_number,
            path = %page_path.display(),
            confidence = result.confidence.unwrap_or_default(),
            reason = result.reason.as_deref().unwrap_or("unknown"),
            "PDF page auto-orientation kept original orientation"
        );
        let _ = fs::remove_file(&oriented_path).await;
        return Ok(());
    }

    if config.pdf_orient_ocr_confirm {
        match confirm_pdf_orientation_with_ocr(page_path, &oriented_path, &result, config).await {
            Ok(confirmation) if confirmation.choice == OrientationOcrChoice::Candidate => {
                debug!(
                    page = page_number,
                    rotation = result.rotation,
                    confidence = confirmation.confidence,
                    original_score = confirmation.original_score,
                    candidate_score = confirmation.candidate_score,
                    "PDF page orientation accepted by recognition-driven OCR confirmation"
                );
            }
            Ok(confirmation) => {
                info!(
                    page = page_number,
                    rotation = result.rotation,
                    confidence = confirmation.confidence,
                    original_score = confirmation.original_score,
                    candidate_score = confirmation.candidate_score,
                    reason = result.reason.as_deref().unwrap_or("unknown"),
                    "PDF page auto-orientation rejected by recognition-driven OCR confirmation"
                );
                let _ = fs::remove_file(&oriented_path).await;
                return Ok(());
            }
            Err(error) => {
                warn!(
                    page = page_number,
                    rotation = result.rotation,
                    error = %error,
                    reason = result.reason.as_deref().unwrap_or("unknown"),
                    "PDF page OCR orientation confirmation failed; keeping original render"
                );
                let _ = fs::remove_file(&oriented_path).await;
                return Ok(());
            }
        }
    }

    fs::rename(&oriented_path, page_path)
        .await
        .with_context(|| {
            format!(
                "failed to replace auto-oriented page {}",
                page_path.display()
            )
        })?;
    info!(
        page = page_number,
        rotation = result.rotation,
        confidence = result.confidence.unwrap_or_default(),
        reason = result.reason.as_deref().unwrap_or("unknown"),
        path = %page_path.display(),
        "auto-rotated PDF page before OCR"
    );
    Ok(())
}

async fn confirm_pdf_orientation_with_ocr(
    original_path: &Path,
    candidate_path: &Path,
    orientation: &PageOrientationResult,
    config: &Config,
) -> Result<OrientationOcrConfirmation> {
    let original_text = tesseract_orientation_probe(original_path, config)
        .await
        .with_context(|| {
            format!(
                "failed to OCR original page orientation probe {}",
                original_path.display()
            )
        })?;
    let candidate_text = tesseract_orientation_probe(candidate_path, config)
        .await
        .with_context(|| {
            format!(
                "failed to OCR candidate page orientation probe {}",
                candidate_path.display()
            )
        })?;

    let original_score = orientation_ocr_text_score(&original_text);
    let candidate_score = orientation_ocr_text_score(&candidate_text);
    let max_score = original_score.max(candidate_score).max(1.0);
    let confidence = ((candidate_score - original_score) / max_score).max(0.0) as f32;
    let enough_signal = candidate_score >= config.pdf_orient_ocr_min_score as f64;
    let choice = if enough_signal && confidence >= config.pdf_orient_ocr_min_confidence {
        OrientationOcrChoice::Candidate
    } else if original_score > candidate_score {
        OrientationOcrChoice::Original
    } else {
        OrientationOcrChoice::Tie
    };

    debug!(
        rotation = orientation.rotation,
        helper_confidence = orientation.confidence.unwrap_or_default(),
        helper_reason = orientation.reason.as_deref().unwrap_or("unknown"),
        line_model_best_rotation = orientation.line_model_best_rotation.unwrap_or_default(),
        line_model_score_margin = orientation.line_model_score_margin.unwrap_or_default(),
        original_score,
        candidate_score,
        confidence,
        "PDF orientation OCR confirmation scored candidate"
    );

    Ok(OrientationOcrConfirmation {
        choice,
        confidence,
        original_score,
        candidate_score,
    })
}

async fn tesseract_orientation_probe(path: &Path, config: &Config) -> Result<String> {
    let args = vec![
        path.as_os_str().to_os_string(),
        OsString::from("stdout"),
        OsString::from("-l"),
        OsString::from(&config.pdf_orient_ocr_lang),
        OsString::from("--psm"),
        OsString::from("6"),
        OsString::from("tsv"),
    ];
    let output = Command::new(&config.pdf_orient_ocr_cli)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .with_context(|| format!("failed to execute {}", config.pdf_orient_ocr_cli))?;
    ensure_success(&config.pdf_orient_ocr_cli, &args, &output)?;
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn orientation_ocr_text_score(text: &str) -> f64 {
    if text
        .lines()
        .next()
        .map(|line| line.starts_with("level\t"))
        .unwrap_or(false)
    {
        return orientation_ocr_tsv_score(text);
    }

    let mut non_ws = 0usize;
    let mut signal_chars = 0usize;
    for ch in text.chars() {
        if ch.is_whitespace() {
            continue;
        }
        non_ws += 1;
        if is_ocr_signal_char(ch) {
            signal_chars += 1;
        }
    }
    if non_ws == 0 || signal_chars == 0 {
        return 0.0;
    }

    let good_tokens = text
        .split_whitespace()
        .filter(|token| {
            let total = token.chars().filter(|ch| !ch.is_whitespace()).count();
            if total == 0 {
                return false;
            }
            let signal = token.chars().filter(|ch| is_ocr_signal_char(*ch)).count();
            signal >= 2 && (signal as f64 / total as f64) >= 0.50
        })
        .count();

    let signal_ratio = signal_chars as f64 / non_ws as f64;
    signal_chars as f64 * signal_ratio + good_tokens as f64 * 4.0
}

fn orientation_ocr_tsv_score(tsv: &str) -> f64 {
    let mut lines = tsv.lines();
    let Some(header) = lines.next() else {
        return 0.0;
    };
    let columns = header.split('\t').collect::<Vec<_>>();
    let conf_index = columns.iter().position(|column| *column == "conf");
    let text_index = columns.iter().position(|column| *column == "text");
    let (Some(conf_index), Some(text_index)) = (conf_index, text_index) else {
        return 0.0;
    };

    let mut good_tokens = 0usize;
    let mut high_confidence_tokens = 0usize;
    let mut confidence_sum = 0.0f64;
    let mut weighted_signal = 0.0f64;

    for line in lines {
        let fields = line.split('\t').collect::<Vec<_>>();
        if fields.len() <= conf_index || fields.len() <= text_index {
            continue;
        }
        let text = fields[text_index].trim();
        if text.is_empty() {
            continue;
        }
        let Ok(confidence) = fields[conf_index].parse::<f64>() else {
            continue;
        };
        if confidence < 0.0 {
            continue;
        }

        let total_chars = text.chars().filter(|ch| !ch.is_whitespace()).count();
        if total_chars == 0 {
            continue;
        }
        let signal_chars = text.chars().filter(|ch| is_ocr_signal_char(*ch)).count();
        if signal_chars < 2 || (signal_chars as f64 / total_chars as f64) < 0.50 {
            continue;
        }

        good_tokens += 1;
        confidence_sum += confidence;
        weighted_signal += (confidence / 100.0).max(0.0) * signal_chars as f64;
        if confidence >= 55.0 {
            high_confidence_tokens += 1;
        }
    }

    if good_tokens == 0 {
        return 0.0;
    }

    let average_confidence = confidence_sum / good_tokens as f64;
    weighted_signal + high_confidence_tokens as f64 * 5.0 + average_confidence * 0.5
}

fn is_ocr_signal_char(ch: char) -> bool {
    ch.is_alphanumeric()
        || ('\u{ac00}'..='\u{d7af}').contains(&ch)
        || ('\u{4e00}'..='\u{9fff}').contains(&ch)
        || ('\u{3040}'..='\u{30ff}').contains(&ch)
}

const OCR_SYSTEM_PROMPT: &str = "\
You are a precise OCR-to-Markdown conversion engine. Convert only visible image content into clean Markdown. Preserve reading order, headings, paragraphs, lists, tables, form labels, checkboxes, code blocks, page numbers, captions, and the original language/script. Do not summarize, explain, infer hidden content, or mention OCR. Use [illegible] only for text that is visible but unreadable.";

const OCR_USER_PROMPT: &str = "\
Convert this page image to faithful Markdown.

Requirements:
- Output Markdown only.
- Preserve visible text exactly, including original language, punctuation, casing, and numbers.
- Preserve tables as GitHub-flavored Markdown tables when practical; otherwise use line-preserving Markdown.
- Preserve headings, lists, checkboxes, form labels, signatures, stamps, captions, and footnotes.
- Keep natural reading order for the page layout.
- Do not add commentary, confidence notes, source URLs, or invented content.
- Do not wrap the response in a Markdown code fence such as ```text, ```md, or ```markdown.";

#[derive(Debug, Serialize)]
struct GeminiGenerateRequest<'a> {
    system_instruction: GeminiSystemInstruction<'a>,
    contents: Vec<GeminiContent<'a>>,
    #[serde(rename = "generationConfig")]
    generation_config: GeminiGenerationConfig,
}

#[derive(Debug, Serialize)]
struct GeminiSystemInstruction<'a> {
    parts: Vec<GeminiTextPart<'a>>,
}

#[derive(Debug, Serialize)]
struct GeminiTextPart<'a> {
    text: &'a str,
}

#[derive(Debug, Serialize)]
struct GeminiContent<'a> {
    role: &'a str,
    parts: Vec<GeminiRequestPart<'a>>,
}

#[derive(Debug, Serialize)]
struct GeminiRequestPart<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<&'a str>,
    #[serde(rename = "inline_data", skip_serializing_if = "Option::is_none")]
    inline_data: Option<GeminiInlineData<'a>>,
}

#[derive(Debug, Serialize)]
struct GeminiInlineData<'a> {
    #[serde(rename = "mime_type")]
    mime_type: &'a str,
    data: &'a str,
}

#[derive(Debug, Serialize)]
struct GeminiGenerationConfig {
    temperature: f32,
    #[serde(rename = "topP")]
    top_p: f32,
    #[serde(rename = "topK")]
    top_k: usize,
    #[serde(rename = "thinkingConfig", skip_serializing_if = "Option::is_none")]
    thinking_config: Option<GeminiThinkingConfig>,
}

#[derive(Debug, Serialize)]
struct GeminiThinkingConfig {
    #[serde(rename = "thinkingBudget")]
    thinking_budget: usize,
}

#[derive(Debug, Deserialize)]
struct GeminiGenerateResponse {
    candidates: Vec<GeminiCandidate>,
}

#[derive(Debug, Deserialize)]
struct GeminiCandidate {
    content: Option<GeminiResponseContent>,
    #[serde(rename = "finishReason")]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GeminiResponseContent {
    parts: Vec<GeminiResponsePart>,
}

#[derive(Debug, Deserialize)]
struct GeminiResponsePart {
    text: Option<String>,
}

#[derive(Debug, Serialize)]
struct OllamaGenerateRequest<'a> {
    model: &'a str,
    prompt: &'a str,
    images: Vec<String>,
    stream: bool,
    keep_alive: &'a str,
    options: OllamaGenerateOptions,
}

#[derive(Debug, Serialize)]
struct OllamaGenerateOptions {
    num_thread: usize,
    temperature: f32,
}

#[derive(Debug, Deserialize)]
struct OllamaGenerateResponse {
    response: String,
}

async fn ocr_document_image_to_markdown(
    path: &Path,
    config: &Config,
    client: &Client,
    gemini_disabled_for_job: &mut bool,
    last_gemini_attempt: &mut Option<tokio::time::Instant>,
) -> Result<String> {
    if !*gemini_disabled_for_job && gemini_is_configured(config) {
        wait_for_gemini_rate_limit(config, last_gemini_attempt).await;
        match ocr_document_image_with_gemini(path, config, client).await {
            Ok(markdown) => return Ok(strip_response_markdown_fence(&markdown)),
            Err(error) => {
                *gemini_disabled_for_job = true;
                warn!(
                    path = %path.display(),
                    error = %error,
                    "Gemini OCR failed; falling back to local GLM/Ollama OCR for the rest of this job"
                );
            }
        }
    }

    ocr_document_image_with_local_glm(path, config, client)
        .await
        .map(|markdown| strip_response_markdown_fence(&markdown))
}

fn gemini_is_configured(config: &Config) -> bool {
    config.gemini_ocr_enabled
        && config
            .gemini_api_key
            .as_deref()
            .is_some_and(|key| !key.trim().is_empty())
        && !config.gemini_api_endpoint.trim().is_empty()
}

async fn wait_for_gemini_rate_limit(
    config: &Config,
    last_gemini_attempt: &mut Option<tokio::time::Instant>,
) {
    let interval = Duration::from_secs(config.gemini_min_interval_seconds as u64);
    if !interval.is_zero() {
        if let Some(last_attempt) = *last_gemini_attempt {
            let elapsed = last_attempt.elapsed();
            if elapsed < interval {
                tokio::time::sleep(interval - elapsed).await;
            }
        }
    }
    *last_gemini_attempt = Some(tokio::time::Instant::now());
}

async fn ocr_document_image_with_gemini(
    path: &Path,
    config: &Config,
    client: &Client,
) -> Result<String> {
    let api_key = config
        .gemini_api_key
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .context("GEMINI_API_KEY is not configured")?;
    let image_bytes = fs::read(path)
        .await
        .with_context(|| format!("failed to read image {}", path.display()))?;
    let image_base64 = BASE64.encode(image_bytes);
    let mime_type = image_mime_type(path);
    let thinking_config = config
        .gemini_thinking_budget
        .map(|thinking_budget| GeminiThinkingConfig { thinking_budget });
    let request = GeminiGenerateRequest {
        system_instruction: GeminiSystemInstruction {
            parts: vec![GeminiTextPart {
                text: OCR_SYSTEM_PROMPT,
            }],
        },
        contents: vec![GeminiContent {
            role: "user",
            parts: vec![
                GeminiRequestPart {
                    text: Some(OCR_USER_PROMPT),
                    inline_data: None,
                },
                GeminiRequestPart {
                    text: None,
                    inline_data: Some(GeminiInlineData {
                        mime_type: &mime_type,
                        data: &image_base64,
                    }),
                },
            ],
        }],
        generation_config: GeminiGenerationConfig {
            temperature: 0.0,
            top_p: 0.1,
            top_k: 1,
            thinking_config,
        },
    };

    let header_name = HeaderName::from_bytes(config.gemini_api_key_header.as_bytes())
        .with_context(|| {
            format!(
                "invalid GEMINI_API_KEY_HEADER {}",
                config.gemini_api_key_header
            )
        })?;
    let header_value = HeaderValue::from_str(api_key)
        .context("GEMINI_API_KEY contains an invalid header value")?;
    let response = client
        .post(gemini_generate_content_url(config))
        .header(header_name, header_value)
        .timeout(std::time::Duration::from_secs(
            config.gemini_timeout_seconds as u64,
        ))
        .json(&request)
        .send()
        .await
        .context("failed to call Gemini generateContent API")?;
    let status = response.status();
    let body = response
        .text()
        .await
        .context("failed to read Gemini response body")?;
    if !status.is_success() {
        bail!(
            "Gemini returned HTTP {status}: {}",
            response_body_snippet(&body)
        );
    }
    let parsed: GeminiGenerateResponse = serde_json::from_str(&body).with_context(|| {
        format!(
            "failed to parse Gemini JSON response: {}",
            response_body_snippet(&body)
        )
    })?;
    extract_gemini_text(parsed)
}

fn image_mime_type(path: &Path) -> String {
    MimeGuess::from_path(path)
        .first()
        .filter(|mime| mime.type_().as_str() == "image")
        .map(|mime| mime.essence_str().to_string())
        .unwrap_or_else(|| "image/jpeg".to_string())
}

fn gemini_generate_content_url(config: &Config) -> String {
    let endpoint = config.gemini_api_endpoint.trim();
    if endpoint.contains("{deployment-id}") {
        endpoint.replace("{deployment-id}", &config.gemini_deployment_id)
    } else if endpoint.contains("{deployment_id}") {
        endpoint.replace("{deployment_id}", &config.gemini_deployment_id)
    } else {
        endpoint.to_string()
    }
}

fn extract_gemini_text(response: GeminiGenerateResponse) -> Result<String> {
    let mut finish_reasons = Vec::new();
    for candidate in response.candidates {
        if let Some(reason) = candidate.finish_reason {
            finish_reasons.push(reason);
        }
        let Some(content) = candidate.content else {
            continue;
        };
        let text = content
            .parts
            .into_iter()
            .filter_map(|part| part.text)
            .map(|part| part.trim().to_string())
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        if !text.trim().is_empty() {
            return Ok(text);
        }
    }

    if finish_reasons.is_empty() {
        bail!("Gemini response did not contain Markdown text")
    } else {
        bail!(
            "Gemini response did not contain Markdown text; finish reasons: {}",
            finish_reasons.join(", ")
        )
    }
}

fn response_body_snippet(body: &str) -> String {
    const MAX_CHARS: usize = 600;
    let mut snippet = body.chars().take(MAX_CHARS).collect::<String>();
    if body.chars().count() > MAX_CHARS {
        snippet.push_str("...");
    }
    snippet
}

async fn ocr_document_image_with_local_glm(
    path: &Path,
    config: &Config,
    client: &Client,
) -> Result<String> {
    generate_with_ollama_images(&[path], OCR_USER_PROMPT, config, client).await
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
        options: OllamaGenerateOptions {
            num_thread: config.ollama_num_thread,
            temperature: 0.0,
        },
    };
    let url = format!(
        "{}/api/generate",
        config.ollama_base_url.trim_end_matches('/')
    );
    let response = client
        .post(url)
        .timeout(Duration::from_secs(config.ollama_timeout_seconds as u64))
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
        OsString::from("-hide_banner"),
        OsString::from("-loglevel"),
        OsString::from("error"),
        OsString::from("-y"),
        OsString::from("-i"),
        path.as_os_str().to_os_string(),
        OsString::from("-vn"),
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
        OsString::from("-t"),
        OsString::from(config.whisper_threads.to_string()),
        OsString::from("-p"),
        OsString::from(config.whisper_processors.to_string()),
        OsString::from("-bs"),
        OsString::from(config.whisper_beam_size.to_string()),
        OsString::from("-bo"),
        OsString::from(config.whisper_best_of.to_string()),
        OsString::from("-f"),
        wav_path.as_os_str().to_os_string(),
        OsString::from("-otxt"),
        OsString::from("-of"),
        output_prefix.as_os_str().to_os_string(),
    ];
    let whisper_args = if config.whisper_no_fallback {
        let mut args = whisper_args;
        args.push(OsString::from("-nf"));
        args
    } else {
        whisper_args
    };
    run_command(&config.whisper_cli, &whisper_args)
        .await
        .with_context(|| format!("failed to run {} for transcription", config.whisper_cli))?;

    let transcript_path = output_prefix.with_extension("txt");
    let transcript = fs::read_to_string(&transcript_path)
        .await
        .with_context(|| format!("failed to read transcript {}", transcript_path.display()))?;
    Ok(collapse_consecutive_repeated_transcript_lines(&transcript))
}

fn collapse_consecutive_repeated_transcript_lines(transcript: &str) -> String {
    let normalized = transcript.replace("\r\n", "\n").replace('\r', "\n");
    let mut output = Vec::new();
    let mut previous_key: Option<String> = None;

    for line in normalized.lines() {
        let trimmed = line.trim_end();
        let key = transcript_line_repetition_key(trimmed);
        if key.is_empty() {
            output.push(trimmed.to_string());
            previous_key = None;
            continue;
        }
        if previous_key.as_deref() == Some(key.as_str()) {
            continue;
        }
        output.push(trimmed.to_string());
        previous_key = Some(key);
    }

    output.join("\n").trim().to_string()
}

fn transcript_line_repetition_key(line: &str) -> String {
    let line = strip_whisper_timestamp_prefix(line.trim());
    let mut key = String::new();
    let mut pending_space = false;
    for ch in line.chars() {
        if ch.is_alphanumeric()
            || ('\u{ac00}'..='\u{d7af}').contains(&ch)
            || ('\u{4e00}'..='\u{9fff}').contains(&ch)
            || ('\u{3040}'..='\u{30ff}').contains(&ch)
        {
            if pending_space && !key.is_empty() {
                key.push(' ');
            }
            for lower in ch.to_lowercase() {
                key.push(lower);
            }
            pending_space = false;
        } else {
            pending_space = true;
        }
    }
    key.trim().to_string()
}

fn strip_whisper_timestamp_prefix(line: &str) -> &str {
    let trimmed = line.trim_start();
    if let Some(rest) = strip_wrapped_timestamp_prefix(trimmed, '[', ']') {
        return rest.trim_start();
    }
    if let Some(rest) = strip_wrapped_timestamp_prefix(trimmed, '(', ')') {
        return rest.trim_start();
    }
    if let Some(index) = trimmed.find("-->") {
        if index <= 20 {
            let after_arrow = trimmed[index + 3..].trim_start();
            let split_at = after_arrow
                .char_indices()
                .find_map(|(offset, ch)| {
                    if ch.is_whitespace() {
                        Some(offset + ch.len_utf8())
                    } else {
                        None
                    }
                })
                .unwrap_or(after_arrow.len());
            return after_arrow[split_at..].trim_start();
        }
    }
    trimmed
}

fn strip_wrapped_timestamp_prefix(line: &str, open: char, close: char) -> Option<&str> {
    if !line.starts_with(open) {
        return None;
    }
    let close_index = line.find(close)?;
    let inside = &line[open.len_utf8()..close_index];
    if inside.contains("-->") || inside.chars().filter(|ch| *ch == ':').count() >= 2 {
        Some(&line[close_index + close.len_utf8()..])
    } else {
        None
    }
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
    let mut failed_frames = 0usize;
    for (index, frame) in frames.iter().enumerate() {
        let previous = index
            .checked_sub(1)
            .and_then(|previous_index| frames.get(previous_index));
        output.push_str(&format!("### Frame {} — {}\n\n", index + 1, frame.label));
        match analyze_video_frame(frame, previous, index, frames.len(), config, client).await {
            Ok(analysis) => output.push_str(analysis.trim()),
            Err(error) => {
                failed_frames += 1;
                warn!(
                    path = %path.display(),
                    frame = index + 1,
                    error = %error,
                    "video frame analysis failed; keeping partial output"
                );
                output.push_str(&format!("_Frame analysis failed: {error:#}_"));
            }
        }
        output.push_str("\n\n");
    }
    if failed_frames > 0 {
        output.push_str("## Conversion warnings\n\n");
        output.push_str(&format!(
            "- Visual analysis failed for {failed_frames} of {} selected frames; the transcript and other frame notes were preserved.\n\n",
            frames.len()
        ));
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PrivateCommandStatus {
    Success,
    Failed,
    NotFound,
}

async fn run_private_command_status(
    program: &str,
    args: &[OsString],
    description: &str,
) -> Result<PrivateCommandStatus> {
    let output = match Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
    {
        Ok(output) => output,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(PrivateCommandStatus::NotFound);
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!("failed to execute {program} for {description}; source URL omitted")
            });
        }
    };

    if output.status.success() {
        Ok(PrivateCommandStatus::Success)
    } else {
        debug!(
            program,
            status = %output.status,
            description,
            "URL command failed; stdout/stderr omitted to avoid logging private URLs"
        );
        Ok(PrivateCommandStatus::Failed)
    }
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
    use crate::Config;

    use super::{
        ConversionRoute, collapse_consecutive_repeated_transcript_lines, csv_bytes_to_markdown,
        detect_route, extract_urls, fallback_frame_timestamps, gemini_generate_content_url,
        normalize_markdown, orientation_ocr_text_score, should_autodetect_urls,
        strip_response_markdown_fence,
    };
    use std::{net::SocketAddr, path::Path, path::PathBuf, time::Duration};

    fn test_config() -> Config {
        Config {
            input_dir: PathBuf::from("/tmp/input"),
            results_dir: PathBuf::from("/tmp/results"),
            archive_dir: PathBuf::from("/tmp/archive"),
            failed_dir: PathBuf::from("/tmp/failed"),
            bind_addr: "127.0.0.1:8080".parse::<SocketAddr>().unwrap(),
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
        assert_eq!(detect_route(Path::new("scan.jpg")), ConversionRoute::Image);
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
    fn strips_common_response_markdown_fences() {
        assert_eq!(
            strip_response_markdown_fence("```markdown\n# Title\n\nBody\n```"),
            "# Title\n\nBody"
        );
        assert_eq!(
            strip_response_markdown_fence("  ```text  \nplain OCR text\n```  "),
            "plain OCR text"
        );
        assert_eq!(
            strip_response_markdown_fence("```md\n```markdown\n# Nested\n```\n```"),
            "# Nested"
        );
    }

    #[test]
    fn keeps_programming_language_fences() {
        let fenced = "```python\nprint('visible source code')\n```";
        assert_eq!(strip_response_markdown_fence(fenced), fenced);
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

    #[test]
    fn extracts_http_urls_from_text_tokens() {
        let urls = extract_urls(
            "Please process (https://example.com/watch?id=1), and https://example.org/a.",
        );
        assert_eq!(
            urls,
            vec![
                "https://example.com/watch?id=1".to_string(),
                "https://example.org/a".to_string()
            ]
        );
    }

    #[test]
    fn only_plain_url_text_files_autodetect_urls() {
        assert!(should_autodetect_urls(Path::new("links.txt")));
        assert!(should_autodetect_urls(Path::new("recording.url")));
        assert!(!should_autodetect_urls(Path::new("notes.md")));
        assert!(!should_autodetect_urls(Path::new("script.ts")));
    }

    #[test]
    fn gemini_endpoint_template_uses_configured_deployment() {
        let config = test_config();
        assert_eq!(
            gemini_generate_content_url(&config),
            "https://api.hku.hk/gemini/student/gemini-3-flash-preview:generateContent"
        );
    }

    #[test]
    fn orientation_ocr_score_prefers_readable_text_over_noise() {
        let readable = "TO WHOM IT MAY CONCERN\nThis is to certify that the student resides here.";
        let noisy = "— | / \\\\ ~~~\n. . . __";
        assert!(orientation_ocr_text_score(readable) > orientation_ocr_text_score(noisy) + 20.0);
    }

    #[test]
    fn orientation_ocr_score_uses_tesseract_tsv_confidence() {
        let low_confidence = "\
level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n\
5\t1\t1\t1\t1\t1\t0\t0\t10\t10\t12.0\tBuoy\n\
5\t1\t1\t1\t1\t2\t0\t0\t10\t10\t18.0\tAayor\n";
        let high_confidence = "\
level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n\
5\t1\t1\t1\t1\t1\t0\t0\t10\t10\t91.0\tCONFIDENTIAL\n\
5\t1\t1\t1\t1\t2\t0\t0\t10\t10\t88.0\tSeptember\n";
        assert!(
            orientation_ocr_text_score(high_confidence)
                > orientation_ocr_text_score(low_confidence) + 20.0
        );
    }

    #[test]
    fn collapses_consecutive_repeated_whisper_lines() {
        let transcript = "\
 First line.\n\
 Repeated line!\n\
 repeated line\n\
 repeated—line.\n\
 Next line.\n";
        assert_eq!(
            collapse_consecutive_repeated_transcript_lines(transcript),
            "First line.\nRepeated line!\nNext line."
        );
    }

    #[test]
    fn collapses_timestamped_repeated_whisper_lines() {
        let transcript = "\
[00:00:01.000 --> 00:00:02.000] Same phrase here.\n\
[00:00:02.000 --> 00:00:03.000] same phrase here\n\
[00:00:03.000 --> 00:00:04.000] Different phrase.\n";
        assert_eq!(
            collapse_consecutive_repeated_transcript_lines(transcript),
            "[00:00:01.000 --> 00:00:02.000] Same phrase here.\n[00:00:03.000 --> 00:00:04.000] Different phrase."
        );
    }

    #[test]
    fn collapses_bare_timestamped_repeated_whisper_lines() {
        let transcript = "\
00:00:01.000 --> 00:00:02.000 Same phrase here.\n\
00:00:02.000 --> 00:00:03.000 same phrase here\n\
00:00:03.000 --> 00:00:04.000 Different phrase.\n";
        assert_eq!(
            collapse_consecutive_repeated_transcript_lines(transcript),
            "00:00:01.000 --> 00:00:02.000 Same phrase here.\n00:00:03.000 --> 00:00:04.000 Different phrase."
        );
    }

    #[test]
    fn preserves_non_consecutive_repeated_whisper_lines() {
        let transcript = "Repeat me.\nInterruption.\nRepeat me.\n";
        assert_eq!(
            collapse_consecutive_repeated_transcript_lines(transcript),
            "Repeat me.\nInterruption.\nRepeat me."
        );
    }
}
