#!/bin/sh
# Inside shards-dind: builds each case to an OCI layout with its metadata file, and
# prints, for `generate` to gather, each case's name, its Dockerfile and flags, and what
# the build recorded: records of `KEY base64(VALUE)`, a case ended by `END`.
set -eu
work=$(mktemp -d /tmp/shards-provenance-XXXXXX)
trap 'rm -rf "$work"' EXIT
cd "$work"
echo hi >a
ssh-keygen -q -t ed25519 -N '' -f "$work/key"
printf 'secret value\n' >"$work/secret.txt"

b64() { base64 -w0 "$1" 2>/dev/null || base64 "$1" | tr -d '\n'; }
say() { printf '%s %s\n' "$1" "$(printf '%s' "$2" | base64 | tr -d '\n')"; }

# case_ NAME DOCKERFILE FLAGS...
case_() {
	name=$1
	dockerfile=$2
	shift 2
	dir="$work/$name"
	mkdir -p "$dir"
	cp a "$dir/a"
	printf '%b' "$dockerfile" >"$dir/Dockerfile"
	(cd "$dir" && docker buildx build -q "$@" --metadata-file "$dir/meta.json" -o "type=oci,dest=$dir/out.tar" . >/dev/null 2>"$dir/err") || {
		cat "$dir/err" >&2
		exit 1
	}
	mkdir "$dir/out"
	tar -xf "$dir/out.tar" -C "$dir/out"
	say NAME "$name"
	printf 'DOCKERFILE %s\n' "$(b64 "$dir/Dockerfile")"
	for f in "$@"; do say FLAG "$f"; done
	printf 'LAYOUT %s\n' "$(b64 "$dir/out/index.json")"
	printf 'METADATA %s\n' "$(b64 "$dir/meta.json")"
	for f in "$dir/out/blobs/sha256"/*; do
		printf 'BLOB %s %s\n' "$(basename "$f")" "$(b64 "$f")"
	done
	echo END
}

# An OCI layout is given no attestation unless one is asked for.
case_ scratch-default 'FROM scratch\nCOPY a /a\n'
case_ scratch-min 'FROM scratch\nCOPY a /a\n' --provenance=mode=min
case_ args 'ARG V\nFROM scratch AS out\nARG W\nCOPY a /a\nLABEL x=y\n' --provenance=mode=min \
	--build-arg V=1 --build-arg W=2 --label l=m --target out --add-host h:10.0.0.1 \
	--shm-size 64m --ulimit nofile=1024:2048 --network none --no-cache \
	--cgroup-parent shards --platform linux/arm64
case_ mounts 'FROM busybox:1.36\nRUN --mount=type=secret,id=s --mount=type=secret,id=opt,required=false --mount=type=ssh true\n' \
	--provenance=mode=min --secret "id=s,src=$work/secret.txt" --ssh "default=$work/key"
case_ base 'FROM busybox:1.36\nCOPY a /a\n' --provenance=mode=min
case_ http 'FROM scratch\nADD https://raw.githubusercontent.com/moby/buildkit/v0.28.1/README.md /r\n' --provenance=mode=min
case_ git 'FROM scratch\nADD https://github.com/moby/buildkit.git#v0.28.1:docs/attestations /d\n' --provenance=mode=min
case_ base-registry 'FROM gcr.io/distroless/static-debian12:nonroot\nCOPY a /a\n' --provenance=mode=min
case_ base-digest 'FROM busybox@sha256:73aaf090f3d85aa34ee199857f03fa3a95c8ede2ffd4cc2cdb5b94e566b11662\nCOPY a /a\n' --provenance=mode=min
case_ base-untagged 'FROM busybox\nCOPY a /a\n' --provenance=mode=min
case_ named-context 'FROM scratch\nCOPY --from=extra a /b\n' --provenance=mode=min --build-context "extra=$work"
case_ filename 'FROM scratch\nCOPY a /a\n' --provenance=mode=min -f Dockerfile
# mode=max: the LLB definition, its source map and each step's layers (D80).
case_ max-base 'FROM busybox:1.36\nCOPY a /a\nRUN echo x > /b\n' --provenance=mode=max
case_ max-mounts 'FROM busybox:1.36\nRUN --mount=type=secret,id=s --mount=type=ssh --mount=type=cache,target=/c,sharing=locked --mount=type=tmpfs,target=/t,size=64m --network=none true\n' \
	--provenance=mode=max --secret "id=s,src=$work/secret.txt" --ssh "default=$work/key" --build-arg A=1 --label l=m
case_ max-stages 'FROM busybox:1.36 AS b\nRUN echo x > /x\nFROM scratch\nWORKDIR /w\nCOPY --from=b /x /y\nCOPY --link a /l\nCOPY --chown=1:2 a /o\nCOPY <<EOF /h\nhello\nEOF\n' \
	--provenance=mode=max
# SBOMs (D81): the scanner run over the image, and over a stage and the context.
case_ sbom-base 'FROM busybox:1.36\nCOPY a /a\n' --sbom=true
case_ sbom-extras 'FROM busybox:1.36 AS deps\nARG BUILDKIT_SBOM_SCAN_STAGE=true\nRUN echo x > /x\nFROM busybox:1.36\nARG BUILDKIT_SBOM_SCAN_CONTEXT=true\nCOPY --from=deps /x /x\nCOPY a /a\n' --sbom=true --provenance=mode=min
# A base image from a registry with a port: a registry of the run's own, inside.
docker run -d --rm --name shards-provenance-registry -p 127.0.0.1:5000:5000 registry:2 >/dev/null
trap 'docker rm -f shards-provenance-registry >/dev/null 2>&1; rm -rf "$work"' EXIT
docker pull -q busybox:1.36 >/dev/null
docker tag busybox:1.36 localhost:5000/team/busybox:1.36
for _ in 1 2 3 4 5 6 7 8 9 10; do docker push -q localhost:5000/team/busybox:1.36 >/dev/null 2>&1 && break; sleep 1; done
docker image rm localhost:5000/team/busybox:1.36 >/dev/null
case_ base-port 'FROM localhost:5000/team/busybox:1.36\nCOPY a /a\n' --provenance=mode=min
case_ oci-named 'FROM scratch\nCOPY a /a\n' --provenance=mode=min -t shards-provenance-probe:oci
case_ provenance-false 'FROM scratch\nCOPY a /a\n' --provenance=false
case_ attest 'FROM scratch\nCOPY a /a\n' --attest type=provenance,mode=min

# A local output given a provenance: the files it holds.
dir="$work/local"
mkdir -p "$dir/out"
cp a "$dir/a"
printf 'FROM scratch\nCOPY a /a\n' >"$dir/Dockerfile"
(cd "$dir" && docker buildx build -q --provenance=mode=min -o "type=local,dest=$dir/out" . >/dev/null)
say NAME local-output
printf 'DOCKERFILE %s\n' "$(b64 "$dir/Dockerfile")"
say FLAG "--provenance=mode=min"
say FILES "$(cd "$dir/out" && find . | sort | tr '\n' ' ')"
[ -f "$dir/out/provenance.json" ] && printf 'LOCALPROV %s\n' "$(b64 "$dir/out/provenance.json")"
echo END

# A local output given an SBOM: the files it holds.
dir="$work/local-sbom"
mkdir -p "$dir/out"
cp a "$dir/a"
printf 'FROM busybox:1.36\nCOPY a /a\n' >"$dir/Dockerfile"
(cd "$dir" && docker buildx build -q --sbom=true -o "type=local,dest=$dir/out" . >/dev/null)
say NAME sbom-local
printf 'DOCKERFILE %s\n' "$(b64 "$dir/Dockerfile")"
say FLAG "--sbom=true"
say FILES "$(cd "$dir/out" && find . -maxdepth 1 | sort | tr '\n' ' ')"
[ -f "$dir/out/provenance.json" ] && printf 'LOCALPROV %s\n' "$(b64 "$dir/out/provenance.json")"
[ -f "$dir/out/sbom.spdx.json" ] && printf 'LOCALSBOM %s\n' "$(b64 "$dir/out/sbom.spdx.json")"
echo END

# A build stored with no flag: the index its name resolves to, as containerd keeps it.
store=/var/lib/docker/containerd/daemon/io.containerd.content.v1.content/blobs/sha256
dir="$work/stored"
mkdir -p "$dir"
cp a "$dir/a"
printf 'FROM scratch\nCOPY a /a\n' >"$dir/Dockerfile"
tag=shards-provenance-probe:stored
(cd "$dir" && docker buildx build -q -t "$tag" --metadata-file "$dir/meta.json" . >/dev/null)
id=$(docker image inspect "$tag" --format '{{.Id}}')
say NAME stored-default
printf 'DOCKERFILE %s\n' "$(b64 "$dir/Dockerfile")"
say FLAG "-t"
say FLAG "$tag"
say ID "$id"
printf 'METADATA %s\n' "$(b64 "$dir/meta.json")"
idx="$store/${id#sha256:}"
printf 'BLOB %s %s\n' "${id#sha256:}" "$(b64 "$idx")"
for d in $(sed -n 's/.*"digest": "sha256:\([0-9a-f]*\)".*/\1/p' "$idx"); do
	printf 'BLOB %s %s\n' "$d" "$(b64 "$store/$d")"
	for e in $(sed -n 's/.*"digest": "sha256:\([0-9a-f]*\)".*/\1/p' "$store/$d"); do
		[ -f "$store/$e" ] && printf 'BLOB %s %s\n' "$e" "$(b64 "$store/$e")"
	done
done
echo END
docker image rm "$tag" >/dev/null
