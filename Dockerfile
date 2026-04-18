FROM rust:1-bookworm AS app-builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --locked --release

FROM debian:bookworm-slim AS whisper-builder
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates cmake g++ git make pkg-config \
    && rm -rf /var/lib/apt/lists/*
RUN git clone --depth 1 https://github.com/ggerganov/whisper.cpp.git /whisper.cpp
WORKDIR /whisper.cpp
RUN cmake -B build \
      -DWHISPER_BUILD_TESTS=OFF \
      -DWHISPER_BUILD_EXAMPLES=ON \
      -DGGML_NATIVE=OFF \
    && cmake --build build --config Release -j"$(nproc)" \
    && mkdir -p /out \
    && install -m 0755 "$(find build -type f -name whisper-cli | head -n 1)" /out/whisper-cli

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
      ca-certificates \
      ffmpeg \
      libreoffice \
      pandoc \
      poppler-utils \
    && rm -rf /var/lib/apt/lists/*

COPY --from=app-builder /app/target/release/universal-drop /usr/local/bin/universal-drop
COPY --from=whisper-builder /out/whisper-cli /usr/local/bin/whisper-cli

RUN mkdir -p /data/input /data/results /data/archive /models/whisper

ENV BIND_ADDR=0.0.0.0:8080 \
    INPUT_DIR=/data/input \
    RESULTS_DIR=/data/results \
    ARCHIVE_DIR=/data/archive \
    OLLAMA_BASE_URL=http://ollama:11434 \
    OLLAMA_MODEL=glm-ocr \
    OLLAMA_KEEP_ALIVE=5m \
    WHISPER_CLI=whisper-cli \
    WHISPER_MODEL_PATH=/models/whisper/ggml-small.bin \
    RUST_LOG=info

EXPOSE 8080
CMD ["universal-drop"]
