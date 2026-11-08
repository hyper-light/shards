FROM alpine
ENV A=1 B="two words"
ENV legacy value
LABEL l1=v1 "l 2"=v2
USER app
EXPOSE 80 443/udp
VOLUME /v
STOPSIGNAL SIGTERM
SHELL ["/bin/bash","-c"]
CMD echo hi
ENTRYPOINT ["/e"]
HEALTHCHECK --interval=5s CMD ["true"]
ONBUILD RUN echo later
MAINTAINER me
