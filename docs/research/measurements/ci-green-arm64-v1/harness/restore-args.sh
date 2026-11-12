#!/bin/sh
# What `shards restore DIR --no-console` hands its VM process, and the helpers it starts,
# without a VM: SHARDS_VM_BINARY names a stand-in that writes the arguments it was given and
# the processes beside it (the working set's writer on Linux, the grants broker on macOS),
# then exits. DIR is a snapshot as snapshot::exists reads one (a `current` pointer, its
# generation's `state`) with no working set.
#
#   restore-args.sh SHARDS_BINARY OUT_DIR
set -eu
shards=$1
out=$2
rm -rf "$out"
mkdir -p "$out/snap/g-1" "$out/home"
printf 'g-1\n' > "$out/snap/current"
printf 'x' > "$out/snap/g-1/state"
cat > "$out/vm" <<EOF
#!/bin/sh
printf '%s\n' "\$@" > "$out/vm-args"
if [ -d /proc/self ]; then
  for c in /proc/[0-9]*/cmdline; do
    line=\$(tr '\0' ' ' < "\$c" 2>/dev/null) || continue
    case "\$line" in *working-set*|*grants*|*console-said*) echo "\$line" ;; esac
  done > "$out/helpers"
else
  ps -axo command | grep -E 'working-set|grants|console-said' | grep -v grep > "$out/helpers" || true
fi
exit 0
EOF
chmod +x "$out/vm"
cd "$out"
SHARDS_HOME="$out/home" SHARDS_VM_BINARY="$out/vm" "$shards" restore "$out/snap" --no-console || true
echo "== the VM's arguments"
tr '\n' ' ' < "$out/vm-args"; echo
echo "== helpers beside it"
cat "$out/helpers"
if grep -qx -- --working-set "$out/vm-args"; then echo "RESULT working-set writer: started"; else echo "RESULT working-set writer: none"; fi
