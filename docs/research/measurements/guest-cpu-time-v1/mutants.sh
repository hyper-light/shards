#!/usr/bin/env bash
# Guard mutations for guest-cpu-time-v1 (R10): each removes one guard and runs the unit test
# that holds it, which must fail; the file is restored from git and touched after each, so
# that cargo never keeps a mutant's build (the touch: a restored file's old mtime would).
# Run from the repository's root on a Linux host: docs/research/measurements/guest-cpu-time-v1/mutants.sh
set -u
target=${TARGET:-x86_64-unknown-linux-gnu}
failed_as_expected=0
survived=0

mutate() {
	local name=$1 file=$2 from=$3 to=$4 crate=$5 test=$6
	if ! grep -qF -- "$from" "$file"; then
		echo "MUTANT $name: pattern not found in $file"
		survived=$((survived + 1))
		return
	fi
	python3 -I - "$file" "$from" "$to" <<'EOF'
import sys
path, old, new = sys.argv[1:4]
text = open(path).read()
assert text.count(old) == 1, f"{old!r} appears {text.count(old)} times"
open(path, "w").write(text.replace(old, new))
EOF
	local out
	out=$(cargo test -q -j4 --target "$target" -p "$crate" --lib -- "$test" 2>&1)
	local status=$?
	git checkout -- "$file" && touch "$file"
	if [ $status -ne 0 ] && printf '%s' "$out" | grep -q "panicked"; then
		echo "MUTANT $name: killed by $test"
		printf '%s\n' "$out" | grep -E "panicked|assertion|left:|right:" | head -6 | sed 's/^/    /'
		failed_as_expected=$((failed_as_expected + 1))
	else
		echo "MUTANT $name: SURVIVED ($test, status $status)"
		printf '%s\n' "$out" | tail -15 | sed 's/^/    /'
		survived=$((survived + 1))
	fi
}

g=crates/vmm/src/vm/guest_time.rs
mutate "a vCPU's time may fall" $g \
	"self.last = self.last.max(ns);" "self.last = ns;" \
	shards-vmm guest_time::tests::a_vcpus_time_never_falls_nor_does_the_machines
mutate "a leaving vCPU's time is dropped" $g \
	"                let (ns, _) = clock.sample();
                clocks.departed = clocks.departed.saturating_add(ns);" \
	"                let _ = clock.sample();" \
	shards-vmm guest_time::tests::a_vcpu_that_leaves_keeps_its_time_in_the_machines
mutate "a stale guard removes a newer vCPU of its index" $g \
	"is_none_or(|c| c.joined != joined)" "is_none()" \
	shards-vmm guest_time::tests::an_index_that_comes_back_starts_again_and_the_total_keeps_the_first
mutate "a replaced vCPU's time is dropped" $g \
	"                clocks.departed = clocks.departed.saturating_add(stale.last);" \
	"                let _ = stale;" \
	shards-vmm guest_time::tests::an_index_that_comes_back_starts_again_and_the_total_keeps_the_first
mutate "an unread vCPU says nothing of why" $g \
	"Err(why) => (self.last, Some(why))," "Err(_) => (self.last, None)," \
	shards-vmm guest_time::tests::an_unread_vcpu_says_why_and_keeps_its_last
mutate "decoding takes vCPUs out of order" $g \
	"if vcpus.last().is_some_and(|v| v.index >= index) {" "if false {" \
	shards-vmm guest_time::tests::text_other_than_encoded_times_is_refused
mutate "encoding keeps a reason's line breaks" $g \
	"if c == '\\n' || c == '\\r' { ' ' } else { c }" "c" \
	shards-vmm guest_time::tests::encoded_times_decode_as_they_were
mutate "the guest time is field 42" crates/vmm/src/platform/unix.rs \
	"fields.split_ascii_whitespace().nth(40)" "fields.split_ascii_whitespace().nth(39)" \
	shards-vmm platform::unix::usage_tests::a_stat_lines_guest_time_is_its_43rd_field
mutate "the name ends at its first )" crates/vmm/src/platform/unix.rs \
	"line.iter().rposition(|&b| b == b')')?" "line.iter().position(|&b| b == b')')?" \
	shards-vmm platform::unix::usage_tests::a_stat_lines_guest_time_is_its_43rd_field
mutate "WHP's guest time counts the hypervisor's" crates/vmm/src/hv/mod.rs \
	"total_100ns.saturating_sub(hypervisor_100ns).saturating_mul(100)" "total_100ns.saturating_mul(100)" \
	shards-vmm hv::tests::a_whp_vcpus_guest_time_is_its_run_time_less_the_hypervisors
echo "killed $failed_as_expected, survived $survived"
[ "$survived" -eq 0 ]
