#!/bin/bash
# Roadmap item 7a's light gate (D176 part 6, D242): the unit tests of a move's checks and of
# CPU baselines, then each guard's removal, which must fail them. The one VM it makes is a
# KVM VM with no guest (hv::kvm's baseline test, SKIP without KVM); no microVM boots. Run in
# a copy of the tree (rsync, no .git), each step logged, at nice 10:
#
#   light-gate.sh SRC LOG
#
# CARGO_TARGET_DIR, where set, is the build's; else SRC/../target. A mutation is made in
# SRC, its file kept aside and put back with a fresh mtime (cargo would otherwise keep the
# mutant's build), whatever its run does.
set -u
src=$(cd "$1" && pwd) log=$2
case $log in /*) ;; *) log=$PWD/$log ;; esac
cd "$src" || exit 1
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$src/../target} SHARDS_HELPERS=skip
# The guest binaries `shards` carries run in no unit test: empty stand-ins, not built.
fake=$CARGO_TARGET_DIR/fake
mkdir -p "$fake" && : > "$fake/init" && : > "$fake/server"
export SHARDS_INIT_BINARY=$fake/init SHARDS_SERVER_BINARY=$fake/server
: > "$log"
echo "# $(date -u +%FT%TZ) $(uname -srm); $(rustc --version); tree $(cat "$src/.gate-rev" 2>/dev/null || echo unknown)" >> "$log"

step() {
	echo "=== $*" >> "$log"
	nice -n 10 "$@" >> "$log" 2>&1
	echo "=== exit $? : $*" >> "$log"
}

shards=(cargo test -p shards --bin shards --)
vmm=(cargo test -p shards-vmm --lib --)
told=hv::kvm::x86_64::tests::a_vm_told_a_baseline_gives_its_vcpus_no_more
step "${vmm[@]}" arch:: "$told"
step "${shards[@]}" daemon::moves cli::moves a_move_of_another_hypervisors_microvm_is_refused_typed
step cargo test -p shards --test moved --no-run

# mutate FILE OLD NEW COMMAND...: OLD replaced by NEW once in FILE, COMMAND run, FILE put back.
mutate() {
	local file=$1 old=$2 new=$3
	shift 3
	cp "$file" "$file.kept"
	if python3 - "$file" "$old" "$new" <<'P'; then
import sys
p, old, new = sys.argv[1:4]
s = open(p).read()
if s.count(old) != 1:
    sys.exit(f"{p}: {old!r} found {s.count(old)} times")
open(p, "w").write(s.replace(old, new))
P
		echo "=== mutant: $file: $old -> $new" >> "$log"
		step "$@"
	else
		echo "=== mutant not made: $file: $old" >> "$log"
	fi
	mv "$file.kept" "$file"
	touch "$file"
}

moves_rs=crates/shards/src/daemon/moves.rs
cpuid_rs=crates/vmm/src/arch/x86_64/cpuid.rs
masks=arch::x86_64::cpuid::covers_tests::a_baseline_masks_what_any_machine_lacks
# M1: a plan of another hypervisor taken.
mutate $moves_rs "        && Some(theirs) != hypervisor
" "        && Some(theirs) != hypervisor
        && false
" "${shards[@]}" a_microvm_of_another_hypervisor_is_refused_before_anything_moves a_move_of_another_hypervisors_microvm_is_refused_typed
# M2: the refusal said in words alone, its type not on the wire.
mutate $moves_rs "                        reply.out(&json);
" "                        let _ = json;
" "${shards[@]}" a_move_of_another_hypervisors_microvm_is_refused_typed
# M3: the mover reads the step's words alone.
mutate crates/shards/src/cli/moves.rs "    match serde_json::from_slice::<crate::daemon::Refused>(&answer.1) {" \
	"    match serde_json::from_slice::<crate::daemon::Refused>(b\"\") {" \
	"${shards[@]}" a_refusal_is_said_from_its_type_and_else_in_the_steps_words
# M4: a KVM VM not told its baseline.
mutate crates/vmm/src/hv/kvm/x86_64.rs "        cpuid::within(&mut cpuid, &config.baseline);
" "" "${vmm[@]}" "$told"
# M5: a meet of every bit either has, not both.
mutate $cpuid_rs "                *slot = bits(la, at) & bits(lb, at);" "                *slot = bits(la, at) | bits(lb, at);" \
	"${vmm[@]}" "$masks"
# M6: a meet of two vendors' CPUs.
mutate $cpuid_rs "    if vendor(a) != vendor(b) {
        return Err(Lacks::Vendor);
    }
    let mut both" "    let mut both" "${vmm[@]}" "$masks"
# M7: XSAVE's state components not among the features.
mutate $cpuid_rs "    (0xd, 0, &[3, 6]),
" "" "${vmm[@]}" "$masks"
# M8: a baseline's text of any number of leaves.
mutate $cpuid_rs "    if leaves.len() > most {" "    if leaves.len() > most.saturating_mul(1 << 20) {" \
	"${vmm[@]}" arch::x86_64::cpuid::covers_tests::a_baseline_reads_back_as_written
# M9: an x86-64 guest told a baseline still checked by Linux's names.
mutate $moves_rs "    if baseline.is_empty() {
        ran_on.to_vec()
    } else {
        Vec::new()
    }" "    let _ = baseline;
    ran_on.to_vec()" "${shards[@]}" a_baseline_masks_every_feature_an_x86_guest_finds
# M10: a baseline taken where this machine's KVM shows less of it.
mutate $moves_rs "    shards_vmm::arch::cpu::covers(here, baseline)" "    shards_vmm::arch::cpu::covers(baseline, baseline)" \
	"${shards[@]}" a_machine_short_of_a_baseline_refuses_it_by_each_register
echo GATE_DONE >> "$log"
