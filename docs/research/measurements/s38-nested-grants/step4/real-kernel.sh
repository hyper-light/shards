# Runs inside `systemd-run --user --scope -p Delegate=yes`: this shell and what it starts in
# a leaf of the scope, pids offered to `work`, which holds no process.
set -o pipefail
cg=/sys/fs/cgroup$(sed -n 's/^0:://p' /proc/self/cgroup)
mkdir "$cg/leaf0" && echo 0 > "$cg/leaf0/cgroup.procs"
echo "+pids +memory" > "$cg/cgroup.subtree_control"
mkdir "$cg/work"
echo "scope=$cg controllers(work)=$(cat "$cg/work/cgroup.controllers")"
cd ~/s38-init-step4/src
export CARGO_TARGET_DIR=$HOME/s38-init-step4/target CARGO_BUILD_JOBS=4
SHARDS_TEST_CGROUP=$cg/work cargo test -p shards-init --target x86_64-unknown-linux-musl -j4 -- on_a_real_kernel --nocapture > "$1" 2>&1
echo "real_kernel_exit=$?"
