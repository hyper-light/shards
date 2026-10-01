ARG BASE=alpine
ARG V
# base the base stage
FROM $BASE AS base
RUN echo
# other
from base as Next
COPY --from=base /a /b
FROM --platform=$BUILDPLATFORM scratch AS Final
ARG X=1 Y
ENV A=1 B=2
ENV legacy value here
LABEL a=b c="d e"
WORKDIR /x
USER app:app
VOLUME /v1 /v2
VOLUME ["/v3", " /v4 "]
STOPSIGNAL SIGTERM
EXPOSE 80/tcp 443 8080/UDP 1.2.3.4:80:80
SHELL ["/bin/sh","-c"]
CMD echo hi
CMD ["a","b"]
ENTRYPOINT ["e"]
ENTRYPOINT
MAINTAINER someone
