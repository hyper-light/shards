"""Step 3's R10 reds, each shown once: a guard removed by hand (the replacement below), its
one test run and expected to fail, the file restored. Run from the repository's root; given
words, only the reds whose names hold one of them run."""
import pathlib
import re
import subprocess
import sys

DNS = ["cargo", "test", "-p", "shards-dns", "--lib", "--", "--exact"]
NET = ["cargo", "test", "-p", "shards-net", "--lib", "--", "--exact"]
API = ["cargo", "test", "-p", "shards-api", "--lib", "--", "--exact"]
SHARDS = ["cargo", "test", "-p", "shards", "--bin", "shards", "--", "--exact"]

CREDENTIAL = "crates/dns/src/credential.rs"
NETLIB = "crates/net/src/lib.rs"
IMAGE = "crates/api/src/image.rs"
WIRE = "crates/api/src/image/wire.rs"
RUN = "crates/shards/src/api_run.rs"

REDS = [
    # Slice 1: the placeholder's carriage and the shared decision.
    ("dns: a placeholder fills its own grant alone", CREDENTIAL,
     "        if same(grant.placeholder.as_bytes(), token.as_bytes())\n            && grant",
     "        if grant",
     DNS + ["credential::tests::decide_fills_only_a_grants_placeholder_on_an_agreed_granted_host"]),
    ("dns: a placeholder fills toward its grant's hosts alone (substituting regardless of host)",
     CREDENTIAL,
     "            && grant.toward.iter().any(|h| h.eq_ignore_ascii_case(host))\n", "\n",
     DNS + ["credential::tests::decide_fills_only_a_grants_placeholder_on_an_agreed_granted_host"]),
    ("dns: a placeholder is a token", CREDENTIAL,
     "        if !placeholder(&grant.placeholder) {\n"
     "            return Err(Refused::NotAToken(grant.reference.clone()));\n"
     "        }\n", "",
     DNS + ["credential::tests::each_placeholder_is_a_token_and_one_credentials_alone"]),
    ("dns: one placeholder, one credential", CREDENTIAL,
     ".find(|g| g.placeholder == grant.placeholder && g.reference != grant.reference)",
     ".find(|_| false)",
     DNS + ["credential::tests::each_placeholder_is_a_token_and_one_credentials_alone"]),
    ("dns: a placeholder never prints", CREDENTIAL,
     '.field("placeholder", &format_args!("{} bytes", self.placeholder.len()))',
     '.field("placeholder", &self.placeholder)',
     DNS + ["credential::tests::a_grants_placeholder_never_prints"]),
    ("api: a request's placeholders checked", IMAGE,
     "    shards_dns::credential::check(&credentials).map_err(|refused| ImageRefusal::Spec {\n"
     "        what: \"a credential grant\",\n"
     "        why: refused.to_string(),\n"
     "    })?;\n", "",
     API + ["image::tests::a_credential_grant_rides_by_reference"]),
    ("api: each placeholder rides the channel", WIRE,
     "        for grant in &self.credentials {\n            w.text(&grant.placeholder)?;\n        }\n", "",
     API + ["image::wire::tests::messages_round_trip"]),
    ("run host: placeholders checked again", RUN,
     "    shards_net::dns::credential::check(grants)\n"
     "        .map_err(|refused| No(Cause::Request, format!(\"credential {refused}\")))?;\n",
     "",
     SHARDS + ["api_run::tests::credential_grants_reach_the_network_process_by_reference"]),
    ("net: placeholders checked again as the grants come", NETLIB,
     "                || dns::credential::check(&grants).is_err()\n", "",
     NET + ["tests::credential_grants_are_taken_with_their_placeholders_checked"]),
]

