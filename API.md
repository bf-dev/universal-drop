# Universal Drop API

Default local deployment base URL:

```text
http://localhost:9360
```

`docker-compose.yml` maps host port `${APP_PORT:-9360}` to container port `8080`, so set `APP_PORT` before starting Compose if you want a different host port.

Current Docker defaults mount three host data directories:

| Purpose | Host directory | Container directory |
| --- | --- | --- |
| Input/drop folder | `/home/bfdev/Documents/universal-drop-input` | `/data/input` |
| Markdown results | `/home/bfdev/Documents/notes/ai_process_dump` | `/data/results` |
| Successful-original archive | `/home/bfdev/Documents/universal-drop-archive` | `/data/archive` |

Override them with `DROP_INPUT_DIR`, `DROP_RESULTS_DIR`, and `DROP_ARCHIVE_DIR` when starting Docker Compose.

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
      "status": "queued",
      "error": null,
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
      "status": "succeeded",
      "error": null,
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

The service watches the input directory, converts the file, writes Markdown to `~/Documents/notes/ai_process_dump/<original-name>.md`, and moves the original into `~/Documents/universal-drop-archive/` after success.

## Conversion notes

- PDFs are always rendered page-by-page and OCRed with Ollama `glm-ocr`.
- Audio files are normalized with `ffmpeg` and transcribed by launching `whisper.cpp` only for that job.
- Video files are converted locally by extracting audio for Whisper large-v3 and selecting a bounded set of significant visual frames with FFmpeg scene-change detection. The service analyzes the first selected frame and compares later selected frames to the previous selected frame through local Ollama, instead of describing every frame.
- Video frame controls are `VIDEO_MIN_FRAMES`, `VIDEO_MAX_FRAMES`, and `VIDEO_SCENE_THRESHOLD`.
- CSV/TSV output is capped by `MAX_CSV_ROWS`.
- Failed conversions leave the source file in the input directory and put the failure message in the job's `error` field.
