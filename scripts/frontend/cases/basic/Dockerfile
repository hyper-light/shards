FROM alpine:3.20
ARG VERSION=1.2.3
ENV APP_HOME=/app PATH=/app/bin:$PATH
WORKDIR $APP_HOME
COPY --chmod=0755 hello.sh bin/hello
COPY --chown=1000:1000 data/ data/
RUN echo "version $VERSION" > /app/version && adduser -D -u 1000 app
RUN ["/bin/sh", "-c", "ls -1 /app > /app/listing"]
USER app
LABEL org.opencontainers.image.title="basic" maintainer="shards"
EXPOSE 8080/tcp 53/udp
VOLUME /app/state
HEALTHCHECK --interval=30s --timeout=3s CMD ["/app/bin/hello"]
STOPSIGNAL SIGTERM
SHELL ["/bin/sh", "-e", "-c"]
ENTRYPOINT ["/app/bin/hello"]
CMD ["--help"]
