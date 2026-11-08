FROM alpine
COPY a b /dst/
COPY --chown=1:2 --chmod=0644 c /f
ADD d /g
COPY <<EOF /inline
hello
EOF
