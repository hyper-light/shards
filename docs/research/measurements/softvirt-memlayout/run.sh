#!/bin/sh
# softvirt-memlayout/run.sh OUT BASELINES: boots shards' pinned guest kernel in a
# Firecracker microVM (from softvirt-baselines/run.sh's BASELINES/kvm) with an Alpine root
# holding python3, nodejs and git, runs agent-like work in it, and counts with pagemap.c
# how many host mappings its memory would need (OUT/memlayout.md). Needs zig 0.16, docker
# (Alpine's apk builds the root; the image alpine:latest, already pulled), mke2fs.
set -eu
out=$(mkdir -p "$1" && cd "$1" && pwd)
base=$(cd "$2" && pwd)
here=$(cd "$(dirname "$0")" && pwd)
arch=$(uname -m)

zig cc -target "$arch-linux-musl" -O2 -static -o "$out/pagemap" "$here/pagemap.c"
root=$out/root
if [ ! -x "$root/usr/bin/python3" ]; then
    rm -rf "$root" && mkdir -p "$root"
    docker run --rm -v "$root:/r" alpine:latest sh -euc '
        apk add -q --root /r --initdb --keys-dir /etc/apk/keys --repositories-file /etc/apk/repositories \
            busybox musl python3 nodejs git >/dev/null
        chown -R '"$(id -u):$(id -g)"' /r'
fi
cp "$out/pagemap" "$root/pagemap"
cat > "$root/init" <<'INIT'
#!/bin/sh
mount -t proc proc /proc; mount -t sysfs sys /sys; mount -t devtmpfs dev /dev; mount -t tmpfs tmp /tmp
echo "- guest: $(uname -r) $(uname -m), $(grep -c ^processor /proc/cpuinfo) vCPUs, THP $(cat /sys/kernel/mm/transparent_hugepage/enabled)"
cd /tmp
python3 -c '
import time
d = {i: str(i) * 3 for i in range(1_000_000)}
l = [bytes(64) for _ in range(200_000)]
print("python holds", len(d), "entries", flush=True)
time.sleep(600)' &
node -e '
const a = []; for (let i = 0; i < 1000000; i++) a.push({i, s: "x" + i});
const m = new Map(); for (let i = 0; i < 200000; i++) m.set(i, Buffer.alloc(64));
console.log("node holds", a.length, "objects"); setTimeout(() => {}, 600000);' &
git init -q repo && cd repo && i=0
while [ $i -lt 2000 ]; do echo "line $i" > "f$i.txt"; i=$((i + 1)); done
git add . && git -c user.name=t -c user.email=t@t commit -q -m files && cd /tmp
sleep 20
echo "### after python, node and git (git's processes gone)"
/pagemap -v
i=0; while [ $i -lt 300 ]; do sh -c 'true' ; i=$((i + 1)); done
python3 -c 'x = [bytearray(4096) for _ in range(100000)]; del x'
echo "### after 300 shells and 400 MiB allocated and freed"
/pagemap
sync; reboot -f
INIT
chmod +x "$root/init"
rm -f "$out/rootfs.ext4"
mke2fs -q -t ext4 -d "$root" "$out/rootfs.ext4" 2G
cat > "$out/config.json" <<JSON
{"boot-source": {"kernel_image_path": "$base/kvm/vmlinux",
  "boot_args": "console=ttyS0 reboot=k panic=1 root=/dev/vda rw init=/init"},
 "drives": [{"drive_id": "root", "path_on_host": "$out/rootfs.ext4", "is_root_device": true, "is_read_only": false}],
 "machine-config": {"vcpu_count": 2, "mem_size_mib": 2048}}
JSON
{
    echo "- host: $(uname -srm); load $(cut -d' ' -f1-3 /proc/loadavg); revision ${REVISION:-$(git -C "$here" rev-parse --short HEAD 2>/dev/null || echo unknown)}"
    echo "- VM: Firecracker (KVM), shards' pinned kernel 6.18.48, 2 vCPUs, 2 GiB, Alpine root"
    "$base/kvm/firecracker" --no-api --config-file "$out/config.json" </dev/null
} > "$out/memlayout.md" 2>&1 || echo "the VM run failed: see $out/memlayout.md"
echo "results in $out/memlayout.md"
