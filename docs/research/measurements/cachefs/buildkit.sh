#!/bin/sh
# What Docker 29.3.1's own BuildKit (v0.28.1) does with `RUN --mount=type=cache`
# (platform-measurements.md M141, D114): what a cache keeps across builds, a failing step's
# writes among it; `uid`, `gid` and `mode`; a root a step gives another owner; another
# namespace; one id twice in a step; `from`/`source`; concurrent builds under each sharing
# mode; the records as `buildx du`, `system df` and `buildx prune` show and remove them;
# the builder's GC policy; and a copy kept when a file it does not copy changes.
#
# Run against the shards-dind container (docker:29.3.1-dind). Every name holds this
# run's token, every build writes no image (`--output type=cacheonly`), and only records
# whose description holds the token are pruned.
#
#   docs/research/measurements/cachefs/buildkit.sh > buildkit.out
#
# An exec's key leaves out its cache mounts' id and sharing, so each mode's steps name
# the mode, or a mode's builds would be answered from another's. No build but P6b's
# passes --no-cache: BuildKit lets go of the caches of a step not
# answered from the cache before the build runs (llbsolver detectPrunedCacheID), so a
# --no-cache probe of what a cache keeps would find it empty every time.
set -eu
box=${CONTAINER:-shards-dind}
docker exec "$box" docker version --format '{{.Server.Version}}' | grep -qx '29.3.1' || {
	echo "$box is not Docker 29.3.1" >&2
	exit 1
}
t=forkt$(date +%s)
alpine=alpine@sha256:5291449c3df73caf6ed85e649dec1b9e818b39a5d8c871e97afc13e9cd5e8fa8
dind() { docker exec -i "$box" sh -c "$1"; }
# Builds the Dockerfile on stdin, with a context of files given as NAME=TEXT, in a
# directory of the run's own in the container.
build() {
	name=$1
	shift
	files=""
	for f in "$@"; do
		case $f in
		--*) ;;
		*=*) files="$files printf '%s' '${f#*=}' > '${f%%=*}';" ;;
		esac
	done
	args=""
	for f in "$@"; do
		case $f in --*) args="$args $f" ;; esac
	done
	dind "d=/tmp/$t-$name; mkdir -p \$d; cd \$d; cat > Dockerfile; $files docker buildx build --progress=plain --output type=cacheonly $args . 2>&1; echo \"exit \$?\""
}
echo "== token $t"

echo "== P1 kept across builds, a failing step's writes too"
printf 'FROM %s\nRUN --mount=type=cache,target=/c,id=%s-p sh -c "echo one > /c/one; : %s"\n' "$alpine" "$t" "$t" | build p1 | grep -E "exit|ERROR" || true
printf 'FROM %s\nRUN --mount=type=cache,target=/c,id=%s-p sh -c "echo two > /c/two; exit 1"\n' "$alpine" "$t" | build p2 | grep -E "exit" || true
printf 'FROM %s\nRUN --mount=type=cache,target=/c,id=%s-p ls /c\n' "$alpine" "$t" | build p3 | grep -E "^#[0-9]+ [0-9.]+ |exit" || true

echo "== P2 uid gid mode"
printf 'FROM %s\nRUN --mount=type=cache,target=/m,id=%s-m,uid=1000,gid=1001,mode=0750 stat -c "%%a %%u:%%g" /m\n' "$alpine" "$t" | build p4 | grep -E "^#[0-9]+ [0-9.]+ [0-9]|exit" || true
printf 'FROM %s\nRUN --mount=type=cache,target=/m2,id=%s-m2,mode=0700 stat -c "%%a %%u:%%g" /m2\n' "$alpine" "$t" | build p4b | grep -E "^#[0-9]+ [0-9.]+ [0-9]|exit" || true
printf 'FROM %s\nRUN --mount=type=cache,target=/m3,id=%s-m3 stat -c "%%a %%u:%%g" /m3\n' "$alpine" "$t" | build p4c | grep -E "^#[0-9]+ [0-9.]+ [0-9]|exit" || true

echo "== P3 a root given another owner and mode"
printf 'FROM %s\nRUN --mount=type=cache,target=/r,id=%s-r sh -c "chmod 700 /r && chown 7:8 /r"\n' "$alpine" "$t" | build p5 | grep -E "exit" || true
printf 'FROM %s\nRUN --mount=type=cache,target=/r,id=%s-r stat -c "%%a %%u:%%g" /r\n' "$alpine" "$t" | build p6 | grep -E "^#[0-9]+ [0-9.]+ [0-9]|exit" || true

echo "== P4 another namespace"
printf 'FROM %s\nRUN --mount=type=cache,target=/c,id=%s-p ls -A /c; echo listed\n' "$alpine" "$t" | build p7 "--build-arg=BUILDKIT_CACHE_MOUNT_NS=$t-ns" | grep -E "^#[0-9]+ [0-9.]+ |exit" || true

