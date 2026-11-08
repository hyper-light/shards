FROM alpine
COPY . /src
COPY a.txt /a
RUN ls /src
