FROM scratch AS src
ADD file.txt /

FROM alpine
RUN true
