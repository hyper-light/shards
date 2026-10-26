# A Rust crate built as its users build it: cargo's registry and its target directory as
# cache mounts, which a round after the first reads back through the builder's join disk
# (D134), as a second build of a project does.
# Its base's registry, which measure.py's --base names where a host's anonymous pulls from
# this one are refused (ECR Public's data limit, on CI's runners).
ARG BASE=public.ecr.aws/docker/library
FROM ${BASE}/rust:1-slim
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/cache/target \
    CARGO_TARGET_DIR=/cache/target cargo install ripgrep --version 14.1.1 --locked --root /out
