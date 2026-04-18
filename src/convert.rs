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
    let bytes = fs::read(path)
        .await
        .with_context(|| format!("failed to read rendered page {}", path.display()))?;
    let image = BASE64.encode(bytes);
    let prompt = "Read this document page using OCR. Return faithful Markdown only. Preserve tables, headings, lists, reading order, and visible text. Do not add commentary.";
    let request = OllamaGenerateRequest {
        model: &config.ollama_model,
        prompt,
        images: vec![image],
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
    let title = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| "audio".into());
    Ok(format!("# Transcript: {title}\n\n{}\n", transcript.trim()))
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
    use super::{ConversionRoute, csv_bytes_to_markdown, detect_route, normalize_markdown};
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
}
