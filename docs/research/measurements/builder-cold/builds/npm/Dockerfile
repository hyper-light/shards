# A web application's dependencies installed as its users install them: npm ci of a
# lockfile, npm's cache a cache mount, which a round after the first reads back.
# Its base's registry, which measure.py's --base names where a host's anonymous pulls from
# this one are refused (ECR Public's data limit, on CI's runners).
ARG BASE=public.ecr.aws/docker/library
FROM ${BASE}/node:22-slim
WORKDIR /app
COPY package.json package-lock.json ./
RUN --mount=type=cache,target=/root/.npm npm ci --no-audit --no-fund
