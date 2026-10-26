# A web application's dependencies installed as its users install them: npm ci of a
# lockfile, npm's cache a cache mount, which a round after the first reads back.
FROM public.ecr.aws/docker/library/node:22-slim
WORKDIR /app
COPY package.json package-lock.json ./
RUN --mount=type=cache,target=/root/.npm npm ci --no-audit --no-fund
