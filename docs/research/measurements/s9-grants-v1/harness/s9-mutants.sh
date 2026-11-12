#!/bin/bash
# Guard-removal mutants of step 9's change (R10): each removes one guard, and the test that
# holds it must fail; the source is restored (and touched, so cargo rebuilds) after each.
cd ~/g121-tmp/s9-mut || exit 2
export CARGO_TARGET_DIR=$HOME/g121-tmp/s9-mut-target SHARDS_HELPERS=skip
echo "== $(git log --oneline -1) $(date -u +%FT%TZ)"
mutant() { # name file perl-expression crate test-filter
  local name=$1 file=$2 expr=$3 crate=$4 filter=$5
  cp "$file" "$file.orig"
  perl -0pi -e "$expr" "$file"
  if cmp -s "$file" "$file.orig"; then echo "MUTANT $name: NOT APPLIED"; mv "$file.orig" "$file"; touch "$file"; return; fi
  out=$(cargo test -p $crate -- "$filter" 2>&1); s=$?
  mv "$file.orig" "$file"; touch "$file"
  if [ $s != 0 ] && printf '%s\n' "$out" | grep -q "test result: FAILED"; then
    echo "MUTANT $name: killed by $(printf '%s\n' "$out" | grep -E '^test .* FAILED' | head -3 | tr '\n' ' ')"
  elif [ $s != 0 ]; then echo "MUTANT $name: build failed"; printf '%s\n' "$out" | grep -E "^error" -A 5 | head -12
  else echo "MUTANT $name: SURVIVED"; fi
}
mutant held-none crates/agentfile/src/agentfile.rs 's/(pub fn held\(directives: &\[Directive\]\) -> Result<\(\), Vec<u8>> \{\n)/$1    if directives.len() < usize::MAX { return Ok(()); }\n/' shards-agentfile members_reaching_one_port_by_name
mutant boundary-ignored crates/agentfile/src/agentfile.rs 's/let opened = open\s*\.iter\(\)\s*\.any\(\|&\(proto, lo, hi\)\| proto == 6 && lo <= port && port <= hi\);/let opened = !open.is_empty() || open.is_empty();/' shards-agentfile destinations_are_what_joined
mutant let-in-destination crates/agentfile/src/agentfile.rs 's/if \*direction != Direction::Egress \{\n(\s*)return Err\(said\(/if *direction == Direction::Ingress \&\& false {\n$1return Err(said(/' shards-agentfile a_destination_is_a_tcp_host
mutant localhost crates/dns/src/lib.rs 's/if host == "localhost" \|\| host.ends_with\(".localhost"\)/if host == "never.localhost"/' shards-dns a_destination_names
mutant host-any-port crates/net/src/lib.rs 's/\.any\(\|\(h, p\)\| \*p == port && h\.eq_ignore_ascii_case\(dns::HOST\)\)/.any(|(h, p)| (*p == port || *p != port) \&\& h.eq_ignore_ascii_case(dns::HOST))/' shards-net the_host_itself
mutant host-asked crates/net/src/lib.rs 's/\|\| \(!name\.eq_ignore_ascii_case\(dns::HOST\)\s*&& /|| (/' shards-net the_host_itself
mutant init-expose-members crates/init/src/netplan.rs 's/let for_d = \(agents\.is_empty\(\) && harnesses\.is_empty\(\)\) \|\| named\.iter\(\)\.any\(\|m\| m == d\.1\);\n(\s*)if for_d\n(\s*)&& e\.get\("direction"\)\.and_then\(Value::str\) != Some\("ingress"\)/let for_d = true;\n$1if for_d\n$2\&\& e.get("direction").and_then(Value::str) != Some("ingress")/' shards-init destinations_reach_the_members
mutant init-no-host-name crates/init/src/netplan.rs 's/hosts\.push_str\(&format!\("\{g\}\\t\{\}\\n", shards_dns::HOST\)\);/let _ = g;/' shards-init destinations_reach_the_members
mutant receivers-dropped crates/shards/src/api_run.rs 's/to\.push\(wire::Receiving \{/if false { to.push(wire::Receiving {/; s/(member: member\.index,\n\s*port,\n\s*\}\));/$1 }/' 'shards --bin shards' published_ports_name
echo "== done $(date -u +%FT%TZ)"