echo "== P5 one id twice in one step"
printf 'FROM %s\nRUN --mount=type=cache,target=/x,id=%s-two --mount=type=cache,target=/y,id=%s-two,ro sh -c "echo 3 > /x/f; cat /y/f"\n' "$alpine" "$t" "$t" | build p8 | grep -E "^#[0-9]+ [0-9.]+ |exit" || true

echo "== P6 from and source"
printf 'FROM %s AS s\nRUN mkdir -p /seed/sub && echo seeded > /seed/sub/f && echo top > /seed/top && chown -R 5:6 /seed\nFROM %s\nRUN --mount=type=cache,target=/c,id=%s-from,from=s,source=/seed sh -c "ls -lnR /c; stat -c \\"%%a %%u:%%g\\" /c"\n' "$alpine" "$alpine" "$t" | build p9 | grep -E "^#[0-9]+ [0-9.]+ |exit" || true
printf 'FROM %s AS s\nRUN mkdir -p /seed/sub && echo seeded > /seed/sub/f && echo top > /seed/top && chown -R 5:6 /seed\nFROM %s\nRUN --mount=type=cache,target=/c,id=%s-from,from=s,source=/seed/sub sh -c "ls -lnR /c"\n' "$alpine" "$alpine" "$t" | build p10 | grep -E "^#[0-9]+ [0-9.]+ |exit" || true

echo "== P6b a step not answered from the cache (--no-cache): its caches"
printf 'FROM %s\nRUN --mount=type=cache,target=/c,id=%s-nc sh -c "echo n > /c/n; : %s"\n' "$alpine" "$t" "$t" | build p11 | grep -E "exit" || true
printf 'FROM %s\nRUN --mount=type=cache,target=/c,id=%s-nc sh -c "ls -A /c; echo listed; : %s"\n' "$alpine" "$t" "$t" | build p12 --no-cache | grep -E "^#[0-9]+ [0-9.]+ |exit" || true
printf 'FROM %s\nRUN --mount=type=cache,target=/c,id=%s-nc sh -c "ls -A /c; echo after; : %s"\n' "$alpine" "$t" "$t" | build p13 | grep -E "^#[0-9]+ [0-9.]+ |exit" || true

for sharing in shared private locked; do
	echo "== P7 concurrent builds, sharing=$sharing"
	printf 'FROM %s\nRUN --mount=type=cache,target=/c,id=%s-%s,sharing=%s sh -c "date +%%s.A.start; echo a > /c/a; sleep 8; ls /c; date +%%s.A.end; : %s %s"\n' "$alpine" "$t" "$sharing" "$sharing" "$t" "$sharing" | build "c-$sharing-a" > "/tmp/$t-$sharing-a.log" &
	sleep 3
	printf 'FROM %s\nRUN --mount=type=cache,target=/c,id=%s-%s,sharing=%s sh -c "date +%%s.B.start; echo b > /c/b; sleep 1; ls /c; date +%%s.B.end; : %s %s"\n' "$alpine" "$t" "$sharing" "$sharing" "$t" "$sharing" | build "c-$sharing-b" > "/tmp/$t-$sharing-b.log" &
	wait
	for side in a b; do
		grep -E "^#[0-9]+ [0-9.]+ [0-9A-Za-z.]+$|exit" "/tmp/$t-$sharing-$side.log" | sed "s/^/$side: /" || true
		rm -f "/tmp/$t-$sharing-$side.log"
	done
	# Which record a build takes once both are done.
	printf 'FROM %s\nRUN --mount=type=cache,target=/c,id=%s-%s,sharing=%s sh -c "ls /c; : %s %s after"\n' "$alpine" "$t" "$sharing" "$sharing" "$t" "$sharing" | build "c-$sharing-c" | grep -E "^#[0-9]+ [0-9.]+ [0-9A-Za-z.]+$" | sed 's/^/then: /' || true
done

echo "== P8 the records: du, system df"
dind "docker buildx du --verbose --filter description~=$t" || true
dind "docker system df -v --format '{{range .BuildCache}}{{.ID}} {{.CacheType}} {{.Size}} {{.Shared}} {{.InUse}} {{.Description}}{{println}}{{end}}'" | grep -E "$t" || true

echo "== P9 copies keyed by what they read"
printf 'FROM %s\nCOPY a.txt /a\nRUN echo %s-copy && cat /a\n' "$alpine" "$t" | build k1 a.txt=a c.txt=c | grep -E "CACHED|COPY|exit" || true
printf 'FROM %s\nCOPY a.txt /a\nRUN echo %s-copy && cat /a\n' "$alpine" "$t" | build k1 a.txt=a c.txt=changed | grep -E "CACHED|COPY|exit" || true

echo "== P10 the GC policy"
dind "docker buildx inspect" | grep -iE "gc|keep|policy|filter|space|duration" || true

echo "== P11 prune: the cache mounts, then everything else of this run's"
dind "docker buildx prune -f --filter type=exec.cachemount --filter description~=$t" || true
dind "docker buildx prune -f --verbose --all --filter description~=$t" | head -40 || true
dind "rm -rf /tmp/$t-*"
