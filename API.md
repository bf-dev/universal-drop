# Universal Drop API

Default local deployment base URL:

```text
http://localhost:9360
```

`docker-compose.yml` maps host port `${APP_PORT:-9360}` to container port `8080`, so set `APP_PORT` before starting Compose if you want a different host port.

Current Docker defaults mount these host data directories:

| Purpose | Host directory | Container directory |
| --- | --- | --- |
| Input/drop folder | `/home/bfdev/Documents/universal-drop-input` | `/data/input` |
| Markdown results | `/home/bfdev/Documents/universal-drop-outputs` | `/data/results` |
| Successful-original archive | `/home/bfdev/Documents/universal-drop-archive` | `/data/archive` |
| Terminal-failure dead letter | `/home/bfdev/Documents/universal-drop-failed` | `/data/failed` |

Override them with `DROP_INPUT_DIR`, `DROP_RESULTS_DIR`, `DROP_ARCHIVE_DIR`, and `DROP_FAILED_DIR` when starting Docker Compose.

## Health

Check that the HTTP service is running.

```bash
curl http://localhost:9360/health
```

Response:

```json
{"status":"ok"}
```

## Upload files

Upload one or more files as multipart form fields. Each file is stored in the input directory and queued for conversion.

```bash
curl -F "file=@./example.pdf" http://localhost:9360/files
```

Multiple files:

```bash
curl \
  -F "file=@./notes.txt" \
  -F "file=@./table.csv" \
  http://localhost:9360/files
```

Successful response: `201 Created`

```json
{
  "jobs": [
    {
      "id": "00000000-0000-0000-0000-000000000000",
      "filename": "example.pdf",
      "input_path": "/data/input/example.pdf",
      "result_path": "/data/results/example.pdf.md",
      "archive_path": null,
      "failed_path": null,
      "status": "queued",
      "error": null,
      "attempts": 0,
      "created_at": "2026-04-18T00:00:00Z",
      "updated_at": "2026-04-18T00:00:00Z"
    }
  ]
}
```

## List recent jobs

```bash
curl http://localhost:9360/jobs
```

Successful response: `200 OK`

```json
{
  "jobs": [
    {
      "id": "00000000-0000-0000-0000-000000000000",
      "filename": "notes.txt",
      "input_path": "/data/input/notes.txt",
      "result_path": "/data/results/notes.txt.md",
      "archive_path": "/data/archive/notes.txt",
      "failed_path": null,
      "status": "succeeded",
      "error": null,
      "attempts": 1,
      "created_at": "2026-04-18T00:00:00Z",
      "updated_at": "2026-04-18T00:00:01Z"
    }
  ]
}
```

Job statuses are:

- `queued`
- `running`
- `succeeded`
- `failed`

## Get one job

```bash
curl http://localhost:9360/jobs/<job-id>
```

Successful response: `200 OK` with the job object.

Missing job response: `404 Not Found`

```json
{"error":"job not found: <job-id>"}
```

## Get Markdown result

Returns the Markdown output for a completed job.

```bash
curl http://localhost:9360/jobs/<job-id>/result
```

Successful response:

- Status: `200 OK`
- Content-Type: `text/markdown; charset=utf-8`
- Body: Markdown text

If the job has not succeeded yet, the API returns `409 Conflict`:

```json
{"error":"job <job-id> is not complete; status is Running"}
```

If the job succeeded but the Markdown file is missing, the API returns `404 Not Found`.

## Folder-based usage

You can skip the upload endpoint and drop files directly into the input volume:

```bash
cp ./example.pdf ~/Documents/universal-drop-input/
```

The service watches the input directory, converts the file, writes Markdown to `~/Documents/universal-drop-outputs/<original-name>.md`, and moves the original into `~/Documents/universal-drop-archive/` after success. After `MAX_JOB_ATTEMPTS` failed attempts, it moves the original into `~/Documents/universal-drop-failed/` so the watched input folder cannot keep re-queuing the same bad file.

## Conversion notes

- PDFs are always rendered page-by-page, auto-oriented with native OpenCV when a whole page is confidently sideways or upside down, confirmed with local Tesseract OCR scoring, and OCRed into Markdown with Qianfan OCR (`QIANFAN_OCR_MODEL`, default `baidu/Qianfan-OCR`).
- PDF pages render at `PDF_RENDER_DPI` before OCR; the default is `150` for faster CPU OCR on this M1/Asahi host. `PDF_AUTO_ORIENT=true` runs OpenCV orientation detection first and only applies 90/180/270-degree rotations, never small deskew rotations. `PDF_ORIENT_OCR_CONFIRM=true` then compares Tesseract TSV confidence scores for the original render and proposed rotated render, and only replaces the original render when the candidate score improves enough.
- Standalone image files (`.jpg`, `.png`, `.webp`, `.gif`, `.bmp`, `.tif`, `.heic`, etc.) are also OCRed into Markdown with Qianfan OCR.
- Configure `QIANFAN_OCR_BASE_URL` to point at an OpenAI-compatible Qianfan OCR service; the bundled Apple Silicon helper serves the public `baidu/Qianfan-OCR` model id through MLX and falls back to a preconverted Qianfan MLX checkpoint when direct conversion is not supported by current `mlx-vlm`. Requests are bounded by `QIANFAN_OCR_TIMEOUT_SECONDS` and `QIANFAN_OCR_MAX_TOKENS`.
- Audio files are normalized with `ffmpeg` and transcribed by launching `whisper.cpp` only for that job. Consecutive duplicate Whisper lines are collapsed before Markdown output. Whisper defaults are tuned for speed: 8 threads, beam size 1, best-of 1, and no fallback retries.
- Video files are converted locally by extracting audio for Whisper large-v3 and selecting a bounded set of significant visual frames with FFmpeg scene-change detection. The service analyzes selected frames through Qianfan OCR instead of describing every frame. If a selected frame fails analysis, the job keeps the transcript and other frame notes instead of failing the whole video.
- Video frame controls are `VIDEO_MIN_FRAMES`, `VIDEO_MAX_FRAMES`, and `VIDEO_SCENE_THRESHOLD`.
- CSV/TSV output is capped by `MAX_CSV_ROWS`.
- Terminal failed conversions move the source file to the failed/dead-letter directory and put the failure message in the job's `error` field.
