FROM alpine
RUN --mount=type=cache,target=/ ls
