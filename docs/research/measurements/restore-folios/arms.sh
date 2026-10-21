# The restore-folios experiment's arms (sourced by .github/workflows/restore-folios.yml):
# each arm's switches, and, for an `evict-` arm, the page cache dropped once a second
# (drop_caches, by root) while its bench runs, as a busy host drops a template's memory
# file between its restores: from outside the VM process, whose seccomp filter refuses
# fadvise(2) (the first experiment's `drop` arm died of it), and for Firecracker's VMs as
# much as shards'.
arm_vars() {
  case $1 in
    *random) echo "SHARDS_RESTORE_RANDOM=1" ;;
    *whole) echo "SHARDS_SAVE_WHOLE=1" ;;
  esac
}
arm_on() {
  case $1 in
    evict-*)
      sudo sh -c 'while :; do echo 1 > /proc/sys/vm/drop_caches; sleep 1; done' &
      DROPS=$!
      ;;
  esac
}
arm_off() {
  case $1 in
    evict-*)
      # The loop ends killed (wait's 143), which a step's `bash -e` must not take for
      # its own failure.
      sudo kill "$DROPS" 2>/dev/null || true
      wait "$DROPS" 2>/dev/null || true
      ;;
  esac
  return 0
}
