FROM alpine
RUN --mount=type=bind,from=later,target=/l ls
FROM alpine AS later
