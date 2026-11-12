#!/bin/sh
# softvirt-baselines/vm.sh OUT: the baselines again inside a VM that has no hardware
# virtualization of its own, as a cloud VM without nested virtualization has none: a
# Firecracker microVM (KVM on this host; Firecracker exposes no VMX or SVM to its guest),
# shards' pinned kernel, an Alpine root filesystem holding the workload, gVisor and a
# static User-mode Linux. Run run.sh OUT first: this takes its workload, gVisor release,
# Firecracker, kernel and UML source from OUT. Needs mke2fs (e2fsprogs) and docker.
set -eu
out=$(cd "$1" && pwd)
arch=$(uname -m)
n=${N:-20000}
alpine=3.24.4
linux=7.3.4

# A static User-mode Linux, its init the workload, built as run.sh builds the dynamic one.
if [ "$arch" = x86_64 ] && [ ! -x "$out/uml/linux-static" ]; then
    docker run --rm -v "$out/uml:/b" -w "/b/linux-$linux" fedora:44 sh -euc "
        dnf -q -y install gcc make flex bison bc diffutils findutils perl glibc-static >/dev/null 2>&1
        make -s ARCH=um defconfig
        scripts/config --enable BLK_DEV_INITRD --set-str INITRAMFS_SOURCE /b/initramfs.list --enable STATIC_LINK
        make -s ARCH=um olddefconfig
        make -s ARCH=um -j\$(nproc) linux
        cp linux /b/linux-static
        chown -R $(id -u):$(id -g) /b"
fi

root=$out/vm/root
rm -rf "$out/vm" && mkdir -p "$root"
base=https://dl-cdn.alpinelinux.org/alpine/v${alpine%.*}/releases/$arch
(cd "$out/vm" && curl -fsSLO "$base/alpine-minirootfs-$alpine-$arch.tar.gz" &&
    curl -fsSLO "$base/alpine-minirootfs-$alpine-$arch.tar.gz.sha256" &&
    sha256sum -c "alpine-minirootfs-$alpine-$arch.tar.gz.sha256" &&
    tar -xzf "alpine-minirootfs-$alpine-$arch.tar.gz" -C root)
cp "$out/workload" "$root/workload"
mkdir -p "$root/gvisor"
cp -r "$out/gvisor/runsc" "$out/gvisor/gvisor-bin" "$root/gvisor/"
if [ -x "$out/uml/linux-static" ]; then
    cp "$out/uml/linux-static" "$root/uml-linux"
fi
cat > "$root/init" <<INIT
#!/bin/sh
mount -t proc proc /proc; mount -t sysfs sys /sys; mount -t devtmpfs dev /dev
mount -t tmpfs tmp /tmp; mkdir -p /dev/shm; mount -t tmpfs shm /dev/shm
echo "- guest: \$(uname -r) \$(uname -m), \$(grep -c ^processor /proc/cpuinfo) vCPUs, /dev/kvm \$(test -e /dev/kvm && echo present || echo absent)"
echo "### native in the VM"; /workload -n $n
echo "### gVisor systrap in the VM"; /gvisor/runsc --network=none --ignore-cgroups --platform=systrap do /workload -n $n 2>&1
if [ -x /uml-linux ]; then
    for mode in on off; do
        echo "### User-mode Linux seccomp=\$mode in the VM"
        TMPDIR=/dev/shm /uml-linux mem=512M seccomp=\$mode con=null con0=fd:0,fd:1 quiet </dev/null
    done
fi
sync; reboot -f
INIT
chmod +x "$root/init"
mke2fs -q -t ext4 -d "$root" "$out/vm/rootfs.ext4" 1500M
cat > "$out/vm/config.json" <<JSON
{"boot-source": {"kernel_image_path": "$out/kvm/vmlinux",
  "boot_args": "console=ttyS0 reboot=k panic=1 root=/dev/vda rw init=/init"},
 "drives": [{"drive_id": "root", "path_on_host": "$out/vm/rootfs.ext4", "is_root_device": true, "is_read_only": false}],
 "machine-config": {"vcpu_count": 2, "mem_size_mib": 3072}}
JSON
{
    echo "- host: $(uname -srm); $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | sed 's/^ //'); load $(cut -d' ' -f1-3 /proc/loadavg)"
    echo "- VM: Firecracker (KVM, no nested virtualization), 2 vCPUs, 3 GiB, Alpine $alpine root"
    "$out/kvm/firecracker" --no-api --config-file "$out/vm/config.json" </dev/null
} > "$out/vm.md" 2>&1 || echo "the VM run failed: see $out/vm.md"
echo "results in $out/vm.md"
