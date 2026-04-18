# Universal Drop MVP Plan

## Summary
Build a greenfield Rust + Docker project that converts dropped/uploaded files into LLM-ready Markdown text.

- MVP shape: attached filesystem folders plus a minimal HTTP API.
- Input/result/archive folders are Docker-mounted volumes.
- Results are Markdown only for users; internal job state may use SQLite or in-memory metadata.
- Processing is single-job-at-a-time to conserve memory.
- PDFs are always OCR-read page-by-page with Ollama `glm-ocr`.
- Audio is transcribed by invoking `whisper.cpp` only during audio jobs, so Whisper models are not resident when idle.
- Originals are moved to an archive folder after successful conversion.

## Key Implementation Changes
- Scaffold a Rust service using:
  - `axum` for HTTP API.
  - `tokio` async runtime.
  - `notify` for watching the input directory.
  - `serde`, `uuid`, `mime_guess`, `tracing`.
  - optional `sqlx` + SQLite for durable job tracking.
- Add Docker assets:
  - `Dockerfile` for the Rust app with required CLI tools: `ffmpeg`, `poppler-utils`, `libreoffice`, `pandoc` if practical, `whisper.cpp`.
  - `docker-compose.yml` with:
    - `app` service.
    - `ollama` service.
    - mounted `./input:/data/input`, `./results:/data/results`, `./archive:/data/archive`, and model/cache volumes.
    - `OLLAMA_KEEP_ALIVE=5m` and app requests using `keep_alive: "5m"` for `glm-ocr`.
- Add repo hygiene:
  - `.gitignore` including `.env`, `.env.*`, `.secrets/`, logs/cache/build outputs, OS/IDE files, `node_modules/`, `target/`, and `DEPLOYMENT.md`.
  - `README.md` with local Docker usage.
  - `DEPLOYMENT.md` kept gitignored if deployment details are needed later.

## Conversion Behavior
- File discovery:
  - Watch `/data/input`.
  - Also scan input folder on startup for unprocessed files.
  - Queue one job at a time.
  - On success, write Markdown to `/data/results/<original-name>.md` and move the source file to `/data/archive/`.
  - On failure, keep the input file in place and expose failure status through the API/logs.
- HTTP API:
  - `GET /health`
  - `POST /files` multipart upload; stores into input folder and queues conversion.
  - `GET /jobs` list recent jobs.
  - `GET /jobs/{id}` status.
  - `GET /jobs/{id}/result` returns Markdown when ready.
- PDF handling:
  - Render every PDF page to images using Poppler.
  - Send each page image to Ollama `glm-ocr`.
  - Produce Markdown with page headings and preserved tables/layout where possible.
  - Use `glm-ocr` because Ollama lists it as a multimodal OCR model for complex document understanding, and expose it through local Ollama API.
- Audio handling:
  - Use `ffmpeg` to normalize/extract audio.
  - Invoke `whisper.cpp` CLI as a subprocess per audio job.
  - Configure model path through `WHISPER_MODEL_PATH`, defaulting to a mounted multilingual small model path.
  - Do not run Whisper as a long-lived server.
- Document handling:
  - Plain text/Markdown: copy/normalize to Markdown.
  - CSV/TSV: convert to Markdown table, with row-count safeguards for very large files.
  - DOC/DOCX/ODT/PPT/PPTX/XLS/XLSX and similar office files: convert through headless LibreOffice and/or Pandoc to text/Markdown.
  - Unknown binary types: mark unsupported with a clear job error.

## Test Plan
- Unit tests:
  - MIME/extension routing.
  - output path generation.
  - archive behavior.
  - Markdown normalization.
  - CSV-to-Markdown conversion.
- Integration tests:
  - Drop a `.txt`, `.csv`, `.docx`, audio sample, and PDF into input folder and verify `.md` outputs.
  - Upload through `POST /files` and fetch result through API.
  - Failed conversion leaves original in input folder.
  - Successful conversion moves original to archive.
- Docker smoke tests:
  - `docker compose up --build`.
  - `GET /health` succeeds.
  - Ollama service reachable from app.
  - `glm-ocr` request uses 5-minute keep-alive behavior.
  - Whisper process is absent when idle and only appears during audio processing.

## Assumptions and Defaults
- Default result artifact is only the user-facing Markdown file; job metadata is internal.
- Default Whisper model is multilingual `small`, configurable by environment variable.
- PDF OCR is always used, not text-first fallback.
- Concurrency is one conversion job at a time.
- Ollama unload policy follows Ollama's documented default/`keep_alive` behavior: models are kept in memory for 5 minutes unless configured otherwise. Source: https://github.com/ollama/ollama/blob/main/docs/faq.mdx
- GLM-OCR model details are based on the Ollama model page: https://ollama.com/library/glm-ocr
