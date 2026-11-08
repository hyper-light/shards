FROM alpine
RUN <<EOF
echo a
echo b
EOF
RUN <<EOF
#!/bin/sh
echo shebang
EOF
RUN cat <<A <<B
first
A
second
B
