FROM alpine
USER nobody
WORKDIR /home/nobody
EXPOSE 80 443/tcp 53/UDP 8000-8002 127.0.0.1:9000:9000
VOLUME /data ["/logs", "/cache"]
STOPSIGNAL SIGINT
HEALTHCHECK --interval=5s --timeout=1m30s --start-period=500ms --start-interval=2s --retries=4 CMD curl -f http://localhost/ || exit 1
LABEL a=1 "b c"=2
LABEL legacy value
ENV legacy value
MAINTAINER someone@example.com
ENTRYPOINT ["/entry"]
CMD ["arg"]
