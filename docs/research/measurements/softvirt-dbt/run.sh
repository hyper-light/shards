#!/bin/sh
# softvirt-dbt/run.sh OUT BASELINES: what a binary translator costs a guest, on the host's
# own architecture: softvirt-baselines' workload (its compute mode too) run natively,
# under QEMU's TCG in user mode (qemu-x86_64: translated user code, host system calls),
# under TCG in full-system mode (shards' pinned kernel, the workload its init: a software
# MMU too), and in a KVM guest under Firecracker, interleaved for ROUNDS rounds (default
# 3), pinned to CPUS when set. Takes Firecracker, the pinned kernel and gen_init_cpio from
# BASELINES (run softvirt-baselines/run.sh first). QEMU comes from Alpine's packages, in
# an image of this script's own name, built here and removed at the end. Needs zig 0.16,
# docker, taskset.
set -eu
out=$(mkdir -p "$1" && cd "$1" && pwd)
base=$(cd "$2" && pwd)
here=$(cd "$(dirname "$0")" && pwd)
arch=$(uname -m)
n=${N:-20000}
rounds=${ROUNDS:-3}
cpus=${CPUS:-}
image=softvirt-dbt-qemu:$$

pin() {
    if [ -n "$cpus" ]; then taskset -c "$cpus" "$@"; else "$@"; fi
}

host() {
    echo "- host: $(uname -srm); $(grep -m1 -E 'model name|CPU part' /proc/cpuinfo | cut -d: -f2 | sed 's/^ //')"
    echo "- load: $(cut -d' ' -f1-3 /proc/loadavg); pinned to ${cpus:-none}; revision ${REVISION:-$(git -C "$here" rev-parse --short HEAD 2>/dev/null || echo unknown)}"
}

zig cc -target "$arch-linux-musl" -O2 -static -o "$out/workload" "$here/../softvirt-baselines/workload.c"
printf 'dir /dev 0755 0 0\nnod /dev/console 0600 0 0 c 5 1\nfile /init %s 0755 0 0\n' "$out/workload" \
    > "$out/initramfs.list"
"$(ls -d "$base"/uml/linux-*/usr/gen_init_cpio | head -1)" "$out/initramfs.list" > "$out/initramfs.cpio"
cat > "$out/firecracker.json" <<EOF
{"boot-source": {"kernel_image_path": "$base/kvm/vmlinux", "initrd_path": "$out/initramfs.cpio",
  "boot_args": "console=ttyS0 reboot=k panic=1 rdinit=/init quiet softvirt.restart"},
 "machine-config": {"vcpu_count": 2, "mem_size_mib": 1024}}
EOF

printf 'FROM alpine:latest\nRUN apk add --no-cache qemu-x86_64 qemu-aarch64 qemu-system-x86_64 qemu-system-aarch64 >/dev/null\n' |
    docker build -q -t "$image" - >/dev/null
trap 'docker rmi -f "$image" >/dev/null 2>&1 || true' EXIT
qemu_version=$(docker run --rm "$image" qemu-system-$arch --version | head -1)
docker_cpus=${cpus:+--cpuset-cpus=$cpus}

r=1
while [ "$r" -le "$rounds" ]; do
    { host; pin "$out/workload" -n "$n"; } > "$out/native-r$r.md" 2>&1
    {
        host
        echo "- $qemu_version, user mode (qemu-$arch), TCG"
        docker run --rm $docker_cpus -v "$out:/o:ro" "$image" "qemu-$arch" /o/workload -n "$n"
    } > "$out/tcg-user-r$r.md" 2>&1 || echo "TCG user mode failed: see $out/tcg-user-r$r.md"
    {
        host
        echo "- $qemu_version, full system, TCG (multi-threaded), shards' pinned kernel 6.18.48, 2 vCPUs, 1 GiB"
        docker run --rm $docker_cpus -v "$out:/o:ro" -v "$base/kvm:/k:ro" "$image" \
            "qemu-system-$arch" -accel tcg,thread=multi -cpu max -smp 2 -m 1024 -nographic -no-reboot \
            -kernel /k/vmlinux -initrd /o/initramfs.cpio \
            -append "console=ttyS0 reboot=k panic=1 rdinit=/init quiet softvirt.restart" </dev/null
    } > "$out/tcg-system-r$r.md" 2>&1 || echo "TCG full system failed: see $out/tcg-system-r$r.md"
    {
        host
        echo "- Firecracker (KVM), shards' pinned kernel 6.18.48, 2 vCPUs, 1 GiB"
        pin "$base/kvm/firecracker" --no-api --config-file "$out/firecracker.json" </dev/null
    } > "$out/kvm-firecracker-r$r.md" 2>&1 || echo "Firecracker failed: see $out/kvm-firecracker-r$r.md"
    r=$((r + 1))
done
echo "results in $out"
