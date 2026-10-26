# A Rust crate built as its users build it: cargo's registry and its target directory as
# cache mounts, which a round after the first reads back through the builder's join disk
# (D134), as a second build of a project does.
FROM public.ecr.aws/docker/library/rust:1-slim
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/cache/target \
    CARGO_TARGET_DIR=/cache/target cargo install ripgrep --version 14.1.1 --locked --root /out
