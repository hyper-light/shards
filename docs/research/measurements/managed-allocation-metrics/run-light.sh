#!/usr/bin/env bash
set -eu
ulimit -c 0

revision="$1"
case "$revision" in
    ''|*[!0-9a-f]*) exit 2 ;;
esac
test "${#revision}" -eq 40
counter_sha="$2"
script_sha="$3"
source_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
target_dir="$HOME/managed-allocation-metrics-target-$revision"
records="$HOME/managed-allocation-metrics-light-$revision"
test ! -e "$target_dir"
test ! -e "$records"
mkdir "$target_dir" "$records"

printf '%s\n' "$revision" > "$records/source-revision.txt"
printf '%s\n' 'Kiln admission: cpus=1,mem=1g,disk=2g; standalone counter unit/light only; no profiling/VM/socket/ns/BPF/campaign' > "$records/scope.txt"
uname -a > "$records/uname.txt"
rustc +1.98.1 -Vv > "$records/rustc.txt"
printf '%s  %s\n' "$counter_sha" "$source_dir/managed_alloc.rs" > "$records/source.sha256"
printf '%s  %s\n' "$script_sha" "$source_dir/run-light.sh" >> "$records/source.sha256"
sha256sum -c "$records/source.sha256" > "$records/source-verification.log"

run_check() {
    local name="$1"; shift
    { printf '%q' "$1"; for arg in "${@:2}"; do printf ' %q' "$arg"; done; printf '\n'; } > "$records/$name.command"
    set +e
    "$@" > "$records/$name.log" 2>&1
    local result=$?
    set -e
    printf '%s\n' "$result" > "$records/$name.exit"
    printf '%s exit=%s\n' "$name" "$result"
}

for target in x86_64-unknown-linux-gnu x86_64-unknown-linux-musl; do
    binary="$target_dir/$target-tests"
    run_check "$target-build" rustc +1.98.1 --edition 2024 --test -D warnings -C codegen-units=1 --target "$target" "$source_dir/managed_alloc.rs" -o "$binary"
    if test "$(cat "$records/$target-build.exit")" -eq 0; then
        sha256sum "$binary" > "$records/$target-binary.sha256"
        run_check "$target-test" "$binary" --test-threads=1 --nocapture
    fi
done

clippy="$(rustup which --toolchain 1.98.1 clippy-driver)"
run_check counter-clippy "$clippy" --edition 2024 --test -D warnings -D clippy::undocumented_unsafe_blocks -C codegen-units=1 "$source_dir/managed_alloc.rs" -o "$target_dir/counter-clippy"
printf '%s\n' 'OWED TO ROOT: actual caller registration/normal helper build, seven-target gate, real N0/N1/N2, actual profiles, paired/interleaved repetitions, mutations/restoration/cleanup; these standalone binaries are not the managed_output caller.' > "$records/root-owed.txt"
tar -czf "$records-raw.tgz" -C "$records" .
sha256sum "$records-raw.tgz"
for target in x86_64-unknown-linux-gnu x86_64-unknown-linux-musl; do
    test "$(cat "$records/$target-build.exit")" -eq 0
    test "$(cat "$records/$target-test.exit")" -eq 0
done
test "$(cat "$records/counter-clippy.exit")" -eq 0
