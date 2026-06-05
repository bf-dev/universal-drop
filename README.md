# Universal Drop

Universal Drop is a Rust + Docker service that watches an input folder and exposes a small HTTP API for converting dropped or uploaded files into LLM-ready Markdown.

## What it does

- Watches `INPUT_DIR` and scans it at startup.
- Processes one job at a time to keep memory usage predictable.
- Writes Markdown results to `RESULTS_DIR/<original-name>.md`.
- Moves successfully converted originals to `ARCHIVE_DIR`.
- Moves terminally failed originals to `FAILED_DIR` after `MAX_JOB_ATTEMPTS`, so one bad file cannot keep re-entering the watched queue forever.
- Accepts plain text through `/text` and auto-detects HTTP(S) URLs in `.txt` drops.
- OCRs PDF pages and standalone image files through a local Qianfan OCR MLX service, defaulting to `baidu/Qianfan-OCR`.
- Auto-detects and corrects whole-page PDF render orientation with native OpenCV before OCR, rotating only by 90/180/270 degrees when confidence is high enough. It never performs small-angle deskew rotations.
- Runs `whisper.cpp` only as a subprocess during audio transcription jobs, with CPU-native/OpenBLAS build acceleration and fast large-v3 decoding defaults.
- Converts videos into Markdown by transcribing audio with Whisper large-v3 and analyzing only selected significant visual frames with Qianfan OCR.
- Converts URLs by trying `yt-dlp` first for media/recording pages, then falling back to headless Chromium webpage capture plus PDF/page OCR.

## Supported inputs

| Kind | Behavior |
| --- | --- |
| `.txt`, `.text`, `.url`, `.urls` | Normalize to Markdown text and auto-detect HTTP(S) URLs for URL conversion. |
| `.md`, code/config text files | Normalize to Markdown text without URL expansion. |
| `.csv`, `.tsv` | Convert to a Markdown table, capped by `MAX_CSV_ROWS`. |
| `.jpg`, `.jpeg`, `.png`, `.webp`, `.gif`, `.bmp`, `.tif`, `.tiff`, `.heic`, `.heif` | OCR the image into faithful Markdown with Qianfan OCR. |
| `.pdf` | Render every page with Poppler, auto-orient whole upside-down/sideways page renders with native OpenCV, confirm proposed rotations with local Tesseract OCR scoring, and OCR each page into faithful Markdown with Qianfan OCR. |
| `.mp3`, `.wav`, `.m4a`, `.flac`, `.ogg`, `.opus`, etc. | Normalize with `ffmpeg`, transcribe with `whisper.cpp`, collapse consecutive duplicate Whisper lines, then write a transcript Markdown file. |
| `.mp4`, `.mov`, `.mkv`, `.webm`, `.avi`, etc. | Extract audio for Whisper large-v3, use FFmpeg scene-change detection plus sparse fallback samples, analyze selected frames through Qianfan OCR, and write a bounded Markdown summary. |
| `.doc`, `.docx`, `.odt`, `.pptx`, `.xlsx`, etc. | Try Pandoc first, then headless LibreOffice text conversion. |
| HTTP(S) URLs submitted through `/text` or a URL text file | Try `yt-dlp` for YouTube and other supported media/recording pages; if no media is downloaded, capture the webpage with headless Chromium into paginated pages and OCR it. |
| Unknown binary files | Mark the job failed with an unsupported-type error. |

## Local Docker usage

```bash
mkdir -p \
  ~/Documents/universal-drop-input \
  ~/Documents/universal-drop-outputs \
  ~/Documents/universal-drop-archive \
  ~/Documents/universal-drop-models/whisper

APP_PORT=9360 docker compose up --build -d
```

Run a local Qianfan OCR MLX service on the host. The script serves the public model id `baidu/Qianfan-OCR` through an OpenAI-compatible `/v1/chat/completions` endpoint. It first tries to convert the official Hugging Face model to a 4-bit MLX checkpoint under `~/Documents/universal-drop-models/qianfan-ocr-mlx`; if current `mlx-vlm` cannot convert the Qianfan architecture directly yet, it downloads a preconverted Qianfan MLX checkpoint for Apple Silicon inference:

```bash
scripts/qianfan-ocr-mlx-server.sh
```

Point the app at that service through `.env` or the shell:

