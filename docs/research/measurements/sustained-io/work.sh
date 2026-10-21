#!/bin/sh
# One workload of the sustained-I/O benchmark (run.py), in a microVM or a container:
# work.sh WORKLOAD PLACE, PLACE where it writes: layer (the root's writable layer, /work),
# volume (/vol) or block (/dev/vdb, fio's alone). Its last line is "SIO" and its
# measures: the workload's time, and what its cgroup used of memory and CPU.
set -eu
workload=$1 place=$2
case $place in
layer) dir=/work ;;
volume) dir=/vol ;;
block) dir=/dev/vdb ;;
*) echo "work.sh: place $place?" >&2; exit 2 ;;
esac
cgroup=/sys/fs/cgroup$(cut -d: -f3 /proc/self/cgroup)
cpus=$(nproc)
start=$(date +%s%N)
case $workload in
nop) true ;;
npm) cd "$dir/cli" && npm ci --offline --ignore-scripts --no-audit --no-fund --no-progress --loglevel=error ;;
cargo) cd "$dir/rust-analyzer" && cargo build --release --offline --locked -q -j "$cpus" -p rust-analyzer ;;
test)
    # Its report, where it failed, the tests that did and why; nothing where it passed.
    cd "$dir/rust-analyzer" && cargo test --offline --locked -q -j "$cpus" --workspace --exclude xtask -- \
        --skip check_code_formatting --skip smoke_test_real_sysroot_cargo >/tmp/sio-test.txt 2>&1 ||
        { s=$?; grep -v '^test .* ok$' /tmp/sio-test.txt | tail -60; exit $s; }
    ;;
fio)
    if [ "$place" = block ]; then file=$dir; else file=$dir/sio.fio; fi
    echo SIO-FIO-BEGIN
    SIO_FILE=$file fio --output-format=json /sio/fio.ini
    echo SIO-FIO-END
    ;;
*) echo "work.sh: workload $workload?" >&2; exit 2 ;;
esac
end=$(date +%s%N)
usage=$(awk '$1 == "usage_usec" { print $2 }' "$cgroup/cpu.stat" 2>/dev/null || echo 0)
echo "SIO work_ns=$((end - start)) guest_peak=$(cat "$cgroup/memory.peak" 2>/dev/null || echo 0) guest_cpu_usec=${usage:-0}"
