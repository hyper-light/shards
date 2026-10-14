FROM a
ONBUILD RUN echo hi
onbuild  COPY a b
ONBUILD ADD <<EOF /x
content
EOF
