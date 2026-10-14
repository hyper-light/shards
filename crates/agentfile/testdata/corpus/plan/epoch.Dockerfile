FROM alpine
WORKDIR /w
COPY a /a
ADD b /b
RUN true
