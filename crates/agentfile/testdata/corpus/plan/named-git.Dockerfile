FROM alpine
COPY --from=repo /README.md /r
