#!/usr/bin/env bash
set -euo pipefail

MODEL_ID="${QIANFAN_OCR_MODEL:-baidu/Qianfan-OCR}"
HOST="${QIANFAN_OCR_HOST:-0.0.0.0}"
PORT="${QIANFAN_OCR_PORT:-9361}"
PYTHON="${QIANFAN_OCR_PYTHON:-python3.12}"
ROOT="${QIANFAN_OCR_ROOT:-$HOME/Documents/universal-drop-models/qianfan-ocr-mlx}"
VENV="${QIANFAN_OCR_VENV:-$ROOT/.venv}"
MLX_PATH="${QIANFAN_OCR_MLX_PATH:-$ROOT/baidu-Qianfan-OCR-4bit}"
Q_BITS="${QIANFAN_OCR_Q_BITS:-4}"

if ! command -v "$PYTHON" >/dev/null 2>&1; then
  echo "Missing $PYTHON. Set QIANFAN_OCR_PYTHON to a Python 3.10+ executable." >&2
  exit 1
fi

if [ ! -x "$VENV/bin/python" ]; then
  mkdir -p "$ROOT"
  "$PYTHON" -m venv "$VENV"
  "$VENV/bin/python" -m pip install --upgrade pip wheel
  "$VENV/bin/python" -m pip install --upgrade mlx-vlm huggingface-hub
fi

if [ ! -f "$MLX_PATH/config.json" ]; then
  mkdir -p "$MLX_PATH"
  echo "Converting $MODEL_ID to MLX at $MLX_PATH ..." >&2
  if [ -x "$VENV/bin/mlx_vlm.convert" ]; then
    "$VENV/bin/mlx_vlm.convert" \
      --hf-path "$MODEL_ID" \
      --mlx-path "$MLX_PATH" \
      --quantize \
      --q-bits "$Q_BITS"
  else
    "$VENV/bin/python" -m mlx_vlm.convert \
      --hf-path "$MODEL_ID" \
      --mlx-path "$MLX_PATH" \
      --quantize \
      --q-bits "$Q_BITS"
  fi
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export QIANFAN_OCR_MODEL="$MODEL_ID"
export QIANFAN_OCR_MLX_PATH="$MLX_PATH"
exec "$VENV/bin/python" "$SCRIPT_DIR/qianfan_ocr_mlx_server.py"