# Slice 2: the injector (crates/net/src/inject.rs) and its handoff (tcp.rs).
INJECT = "crates/net/src/inject.rs"
TCP = "crates/net/src/tcp.rs"
REDS += [
    ("inject: a Host that is not the SNI answered 421", INJECT,
     "        if credential::decide(grants, &self.sni, &host, None) == Fill::Misdirected {",
     "        if false {",
     NET + ["inject::tests::a_request_for_a_host_that_is_not_the_sni_is_answered_421"]),
    ("inject: only a granted name's TLS is ended", INJECT,
     "        if !granted {\n            return self.pass();\n        }\n", "",
     NET + ["inject::tests::a_placeholder_toward_an_ungranted_host_carries_no_credential"]),
    ("inject: a value filled where decide says (a fill regardless of host)", INJECT,
     "            if let Fill::With(reference) = credential::decide(grants, &self.sni, &host, Some(token)) {",
     "            if let Some(reference) = grants.iter().find(|g| g.placeholder == token).map(|g| g.reference.clone()) {",
     NET + ["inject::tests::a_placeholder_toward_an_ungranted_host_carries_no_credential"]),
    ("inject: a request waits for its lease", INJECT,
     "                Asked::Waiting => waiting = true,", "                Asked::Waiting => {}",
     NET + ["inject::tests::a_granted_hosts_request_carries_the_value_and_its_echo_comes_back_masked"]),
    ("inject: an echo of the value masked", INJECT,
     "        if self.filled.is_empty() && self.queue.is_empty() {", "        if true {",
     NET + ["inject::tests::a_granted_hosts_request_carries_the_value_and_its_echo_comes_back_masked"]),
    ("inject: a body streams, held by nothing", INJECT,
     "            Request::Body(mut framing) => {",
     "            Request::Body(framing) if self.pending.len() < (1 << 20) => {\n"
     "                self.request = Request::Body(framing);\n                Ok(false)\n            }\n"
     "            Request::Body(mut framing) => {",
     NET + ["inject::tests::a_large_body_streams_through_unchanged_under_backpressure"]),
    ("inject: hop-by-hop fields cut", INJECT,
     "    edits.sort_by_key(|(r, _)| r.start);\n    edits\n}", "    edits.clear();\n    edits\n}",
     NET + ["inject::tests::hop_by_hop_fields_go_and_the_rest_stays"]),
    ("inject: Transfer-Encoding beside Content-Length refused", INJECT,
     "        if !lengths.is_empty() {\n            return Err(Refusal::Malformed(",
     "        if false {\n            return Err(Refusal::Malformed(",
     NET + ["inject::tests::a_requests_framing_is_read_strictly"]),
    ("inject: a head's lines in CRLF alone", INJECT,
     "        b'\\r' => bytes.get(i + 1).is_none_or(|&n| n == b'\\n'),", "        b'\\r' => true,",
     NET + ["inject::tests::a_heads_lines_end_in_crlf_alone"]),
    ("inject: no control in a chunk's extensions", INJECT,
     "        || ext.iter().any(|&b| (b < 0x20 && b != b'\\t') || b == 0x7f)", "        || false",
     NET + ["inject::tests::chunked_bodies_are_read_strictly"]),
    ("inject: AES-256-GCM alone on the guest's side", INJECT,
     "        cipher_suites: vec![aws_lc_rs::cipher_suite::TLS13_AES_256_GCM_SHA384],",
     "        cipher_suites: vec![\n            aws_lc_rs::cipher_suite::TLS13_AES_256_GCM_SHA384,\n"
     "            aws_lc_rs::cipher_suite::TLS13_AES_128_GCM_SHA256,\n        ],",
     NET + ["inject::tests::a_guest_offering_no_aes_256_gcm_is_refused"]),
    ("tcp: what the injector holds out of the window", TCP,
     "        let injector = self.inject.as_ref().map_or(0, |f| f.held());",
     "        let injector = 0;",
     NET + ["tcp::tests::the_injector_passes_an_ungranted_flow_and_ends_a_granted_ones_tls"]),
]

KEEP = re.compile(r"^(test result|test .* FAILED|thread '.*' .*panicked at|error(\[|:))")


def main() -> int:
    only = sys.argv[1:]
    reds = [r for r in REDS if not only or any(w in r[0] for w in only)]
    bad = 0
    for name, path, old, new, cmd in reds:
        file = pathlib.Path(path)
        source = file.read_text()
        if source.count(old) != 1:
            print(f"STEP red {name}: guard text found {source.count(old)} times", flush=True)
            bad += 1
            continue
        file.write_text(source.replace(old, new))
        try:
            run = subprocess.run(cmd, capture_output=True, text=True)
        finally:
            file.write_text(source)
        out = run.stdout + run.stderr
        failed = run.returncode != 0 and "test result: FAILED" in out
        print(f"STEP red {name}: {'red, as it should be' if failed else 'NOT RED'} (exit {run.returncode})", flush=True)
        for line in out.splitlines():
            if KEEP.match(line):
                print(f"STEP   {line[:220]}", flush=True)
        bad += 0 if failed else 1
    print(f"STEP reds: {len(reds) - bad} of {len(reds)} red", flush=True)
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