```bash
QIANFAN_OCR_BASE_URL=http://host.docker.internal:9361/v1
QIANFAN_OCR_MODEL=baidu/Qianfan-OCR
```

Place a whisper.cpp-compatible multilingual large-v3 model at:

```text
~/Documents/universal-drop-models/whisper/ggml-large-v3.bin
```

Then drop files into `~/Documents/universal-drop-input` or upload through the API. Successful Markdown output appears in `~/Documents/universal-drop-outputs`; successful originals move to `~/Documents/universal-drop-archive`; terminal failures move to `~/Documents/universal-drop-failed`.

The mounted data folders are configurable through Compose variables:

| Compose variable | Default host directory | Container path |
| --- | --- | --- |
| `DROP_INPUT_DIR` | `/Users/bfdev/Documents/universal-drop-input` | `/data/input` |
| `DROP_RESULTS_DIR` | `/Users/bfdev/Documents/universal-drop-outputs` | `/data/results` |
| `DROP_ARCHIVE_DIR` | `/Users/bfdev/Documents/universal-drop-archive` | `/data/archive` |
| `DROP_FAILED_DIR` | `/Users/bfdev/Documents/universal-drop-failed` | `/data/failed` |

`WHISPER_MODELS_DIR` defaults to `/Users/bfdev/Documents/universal-drop-models/whisper` for the optional Whisper model mount.

## API

### Health

```bash
curl http://localhost:9360/health
```

### Upload a file

```bash
curl -F "file=@./example.pdf" http://localhost:9360/files
```

The response includes the queued job ID.

### Submit plain text or URLs

JSON:

```bash
curl -X POST http://localhost:9360/text \
  -H "Content-Type: application/json" \
  -d '{"text":"https://example.com/page","filename":"links.txt"}'
```

Raw UTF-8 text:

```bash
printf 'https://example.com/page\n' | curl -X POST http://localhost:9360/text \
  -H "Content-Type: text/plain; charset=utf-8" \
  --data-binary @-
```

The response matches `/files` and includes the queued job ID.

### List jobs

```bash
curl http://localhost:9360/jobs
```

### Inspect one job

```bash
curl http://localhost:9360/jobs/<job-id>
```

### Fetch a finished Markdown result

```bash
curl http://localhost:9360/jobs/<job-id>/result
```

If the job is not complete, the endpoint returns `409 Conflict`.

## Environment variables

