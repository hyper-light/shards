# A toolchain installed as images install one: apt's archives and lists as cache mounts
# (the docker-clean hook removed, as Docker's own docs say), which a round after the first
# reads back.
# Its base's registry, which measure.py's --base names where a host's anonymous pulls from
# this one are refused (ECR Public's data limit, on CI's runners).
ARG BASE=public.ecr.aws/docker/library
FROM ${BASE}/debian:stable-slim
# Its steps run again in each round, which ROUND names (measure.py), their cache mounts
# kept: --no-cache would have each start its caches empty (PM M141).
ARG ROUND
RUN rm -f /etc/apt/apt.conf.d/docker-clean
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt/lists,sharing=locked \
    apt-get update && apt-get install -y --no-install-recommends build-essential python3 python3-venv git curl ca-certificates
