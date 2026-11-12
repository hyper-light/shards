#!/bin/sh
# softvirt-baselines: workload.c run natively, under gVisor's systrap platform (rootless),
# under User-mode Linux with seccomp and with ptrace (x86_64 hosts: UML has no arm64
# port), and in a KVM guest under Firecracker where /dev/kvm opens. Usage: run.sh OUT.
# Each result is OUT/<how>.md. Needs zig 0.16 (the static workload), curl, and docker
# for UML's build (its kernel build needs flex and bison; the container brings them).
# Downloads are pinned and checked: gVisor's release by its published SHA-512,
# kernel.org's tarball by its signed sha256sums file's line, Firecracker's release by
# its SHA-256 file, shards' pinned guest kernel by crates/shards/src/kernel.rs's digest.
set -eu
out=$(mkdir -p "$1" && cd "$1" && pwd)
here=$(cd "$(dirname "$0")" && pwd)
arch=$(uname -m)
n=${N:-20000}
gvisor=20261109.0
linux=7.3.4
fc=v1.18.0

host() {
    echo "- host: $(uname -srm); $(grep -m1 -E 'model name|CPU part' /proc/cpuinfo | cut -d: -f2 | sed 's/^ //')"
    echo "- load: $(cut -d' ' -f1-3 /proc/loadavg); cpus $(nproc); revision $(git -C "$here" rev-parse --short HEAD 2>/dev/null || echo unknown)"
}

zig cc -target "$arch-linux-musl" -O2 -static -o "$out/workload" "$here/workload.c"

{ host; "$out/workload" -n "$n"; } > "$out/native.md"

# gVisor, systrap, rootless: the platform that needs no hardware virtualization.
if [ ! -x "$out/gvisor/runsc" ]; then
    mkdir -p "$out/gvisor"
    base=https://storage.googleapis.com/gvisor/releases/release/$gvisor/$arch
    (cd "$out/gvisor" && curl -fsSLO "$base/gvisor.tar.bz2" && curl -fsSLO "$base/gvisor.tar.bz2.sha512" &&
        sha512sum -c gvisor.tar.bz2.sha512 && tar -xjf gvisor.tar.bz2)
fi
{
    host
    echo "- gVisor release $gvisor: $("$out/gvisor/runsc" --version | head -1)"
    "$out/gvisor/runsc" --rootless --network=none --platform=systrap do "$out/workload" -n "$n"
} > "$out/gvisor-systrap.md" 2>&1 || echo "gVisor failed: see $out/gvisor-systrap.md"

# User-mode Linux, built in a container, its init this workload in an initramfs.
if [ "$arch" = x86_64 ]; then
    if [ ! -x "$out/uml/linux" ]; then
        mkdir -p "$out/uml"
        major=$(echo "$linux" | cut -d. -f1)
        url=https://cdn.kernel.org/pub/linux/kernel/v$major.x
        (cd "$out/uml" && curl -fsSLO "$url/linux-$linux.tar.xz" && curl -fsSLO "$url/sha256sums.asc" &&
            grep " linux-$linux.tar.xz\$" sha256sums.asc | sha256sum -c && tar -xJf "linux-$linux.tar.xz")
        cp "$out/workload" "$out/uml/init"
        printf 'dir /dev 0755 0 0\nnod /dev/console 0600 0 0 c 5 1\nfile /init /b/init 0755 0 0\n' \
            > "$out/uml/initramfs.list"
        docker run --rm -v "$out/uml:/b" -w "/b/linux-$linux" fedora:44 sh -euc "
            dnf -q -y install gcc make flex bison bc diffutils findutils perl >/dev/null
            make -s ARCH=um defconfig
            scripts/config --enable BLK_DEV_INITRD --set-str INITRAMFS_SOURCE /b/initramfs.list
            make -s ARCH=um olddefconfig
            make -s ARCH=um -j\$(nproc) linux
            cp linux /b/linux && cp .config /b/config
            chown -R $(id -u):$(id -g) /b"
    fi
    for mode in on off; do
        {
            host
            echo "- User-mode Linux $linux, seccomp=$mode, mem=1G"
            "$out/uml/linux" mem=1G "seccomp=$mode" con=null con0=fd:0,fd:1 quiet </dev/null
        } > "$out/uml-seccomp-$mode.md" 2>&1 || echo "UML seccomp=$mode failed: see $out/uml-seccomp-$mode.md"
    done
fi

# The hardware's own: a KVM guest under Firecracker, shards' pinned kernel, the same init.
if [ -r /dev/kvm ] && [ -w /dev/kvm ] && [ -x "$out/uml/linux-$linux/usr/gen_init_cpio" ] ; then
    if [ ! -x "$out/kvm/firecracker" ]; then
        mkdir -p "$out/kvm"
        rel=https://github.com/firecracker-microvm/firecracker/releases/download/$fc
        (cd "$out/kvm" && curl -fsSLO "$rel/firecracker-$fc-$arch.tgz" &&
            curl -fsSLO "$rel/firecracker-$fc-$arch.tgz.sha256.txt" &&
            sha256sum -c "firecracker-$fc-$arch.tgz.sha256.txt" && tar -xzf "firecracker-$fc-$arch.tgz" &&
            cp "release-$fc-$arch/firecracker-$fc-$arch" firecracker)
        kernel=https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-17c3ba205400/vmlinux-6.18.48-x86_64
        curl -fsSLo "$out/kvm/vmlinux" "$kernel"
        echo "48a3824b7467dee7d82f36401fed99626e91c5dc5fefc432bc354a69ec616aa7  $out/kvm/vmlinux" | sha256sum -c
        printf 'dir /dev 0755 0 0\nnod /dev/console 0600 0 0 c 5 1\nfile /init %s 0755 0 0\n' "$out/workload" \
            > "$out/kvm/initramfs.list"
        "$out/uml/linux-$linux/usr/gen_init_cpio" "$out/kvm/initramfs.list" > "$out/kvm/initramfs.cpio"
    fi
    cat > "$out/kvm/config.json" <<EOF
{"boot-source": {"kernel_image_path": "$out/kvm/vmlinux", "initrd_path": "$out/kvm/initramfs.cpio",
  "boot_args": "console=ttyS0 reboot=k panic=1 rdinit=/init quiet softvirt.restart"},
 "machine-config": {"vcpu_count": 1, "mem_size_mib": 1024}}
EOF
    {
        host
        echo "- Firecracker $fc (KVM), shards' pinned kernel 6.18.48, 1 vCPU, 1 GiB"
        "$out/kvm/firecracker" --no-api --config-file "$out/kvm/config.json" </dev/null
    } > "$out/kvm-firecracker.md" 2>&1 || echo "Firecracker failed: see $out/kvm-firecracker.md"
fi
echo "results in $out"
