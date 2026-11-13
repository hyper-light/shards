#!/bin/bash
# Roadmap item 7a's light gate (D176 part 6): the unit tests of a move's checks, then each
# guard's removal, which must fail them. No VM boots. Run on a host of any OS in a copy of
# the tree (rsync, no .git), each step logged, at nice 10:
#
#   light-gate.sh SRC LOG
#
# CARGO_TARGET_DIR, where set, is the build's; else SRC/../target. A mutation is made in
# SRC, its file kept aside and put back with a fresh mtime (cargo would otherwise keep the
# mutant's build), whatever its run does.
set -u
src=$(cd "$1" && pwd) log=$2
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

moves=(daemon::moves cli::moves a_move_of_another_hypervisors_microvm_is_refused_typed)
step cargo test -p shards-vmm --lib arch::
step cargo test -p shards --bin shards -- "${moves[@]}"

# mutate FILE OLD NEW TEST...: OLD replaced by NEW once in FILE, TEST... run, FILE put back.
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
		step cargo test -p shards --bin shards -- "$@"
	else
		echo "=== mutant not made: $file: $old" >> "$log"
	fi
	mv "$file.kept" "$file"
	touch "$file"
}

moves_rs=crates/shards/src/daemon/moves.rs
# M1: a plan of another hypervisor taken.
mutate $moves_rs "        && Some(theirs) != hypervisor
" "        && Some(theirs) != hypervisor
        && false
" a_microvm_of_another_hypervisor_is_refused_before_anything_moves a_move_of_another_hypervisors_microvm_is_refused_typed
# M2: the refusal said in words alone, its type not on the wire.
mutate $moves_rs "                        reply.out(&json);
" "                        let _ = json;
" a_move_of_another_hypervisors_microvm_is_refused_typed
# M3: the mover reads the step's words alone.
mutate crates/shards/src/cli/moves.rs "    match serde_json::from_slice::<crate::daemon::Refused>(&answer.1) {" \
	"    match serde_json::from_slice::<crate::daemon::Refused>(b\"\") {" \
	a_refusal_is_said_from_its_type_and_else_in_the_steps_words
echo GATE_DONE >> "$log"
