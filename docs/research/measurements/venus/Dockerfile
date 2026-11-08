# E6 (docs/research/gpu-accelerators.md): llama.cpp's Vulkan backend, a model, and the
# blob harness, for a guest given this Mac's GPU through Venus (D144). run.sh builds it.
FROM debian:trixie-slim
RUN apt-get update -q && apt-get install -y -q --no-install-recommends \
    build-essential ca-certificates cmake curl git glslc libvulkan-dev mesa-vulkan-drivers \
    spirv-headers vulkan-tools
# llama.cpp v0.6.0.
ARG LLAMA_CPP=8345f333951c661d166b00e6f9362e553768f292
RUN git init -q /llama.cpp && cd /llama.cpp \
    && git fetch -q --depth 1 https://github.com/ggml-org/llama.cpp "$LLAMA_CPP" && git checkout -q FETCH_HEAD \
    && cmake -B build -DGGML_VULKAN=ON -DLLAMA_CURL=OFF -DCMAKE_BUILD_TYPE=Release >/dev/null \
    && cmake --build build -j --target llama-bench test-backend-ops >/dev/null
# TinyLlama 1.1B, Q4_0, as TheBloke published it.
RUN curl -fsSL -o /model.gguf https://huggingface.co/TheBloke/TinyLlama-1.1B-Chat-v1.0-GGUF/resolve/main/tinyllama-1.1b-chat-v1.0.Q4_0.gguf \
    && echo "da3087fb14aede55fde6eb81a0e55e886810e43509ec82ecdc7aa5d62a03b556  /model.gguf" | sha256sum -c -
COPY blobs.c /blobs.c
RUN cc -O2 -o /blobs /blobs.c -lvulkan
