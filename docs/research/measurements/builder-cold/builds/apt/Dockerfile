# A toolchain installed as images install one: apt's archives and lists as cache mounts
# (the docker-clean hook removed, as Docker's own docs say), which a round after the first
# reads back.
FROM public.ecr.aws/docker/library/debian:stable-slim
RUN rm -f /etc/apt/apt.conf.d/docker-clean
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt/lists,sharing=locked \
    apt-get update && apt-get install -y --no-install-recommends build-essential python3 python3-venv git curl ca-certificates
