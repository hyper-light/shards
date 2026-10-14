
FROM alpine:3.6

ENV NAME=me

RUN ls

USER <<INVALID
INVALID

RUN <<EMPTY
EMPTY

RUN 3<<EMPTY2
EMPTY2

RUN "<<NOHEREDOC"

RUN <<INDENT
	foo
	bar
INDENT

RUN <<-UNINDENT
	baz
	quux
UNINDENT

RUN <<-UNINDENT2
	baz
	quux
	UNINDENT2

RUN <<-EXPAND
	expand $NAME
EXPAND

RUN <<-'NOEXPAND'
	don't expand $NAME
NOEXPAND

RUN <<COPY
echo hello world
echo foo bar
COPY

RUN <<COMMENT
# internal comment
echo hello world
echo foo bar # trailing comment
COMMENT

RUN --mount=type=cache,target=/foo <<MOUNT
echo hello
MOUNT

COPY <<FILE1 <<FILE2 /dest
content 1
FILE1
content 2
FILE2

COPY <<EOF /quotes
"foo"
'bar'
EOF

COPY <<X <<Y /dest
Y
X
X
Y

RUN <<COMPLEX python3
print('hello world')
COMPLEX

COPY <<file.txt /dest
hello world
file.txt

RUN <<eo'f'
echo foo
eof

RUN <<eo\'f
echo foo
eo'f

RUN <<'e'o\'f
echo foo
eo'f

RUN <<'one two'
echo bar
one two

RUN <<$EOF
$EOF

RUN <<  EOF
EOF

RUN <<  EOF  > foo
EOF
	