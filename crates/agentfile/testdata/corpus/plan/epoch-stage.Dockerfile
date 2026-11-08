FROM scratch AS src
ARG V=1.0
ARG SOURCE_DATE_EPOCH
ADD --checksum=sha256:24454f830cdb571e2c4ad15481119c43b3cafd48dd869a9b2945d1036d1dc68d https://example.com/releases/app-${V}.tar.gz /

FROM alpine
ARG SOURCE_DATE_EPOCH
RUN echo "[$SOURCE_DATE_EPOCH]"
