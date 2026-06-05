#!/usr/bin/env python3
"""Small OpenAI-compatible Qianfan OCR server for Apple Silicon MLX.

The shell launcher keeps the public OCR model id as baidu/Qianfan-OCR. It first
tries to convert that official Hugging Face model to MLX; when current mlx-vlm
does not support direct qianfan_ocr conversion yet, it downloads a preconverted
Qianfan MLX checkpoint and serves it under the official model id.
"""
from __future__ import annotations

import base64
import json
import mimetypes
import os
import tempfile
import threading
import time
import traceback
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any
from urllib.parse import urlparse

from mlx_vlm import generate, load
from mlx_vlm.prompt_utils import apply_chat_template
from mlx_vlm.utils import load_config

MODEL_ID = os.environ.get("QIANFAN_OCR_MODEL", "baidu/Qianfan-OCR")
MLX_PATH = os.environ.get(
    "QIANFAN_OCR_MLX_PATH",
    str(Path.home() / "Documents/universal-drop-models/qianfan-ocr-mlx/baidu-Qianfan-OCR-4bit"),
)
HOST = os.environ.get("QIANFAN_OCR_HOST", "0.0.0.0")
PORT = int(os.environ.get("QIANFAN_OCR_PORT", "9361"))
DEFAULT_MAX_TOKENS = int(os.environ.get("QIANFAN_OCR_MAX_TOKENS", "4096"))

print(f"Loading Qianfan OCR MLX checkpoint: {MLX_PATH}", flush=True)
_MODEL, _PROCESSOR = load(MLX_PATH, trust_remote_code=True)
_CONFIG = load_config(MLX_PATH)
_GENERATE_LOCK = threading.Lock()


def _json_response(handler: BaseHTTPRequestHandler, status: int, payload: dict[str, Any]) -> None:
    body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
    handler.send_response(status)
    handler.send_header("Content-Type", "application/json; charset=utf-8")
    handler.send_header("Content-Length", str(len(body)))
    handler.end_headers()
    handler.wfile.write(body)


def _error(handler: BaseHTTPRequestHandler, status: int, message: str) -> None:
    _json_response(handler, status, {"error": message})


def _extract_prompt_and_images(messages: list[dict[str, Any]], tmpdir: Path) -> tuple[str, list[str]]:
    text_parts: list[str] = []
    images: list[str] = []

    for message in messages:
        role = message.get("role") or "user"
        content = message.get("content")
        if isinstance(content, str):
            text_parts.append(f"{role}: {content}" if role not in {"user", "system"} else content)
            continue
        if not isinstance(content, list):
            continue
        for item in content:
            if not isinstance(item, dict):
                continue
            item_type = item.get("type")
            if item_type == "text":
                value = item.get("text")
                if isinstance(value, str) and value.strip():
                    text_parts.append(value)
            elif item_type == "image_url":
                value = item.get("image_url")
                url = value.get("url") if isinstance(value, dict) else value
                if isinstance(url, str):
                    images.append(_materialize_image(url, tmpdir))
            elif item_type == "image":
                value = item.get("image")
                if isinstance(value, str):
                    images.append(_materialize_image(value, tmpdir))

    prompt = "\n\n".join(part.strip() for part in text_parts if part.strip())
    if not prompt:
        prompt = "Parse this document to Markdown."
    return prompt, images


def _materialize_image(url: str, tmpdir: Path) -> str:
    if not url.startswith("data:"):
        return url
    header, encoded = url.split(",", 1)
    mime = header[5:].split(";", 1)[0] or "image/png"
    ext = mimetypes.guess_extension(mime) or ".png"
    target = tmpdir / f"image-{len(list(tmpdir.iterdir()))}{ext}"
    target.write_bytes(base64.b64decode(encoded))
    return str(target)


class Handler(BaseHTTPRequestHandler):
    server_version = "qianfan-ocr-mlx/1.0"

    def log_message(self, fmt: str, *args: Any) -> None:
        print(f"[{time.strftime('%Y-%m-%dT%H:%M:%S')}] {self.address_string()} {fmt % args}", flush=True)

    def do_GET(self) -> None:  # noqa: N802
        path = urlparse(self.path).path
        if path in {"/health", "/v1/health"}:
            _json_response(self, HTTPStatus.OK, {"status": "ok", "model": MODEL_ID, "mlx_path": MLX_PATH})
            return
        if path == "/v1/models":
            _json_response(
                self,
                HTTPStatus.OK,
                {"object": "list", "data": [{"id": MODEL_ID, "object": "model", "owned_by": "local"}]},
            )
            return
        _error(self, HTTPStatus.NOT_FOUND, "not found")

    def do_POST(self) -> None:  # noqa: N802
        path = urlparse(self.path).path
        if path not in {"/chat/completions", "/v1/chat/completions"}:
            _error(self, HTTPStatus.NOT_FOUND, "not found")
            return
        try:
            length = int(self.headers.get("Content-Length", "0"))
            payload = json.loads(self.rfile.read(length).decode("utf-8"))
            if payload.get("stream"):
                _error(self, HTTPStatus.BAD_REQUEST, "streaming responses are not supported by this OCR wrapper")
                return
            messages = payload.get("messages")
            if not isinstance(messages, list):
                _error(self, HTTPStatus.BAD_REQUEST, "messages must be an array")
                return
            max_tokens = int(payload.get("max_tokens") or DEFAULT_MAX_TOKENS)
            with tempfile.TemporaryDirectory(prefix="qianfan-ocr-") as tmp:
                prompt, images = _extract_prompt_and_images(messages, Path(tmp))
                if not images:
                    _error(self, HTTPStatus.BAD_REQUEST, "at least one image is required")
                    return
                formatted_prompt = apply_chat_template(_PROCESSOR, _CONFIG, prompt, num_images=len(images))
                with _GENERATE_LOCK:
                    output = generate(
                        _MODEL,
                        _PROCESSOR,
                        formatted_prompt,
                        images,
                        max_tokens=max_tokens,
                    )
            created = int(time.time())
            _json_response(
                self,
                HTTPStatus.OK,
                {
                    "id": f"chatcmpl-qianfan-{created}",
                    "object": "chat.completion",
                    "created": created,
                    "model": MODEL_ID,
                    "choices": [
                        {
                            "index": 0,
                            "message": {"role": "assistant", "content": output},
                            "finish_reason": "stop",
                        }
                    ],
                },
            )
        except Exception as exc:  # keep local server operator-friendly
            traceback.print_exc()
            _error(self, HTTPStatus.INTERNAL_SERVER_ERROR, str(exc))


def main() -> None:
    server = ThreadingHTTPServer((HOST, PORT), Handler)
    print(f"Qianfan OCR MLX server listening on http://{HOST}:{PORT}/v1", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
