FROM a
ADD --chown=1:1 --chmod=0755 --link --keep-git-dir --checksum=sha256:00 --unpack=false --exclude=*.md --exclude=x a b /dst/
COPY --from=st --chown=u --chmod=u+x --link=false --parents --exclude=y s1 s2 /d
COPY <<EOF <<-T /dest
hello $X
EOF
	world
	T
ADD --unpack ["x", "y"]
COPY --link=TRUE a b
