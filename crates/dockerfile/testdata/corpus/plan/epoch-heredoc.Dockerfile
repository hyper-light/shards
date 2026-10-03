FROM scratch AS src
ADD <<EOF /a
hello
EOF

FROM alpine
RUN true
