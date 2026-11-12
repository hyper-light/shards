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
mkdir -p "$root/proc" "$root/sys" "$root/dev" "$root/tmp"
cat > "$root/init" <<'INIT'
#!/bin/sh
# The same agent-like work under each of the guest kernel's huge-page policies: its own
# default (transparent huge pages on madvise, no smaller sizes), THP always, 64 KiB
# folios always (multi-size THP), and every folio size always.
mount -t proc proc /proc; mount -t sysfs sys /sys; mount -t devtmpfs dev /dev; mount -t tmpfs tmp /tmp
thp=/sys/kernel/mm/transparent_hugepage
echo "- guest: $(uname -r) $(uname -m), $(grep -c ^processor /proc/cpuinfo) vCPUs, THP $(cat $thp/enabled), sizes: $(ls -d $thp/hugepages-*kB 2>/dev/null | sed 's/.*hugepages-//' | tr '\n' ' ')"
cat > /tmp/work.py <<'PY'
import time
d = {i: str(i) * 3 for i in range(1_000_000)}
l = [bytes(64) for _ in range(200_000)]
print("python holds", len(d), "entries", flush=True)
time.sleep(600)
PY
cat > /tmp/work.js <<'JS'
const a = []; for (let i = 0; i < 1000000; i++) a.push({i, s: "x" + i});
const m = new Map(); for (let i = 0; i < 200000; i++) m.set(i, Buffer.alloc(64));
console.log("node holds", a.length, "objects"); setTimeout(() => {}, 600000);
JS
policy() {
    for f in $thp/hugepages-*kB/enabled; do echo never > "$f"; done
    case $1 in
    default) echo madvise > $thp/enabled; echo inherit > $thp/hugepages-2048kB/enabled ;;
    thp-always) echo always > $thp/enabled; echo inherit > $thp/hugepages-2048kB/enabled ;;
    64k-always) echo never > $thp/enabled; echo always > $thp/hugepages-64kB/enabled ;;
    all-always) echo always > $thp/enabled; for f in $thp/hugepages-*kB/enabled; do echo always > "$f"; done ;;
    esac
}
for p in default thp-always 64k-always all-always; do
    policy $p
    python3 /tmp/work.py & py=$!
    node /tmp/work.js & js=$!
    sleep 25
    echo "### policy $p: THP $(cat $thp/enabled); 64kB $(cat $thp/hugepages-64kB/enabled 2>/dev/null); $(grep AnonHugePages /proc/meminfo | tr -s ' ')"
    /pagemap -v
    kill $py $js; wait $py $js 2>/dev/null
    echo 3 > /proc/sys/vm/drop_caches; sleep 2
done
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
