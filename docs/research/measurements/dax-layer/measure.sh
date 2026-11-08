#!/bin/sh
# What an ext4 on virtio-pmem, mounted with DAX, costs a microVM, by the device's size (the
# user's plan, item 2: a writable layer off tmpfs): the guest memory libnvdimm takes for
# the device's page structures as it probes it, which filesystem DAX needs (a 64th of the
# device: 64-byte struct pages a 4 KiB page, drivers/nvdimm/pmem.c pmem_should_map_pages),
# the time the probe and the DAX mount take, and a write's path into the host's page cache.
# Each size boots shards' pinned kernel under shards' own VMM (`shards run --kernel`) from
# the sustained-I/O benchmark's ext4 root ($SIO/rootfs.ext4, images.sh), a sparse file of
# SIZE GiB given as its pmem device, formatted on the host with its inode tables and journal
# left to the kernel (lazy), so the file stays sparse. Linux, as its user.
# Each boot's console is kept beside its file ($SIO/dax/console-SIZE-ROUND.txt), its
# kernel's log at boot (no `quiet`) with it: when init ran, and how far the probe got.
# Usage: measure.sh SHARDS ROUNDS SIZE_GIB...   (one line per boot: SIZE ROUND key=value...)
set -eu
shards=$1 rounds=$2
shift 2
SIO=${SIO:-$HOME/sio}
dir=$SIO/dax
mkdir -p "$dir"
# The guest's side: what it has before and after the probe, and the DAX mount's time.
cat >"$dir/m.sh" <<'EOF'
#!/bin/sh
PATH=/usr/sbin:/usr/bin:/sbin:/bin
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
mount -t tmpfs tmpfs /mnt
free() { awk '/^MemFree:/ { f = $2 } /^MemAvailable:/ { a = $2 } END { print "free_kib=" f " available_kib=" a }' /proc/meminfo; }
early=$(free)
for b in /sys/bus/nd/devices/ndbus*; do [ -e "$b/wait_probe" ] && cat "$b/wait_probe" >/dev/null; done
probed=$(free)
mounted=none
if [ -b /dev/pmem0 ]; then
    mkdir -p /mnt/l
    t0=$(date +%s%N)
    if mount -t ext4 -o ro,dax=always /dev/pmem0 /mnt/l 2>/dev/null; then
        t1=$(date +%s%N)
        mounted=$(( (t1 - t0) / 1000 ))
        grep -q ' /mnt/l ext4 .*dax' /proc/mounts && dax=yes || dax=no
    fi
fi
probe=$(dmesg | awk '/pmem0|nd_pmem|virtio_pmem/ { sub(/^\[ */, ""); sub(/\].*/, ""); last = $0 } END { print last }')
echo "SIO-DAX early_$early probed_$probed mount_us=$mounted dax=${dax:-no} probe_at=${probe:-none} uptime=$(cut -d' ' -f1 /proc/uptime)" >/dev/console
# The line out of the console's buffer before the kernel's own say of the power-off.
sleep 1
echo o >/proc/sysrq-trigger
while :; do sleep 60; done
EOF
chmod 755 "$dir/m.sh"
root=$dir/root.ext4
cp --reflink=auto "$SIO/rootfs.ext4" "$root"
debugfs -w -R "write $dir/m.sh m.sh" "$root" >/dev/null 2>&1
debugfs -w -R "set_inode_field m.sh mode 0100755" "$root" >/dev/null 2>&1
for size in "$@"; do
    pmem=$dir/pmem-$size.img
    if [ "$size" -gt 0 ] && [ ! -e "$pmem" ]; then
        truncate -s "${size}G" "$pmem"
        mke2fs -q -t ext4 -E lazy_itable_init=1,lazy_journal_init=1 "$pmem"
    fi
    for round in $(seq "$rounds"); do
        set -- --kernel "$SIO/bin/kernel" --cmdline "console=ttyS0 panic=1 root=/dev/vda ro init=/m.sh" \
            --cpus 2 --memory 1024 --disk "$root:ro"
        if [ "$size" -gt 0 ]; then set -- "$@" --pmem "$pmem"; fi
        console=$dir/console-$size-$round.txt
        "$shards" run "$@" >"$console" 2>&1 || true
        line=$(tr -d '\r' <"$console" | grep -a 'SIO-DAX' || true)
        boot=$(tr -d '\r' <"$console" | grep -a 'Run /m.sh as init' | sed 's/^\[ *//; s/\].*//')
        echo "$size $round init_at=${boot:-none} ${line#SIO-DAX }"
    done
done
echo "allocated: $(du -h "$dir"/pmem-*.img 2>/dev/null | tr '\n' ' ')"