| Variable | Default | Notes |
| --- | --- | --- |
| `BIND_ADDR` | `0.0.0.0:8080` | Container HTTP listen address. Compose publishes it to host port `APP_PORT`, default `9360`. |
| `INPUT_DIR` | `/data/input` | Watched/drop folder. |
| `RESULTS_DIR` | `/data/results` | Markdown output folder. |
| `ARCHIVE_DIR` | `/data/archive` | Successful-original archive. |
| `FAILED_DIR` | `/data/failed` | Terminal-failure dead-letter folder. |
| `QIANFAN_OCR_BASE_URL` | `http://host.docker.internal:9361/v1` | OpenAI-compatible Qianfan OCR service base URL. |
| `QIANFAN_OCR_MODEL` | `baidu/Qianfan-OCR` | Public model id sent to the OCR service and exposed by the local MLX wrapper. |
| `QIANFAN_OCR_MLX_REPO` | `jason1966/Qianfan-OCR-MLX-4bit` | Apple Silicon MLX checkpoint fallback used only when direct conversion of `baidu/Qianfan-OCR` is not supported by current `mlx-vlm`. |
| `QIANFAN_OCR_TIMEOUT_SECONDS` | `600` | Per-image Qianfan OCR HTTP timeout. |
| `QIANFAN_OCR_MAX_TOKENS` | `4096` | Maximum generated OCR tokens per image/page. |
| `WHISPER_CLI` | `whisper-cli` | whisper.cpp CLI executable. |
| `WHISPER_MODEL_PATH` | `/models/whisper/ggml-large-v3.bin` | Mounted Whisper model path. |
| `WHISPER_THREADS` | `8` | whisper.cpp compute threads. |
| `WHISPER_PROCESSORS` | `1` | whisper.cpp processor count. |
| `WHISPER_BEAM_SIZE` | `1` | Faster greedy-ish decoding; increase for slower, more exhaustive decoding. |
| `WHISPER_BEST_OF` | `1` | Faster decoding candidate count. |
| `WHISPER_NO_FALLBACK` | `true` | Disable temperature fallback retries for speed. |
| `PDF_RENDER_DPI` | `150` | PDF page render DPI before OCR; higher values may improve tiny text but slow CPU OCR. |
| `PDF_AUTO_ORIENT` | `true` | Run native OpenCV page-orientation detection before PDF OCR. Only 90/180/270-degree whole-page rotations are applied; small skew angles under the page-rotation threshold are not changed. |
| `PDF_AUTO_ORIENT_CLI` | `pdf-page-auto-orient` | OpenCV helper executable used for PDF page orientation detection and safe rotation. |
| `PDF_ORIENT_OCR_CONFIRM` | `true` | Confirm every proposed PDF page rotation with local Tesseract TSV confidence scoring before replacing the rendered original page. |
| `PDF_ORIENT_OCR_CLI` | `tesseract` | OCR executable used only for recognition-driven page-orientation confirmation. |
| `PDF_ORIENT_OCR_LANG` | `eng` | Tesseract language list for orientation confirmation. |
| `PDF_ORIENT_OCR_MIN_CONFIDENCE` | `0.60` | Minimum relative OCR-score improvement required before applying a proposed page rotation. |
| `PDF_ORIENT_OCR_MIN_SCORE` | `20` | Minimum candidate OCR score required before applying a proposed page rotation. |
| `URL_MAX_PER_TEXT` | `8` | Maximum detected URLs expanded from one `.txt`/text drop. Additional URLs are skipped to avoid runaway jobs. |
| `YT_DLP_CLI` | `yt-dlp` | Executable used to download YouTube and other yt-dlp-supported media/recording pages before running the video/audio conversion flow. |
| `HEADLESS_BROWSER_CLI` | `chromium` | Headless browser executable used for webpage capture. The converter also falls back to common Chromium/Chrome binary names. |
| `WEBPAGE_CAPTURE_VIRTUAL_TIME_MS` | `5000` | Chromium virtual-time budget in milliseconds before printing/capturing a webpage to paginated PDF for OCR. |
| `VIDEO_MIN_FRAMES` | `3` | Minimum selected visual frames for video analysis; fallback samples are added when scene detection finds too few. |
| `VIDEO_MAX_FRAMES` | `24` | Hard cap on selected visual frames to prevent oversized output and overload. |
| `VIDEO_SCENE_THRESHOLD` | `0.35` | FFmpeg scene-change threshold from `0.0` to `1.0`; lower values select more frames. |
| `MAX_CSV_ROWS` | `1000` | CSV/TSV Markdown row cap. |
| `MAX_JOB_ATTEMPTS` | `1` | Attempts per queued file before terminal failure. Raise this only for transient infrastructure issues. |
| `JOB_RETRY_BACKOFF_SECONDS` | `30` | Delay between attempts when `MAX_JOB_ATTEMPTS` is above 1. |
| `FILE_STABILITY_CHECKS` | `3` | Number of stable metadata checks before processing a dropped file. |
| `FILE_STABILITY_DELAY_MS` | `500` | Delay between file-stability checks. |

## Development

```bash
cargo fmt --all
cargo test
cargo run
```

For local non-Docker runs, set the data directories explicitly:

```bash
INPUT_DIR=~/Documents/universal-drop-input \
RESULTS_DIR=~/Documents/universal-drop-outputs \
ARCHIVE_DIR=~/Documents/universal-drop-archive \
QIANFAN_OCR_BASE_URL=http://localhost:9361/v1 \
cargo run
```

## Notes

- PDF and image conversion always use OCR through Qianfan OCR; there is intentionally no text-first fallback.
- Video conversion does not analyze every frame. It uses FFmpeg scene detection with a configured min/max frame budget, then asks Qianfan OCR to analyze selected frames.
- URL conversion omits source URLs from generated URL section headings and command failure logs to avoid accidental disclosure in logs. The original submitted text is still preserved in the Markdown for `.txt`/text drops.
- The job store is in-memory. Restarting the service clears job metadata, but existing input files are scanned again on startup.
- Uploads are written to hidden `.uploading` files first, then atomically renamed so the watcher does not process partial uploads.
