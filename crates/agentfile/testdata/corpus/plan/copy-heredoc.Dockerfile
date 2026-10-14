FROM alpine
ARG NAME=world
COPY <<EOF /greet
hello $NAME
EOF
COPY <<-"EOT" /raw
	no ${NAME} expansion
	EOT
COPY <<A <<B /two/
first
A
second
B
