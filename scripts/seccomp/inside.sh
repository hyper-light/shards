#!/bin/sh
# Inside the generator's container (scripts/seccomp/generate): builds libseccomp 2.5.4,
# runc v1.3.4's seccomp packages with an export in place of the load, and the oracle,
# then records each case for this architecture ($1, amd64 or arm64).
set -eu
arch=$1
libseccomp=2.5.4
libseccomp_sha256=d82902400405cf0068574ef3dc1fe5f5926207543ba1ae6f8e7a1576351dcbdb
runc=v1.3.4

apt-get update -qq >/dev/null
apt-get install -y -qq --no-install-recommends gperf >/dev/null
cd /tmp
curl -sfL "https://github.com/seccomp/libseccomp/releases/download/v$libseccomp/libseccomp-$libseccomp.tar.gz" -o libseccomp.tar.gz
echo "$libseccomp_sha256  libseccomp.tar.gz" | sha256sum -c - >/dev/null
tar xzf libseccomp.tar.gz
(cd "libseccomp-$libseccomp" && ./configure --quiet --prefix=/usr/local --disable-shared >/dev/null && make -s -j"$(nproc)" >/dev/null && make -s install >/dev/null)

git clone -q --depth 1 --branch "$runc" https://github.com/opencontainers/runc /tmp/runc 2>/dev/null
cp /work/runc-export/export_seccomp.go /tmp/runc/libcontainer/seccomp/
cp /work/runc-export/export_patchbpf.go /tmp/runc/libcontainer/seccomp/patchbpf/

cp -R /work/oracle /tmp/oracle
cd /tmp/oracle
go mod edit -replace "github.com/opencontainers/runc=/tmp/runc"
GOFLAGS=-mod=mod go mod tidy >/dev/null 2>&1
CGO_ENABLED=1 PKG_CONFIG_PATH=/usr/local/lib/pkgconfig \
	go build -tags seccomp -o /tmp/oracle-bin .
/tmp/oracle-bin "$arch" "/tmp/libseccomp-$libseccomp/src/syscalls.csv" /work/cases.json \
	"/work/oracle-$arch.json" "/work/syscalls-$arch.json"
