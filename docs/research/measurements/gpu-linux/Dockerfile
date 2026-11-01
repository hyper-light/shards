# E6's image (../venus/Dockerfile, D144), built with 8 compiler jobs where E6's has one a
# CPU: on the GPU desktop's 32, the builder was killed for its memory as cc1plus's 32
# grew past what it was given in time (PM M384), which is the builder's to fix (D133),
# not this measurement's. Without blobs.c, which M384 does not run.
FROM debian:trixie-slim
RUN apt-get update -q && apt-get install -y -q --no-install-recommends \
    build-essential ca-certificates cmake curl git glslc libvulkan-dev mesa-vulkan-drivers \
    spirv-headers vulkan-tools
# llama.cpp v0.6.0.
ARG LLAMA_CPP=8345f333951c661d166b00e6f9362e553768f292
RUN git init -q /llama.cpp && cd /llama.cpp \
    && git fetch -q --depth 1 https://github.com/ggml-org/llama.cpp "$LLAMA_CPP" && git checkout -q FETCH_HEAD \
    && cmake -B build -DGGML_VULKAN=ON -DLLAMA_CURL=OFF -DCMAKE_BUILD_TYPE=Release >/dev/null \
    && cmake --build build -j 8 --target llama-bench test-backend-ops >/dev/null
# TinyLlama 1.1B, Q4_0, as TheBloke published it.
RUN curl -fsSL -o /model.gguf https://huggingface.co/TheBloke/TinyLlama-1.1B-Chat-v1.0-GGUF/resolve/main/tinyllama-1.1b-chat-v1.0.Q4_0.gguf \
    && echo "da3087fb14aede55fde6eb81a0e55e886810e43509ec82ecdc7aa5d62a03b556  /model.gguf" | sha256sum -c -
