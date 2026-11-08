FROM alpine
ARG HTTP_PROXY
RUN curl example.com
