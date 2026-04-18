# Universal Drop

Universal Drop is a Rust + Docker MVP that watches an input folder and exposes a small HTTP API for converting dropped or uploaded files into LLM-ready Markdown.

## What it does

- Watches `INPUT_DIR` and scans it at startup.
- Processes one job at a time to keep memory usage predictable.
- Writes Markdown results to `RESULTS_DIR/<original-name>.md`.
- Moves successfully converted originals to `ARCHIVE_DIR`.
- Leaves failed inputs in place and exposes the error through `/jobs/{id}`.
- OCRs every PDF page through Ollama using `glm-ocr` with `keep_alive: "5m"`.
- Runs `whisper.cpp` only as a subprocess during audio transcription jobs.
- Converts videos into Markdown by transcribing audio with Whisper large-v3 and analyzing only selected significant visual frames with local Ollama.

## Supported inputs

| Kind | Behavior |
| --- | --- |
| `.txt`, `.md`, code/config text files | Normalize to Markdown text. |
| `.csv`, `.tsv` | Convert to a Markdown table, capped by `MAX_CSV_ROWS`. |
| `.pdf` | Render every page with Poppler and OCR each page with Ollama `glm-ocr`. |
| `.mp3`, `.wav`, `.m4a`, `.flac`, `.ogg`, `.opus`, etc. | Normalize with `ffmpeg`, transcribe with `whisper.cpp`, then write a transcript Markdown file. |
| `.mp4`, `.mov`, `.mkv`, `.webm`, `.avi`, etc. | Extract audio for Whisper large-v3, use FFmpeg scene-change detection plus sparse fallback samples, compare selected frames through local Ollama, and write a bounded Markdown summary. |
| `.doc`, `.docx`, `.odt`, `.pptx`, `.xlsx`, etc. | Try Pandoc first, then headless LibreOffice text conversion. |
| Unknown binary files | Mark the job failed with an unsupported-type error. |

## Local Docker usage

```bash
mkdir -p \
  ~/Documents/universal-drop-input \
  ~/Documents/notes/ai_process_dump \
  ~/Documents/universal-drop-archive \
  ~/Documents/universal-drop-models/whisper

APP_PORT=9360 docker compose up --build -d

docker compose exec ollama ollama pull glm-ocr
```

Place a whisper.cpp-compatible multilingual large-v3 model at:

```text
~/Documents/universal-drop-models/whisper/ggml-large-v3.bin
```

Then drop files into `~/Documents/universal-drop-input` or upload through the API. Successful Markdown output appears in `~/Documents/notes/ai_process_dump`; originals move to `~/Documents/universal-drop-archive`.

The three mounted data folders are configurable through Compose variables:

| Compose variable | Default host directory | Container path |
| --- | --- | --- |
| `DROP_INPUT_DIR` | `/home/bfdev/Documents/universal-drop-input` | `/data/input` |
| `DROP_RESULTS_DIR` | `/home/bfdev/Documents/notes/ai_process_dump` | `/data/results` |
| `DROP_ARCHIVE_DIR` | `/home/bfdev/Documents/universal-drop-archive` | `/data/archive` |

`WHISPER_MODELS_DIR` defaults to `/home/bfdev/Documents/universal-drop-models/whisper` for the optional Whisper model mount.

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
| `OLLAMA_BASE_URL` | `http://ollama:11434` | Local Ollama API base URL. The app currently targets local HTTP Ollama. |
| `OLLAMA_MODEL` | `glm-ocr` | Multimodal OCR model. |
| `OLLAMA_KEEP_ALIVE` | `5m` | Sent in PDF OCR requests and mirrored in compose for Ollama. |
| `WHISPER_CLI` | `whisper-cli` | whisper.cpp CLI executable. |
| `WHISPER_MODEL_PATH` | `/models/whisper/ggml-large-v3.bin` | Mounted Whisper model path. |
| `VIDEO_MIN_FRAMES` | `3` | Minimum selected visual frames for video analysis; fallback samples are added when scene detection finds too few. |
| `VIDEO_MAX_FRAMES` | `24` | Hard cap on selected visual frames to prevent oversized output and overload. |
| `VIDEO_SCENE_THRESHOLD` | `0.35` | FFmpeg scene-change threshold from `0.0` to `1.0`; lower values select more frames. |
| `MAX_CSV_ROWS` | `1000` | CSV/TSV Markdown row cap. |
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
RESULTS_DIR=~/Documents/notes/ai_process_dump \
ARCHIVE_DIR=~/Documents/universal-drop-archive \
OLLAMA_BASE_URL=http://localhost:11434 \
cargo run
```

## Notes

- PDF conversion always uses OCR; there is intentionally no text-first fallback in the MVP.
- Video conversion does not analyze every frame. It uses FFmpeg scene detection with a configured min/max frame budget, then asks local Ollama to describe the first key frame and compare each selected frame to the previous selected frame.
- The job store is in-memory for this MVP. Restarting the service clears job metadata, but existing input files are scanned again on startup.
- Uploads are written to hidden `.uploading` files first, then atomically renamed so the watcher does not process partial uploads.
