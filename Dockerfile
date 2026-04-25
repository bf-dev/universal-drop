FROM rust:1-bookworm AS app-builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --locked --release

FROM debian:bookworm-slim AS whisper-builder
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates cmake g++ git libopenblas-dev make pkg-config \
    && rm -rf /var/lib/apt/lists/*
RUN git clone --depth 1 https://github.com/ggerganov/whisper.cpp.git /whisper.cpp
WORKDIR /whisper.cpp
RUN cmake -B build \
      -DWHISPER_BUILD_TESTS=OFF \
      -DWHISPER_BUILD_EXAMPLES=ON \
      -DBUILD_SHARED_LIBS=OFF \
      -DGGML_NATIVE=ON \
      -DGGML_BLAS=1 \
      -DGGML_BLAS_VENDOR=OpenBLAS \
    && cmake --build build --config Release -j"$(nproc)" \
    && mkdir -p /out \
    && install -m 0755 "$(find build -type f -name whisper-cli | head -n 1)" /out/whisper-cli

FROM debian:bookworm-slim AS page-orient-builder
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates g++ libopencv-dev pkg-config \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY tools/pdf-page-auto-orient.cpp ./pdf-page-auto-orient.cpp
RUN g++ -O3 -DNDEBUG -std=c++17 pdf-page-auto-orient.cpp \
      -o /usr/local/bin/pdf-page-auto-orient \
      $(pkg-config --cflags --libs opencv4) \
    && mkdir -p /out \
    && install -m 0755 /usr/local/bin/pdf-page-auto-orient /out/pdf-page-auto-orient

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
      ca-certificates \
      chromium \
      ffmpeg \
      libopenblas0-pthread \
      libopencv-core406 \
      libopencv-imgcodecs406 \
      libopencv-imgproc406 \
      libreoffice \
      pandoc \
      poppler-utils \
      python3 \
      python3-pip \
      tesseract-ocr \
      tesseract-ocr-eng \
    && python3 -m pip install --no-cache-dir --break-system-packages --upgrade yt-dlp \
    && rm -rf /var/lib/apt/lists/*

COPY --from=app-builder /app/target/release/universal-drop /usr/local/bin/universal-drop
COPY --from=whisper-builder /out/whisper-cli /usr/local/bin/whisper-cli
COPY --from=page-orient-builder /out/pdf-page-auto-orient /usr/local/bin/pdf-page-auto-orient

RUN mkdir -p /data/input /data/results /data/archive /models/whisper

ENV BIND_ADDR=0.0.0.0:8080 \
    INPUT_DIR=/data/input \
    RESULTS_DIR=/data/results \
    ARCHIVE_DIR=/data/archive \
    OLLAMA_BASE_URL=http://ollama:11434 \
    OLLAMA_MODEL=glm-ocr \
    OLLAMA_KEEP_ALIVE=30m \
    OLLAMA_NUM_THREAD=8 \
    GEMINI_OCR_ENABLED=true \
    GEMINI_API_KEY= \
    GEMINI_API_KEY_HEADER=Ocp-Apim-Subscription-Key \
    GEMINI_API_ENDPOINT=https://api.hku.hk/gemini/student/{deployment-id}:generateContent \
    GEMINI_DEPLOYMENT_ID=gemini-3-flash-preview \
    GEMINI_THINKING_BUDGET= \
    GEMINI_TIMEOUT_SECONDS=45 \
    WHISPER_CLI=whisper-cli \
    WHISPER_MODEL_PATH=/models/whisper/ggml-large-v3.bin \
    WHISPER_THREADS=8 \
    WHISPER_PROCESSORS=1 \
    WHISPER_BEAM_SIZE=1 \
    WHISPER_BEST_OF=1 \
    WHISPER_NO_FALLBACK=true \
    PDF_RENDER_DPI=150 \
    PDF_AUTO_ORIENT=true \
    PDF_AUTO_ORIENT_CLI=pdf-page-auto-orient \
    PDF_ORIENT_OCR_CONFIRM=true \
    PDF_ORIENT_OCR_CLI=tesseract \
    PDF_ORIENT_OCR_LANG=eng \
    PDF_ORIENT_OCR_MIN_CONFIDENCE=0.60 \
    PDF_ORIENT_OCR_MIN_SCORE=20 \
    URL_MAX_PER_TEXT=8 \
    YT_DLP_CLI=yt-dlp \
    HEADLESS_BROWSER_CLI=chromium \
    WEBPAGE_CAPTURE_VIRTUAL_TIME_MS=5000 \
    VIDEO_MIN_FRAMES=3 \
    VIDEO_MAX_FRAMES=24 \
    VIDEO_SCENE_THRESHOLD=0.35 \
    RUST_LOG=info

EXPOSE 8080
CMD ["universal-drop"]
