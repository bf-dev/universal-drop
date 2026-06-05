# Universal Drop Plan

## Summary
Universal Drop converts dropped/uploaded files into LLM-ready Markdown through a Rust HTTP service and watched folders.

- Input/result/archive/failed folders are Docker-mounted volumes.
- Processing is single-job-at-a-time to keep memory predictable.
- PDF/image/webpage/video-frame OCR uses a local Qianfan OCR service, defaulting to `baidu/Qianfan-OCR`.
- Audio is transcribed by invoking `whisper.cpp` only during audio jobs.
- Videos transcribe audio and analyze a bounded set of selected visual frames.
- Originals move to archive after successful conversion; terminal failures move to the failed folder.

## Key Implementation Shape
- Rust service with `axum`, `tokio`, `notify`, `serde`, `uuid`, `mime_guess`, and `tracing`.
- Docker app container with `ffmpeg`, Poppler, LibreOffice, Pandoc, Tesseract, Chromium, `whisper.cpp`, and the PDF orientation helper.
- Host-native Qianfan OCR service exposed through an OpenAI-compatible `/v1/chat/completions` endpoint.
- Data folders mounted under `/data/input`, `/data/results`, `/data/archive`, and `/data/failed`.

## Conversion Behavior
- `GET /health`, `POST /files`, `POST /text`, `GET /jobs`, `GET /jobs/{id}`, and `GET /jobs/{id}/result`.
- PDFs render every page to images, auto-orient safe whole-page rotations, then send each page image to Qianfan OCR.
- Images go directly to Qianfan OCR.
- Audio normalizes through `ffmpeg` and transcribes through `whisper.cpp`.
- Videos extract/transcribe audio, select frames with FFmpeg scene-change detection, and analyze selected frames with Qianfan OCR.
- Plain text/Markdown normalize directly; CSV/TSV convert to capped Markdown tables; office files try Pandoc then LibreOffice.

## Test Plan
- Unit tests for route detection, output path generation, Markdown normalization, CSV conversion, URL extraction, transcript dedupe, and Qianfan URL construction.
- Runtime checks for health, upload/result flow, watched-folder processing, and failed-file dead-letter behavior.
- Build checks for Docker compose config and release build.
