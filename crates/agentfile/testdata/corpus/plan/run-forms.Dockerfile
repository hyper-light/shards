FROM alpine
SHELL ["/bin/bash", "-c"]
RUN echo shell form with bash
RUN ["exec", "form"]
RUN <<EOF
echo one
echo two
EOF
RUN <<'EOS' bash
set -e
echo quoted
EOS
RUN <<EOF
#!/usr/bin/env python3
print("shebang")
EOF
RUN cat <<A <<B
a
A
b
B
RUN <<-EOF
	#!/bin/sh
	echo chomped
	EOF
