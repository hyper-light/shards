"""Step 8's R10 reds, each shown once: a guard removed by hand (the replacement below), its
one test run and expected to fail, the file restored. Run from the repository's root; given
words, only the reds whose names hold one of them run."""
import pathlib
import re
import subprocess
import sys

NET = ["cargo", "test", "-p", "shards-net", "--lib", "--", "--exact"]

SIGNIN = "crates/net/src/signin.rs"

REDS = [
    # Slice 1: the sign-in profiles (crates/net/src/signin.rs).
    ("signin: a token's name nested or twice refused", SIGNIN,
     "if m.depth != 1 || !m.string || slot.is_some() {", "if !m.string {",
     NET + ["signin::tests::a_token_anywhere_but_its_place_is_refused"]),
    ("signin: a token's value echoed elsewhere refused", SIGNIN,
     "if read.iter().any(|t| contains(&s, &t.value)) {", "if false {",
     NET + ["signin::tests::a_tokens_value_echoed_elsewhere_is_refused_escaped_or_not"]),
    ("signin: an echo sought with its escapes decoded", SIGNIN,
     "let s = unescape(body, r).map_err(|NotJson| Refusal::NotJson)?;",
     "let s = body.get(r.clone()).unwrap_or_default();",
     NET + ["signin::tests::a_tokens_value_echoed_elsewhere_is_refused_escaped_or_not"]),
    ("signin: the access token given its placeholder (the real token through)", SIGNIN,
     "edits.push((t.at.clone(), quoted(with.access.as_bytes())));", "let _ = t;",
     NET + ["signin::tests::a_claude_code_token_response_is_given_placeholders_and_nothing_else_changes"]),
    ("signin: the ID token made inert (the real ID token through)", SIGNIN,
     "edits.push((t.at.clone(), quoted(&inert(&t.value)?)));", "let _ = t;",
     NET + ["signin::tests::a_codex_token_response_keeps_its_id_tokens_claims_and_loses_its_signature"]),
    ("signin: an ID token that is no JWT refused", SIGNIN,
     "if !base64url(header) || !base64url(payload) {", "if false {",
     NET + ["signin::tests::an_id_token_that_is_no_jwt_is_refused"]),
    ("signin: a placeholder a JSON string holds as it is", SIGNIN,
     "if !credential::placeholder(&with.access)\n        || with.refresh.as_ref().is_some_and(|r| !credential::placeholder(r))\n    {",
     "if false {",
     NET + ["signin::tests::a_placeholder_a_json_string_cannot_hold_as_it_is_is_refused"]),
    ("signin: a rewritten response's framing recomputed", SIGNIN,
     '.filter(|f| f.name == "content-length" || f.name == "transfer-encoding")',
     '.filter(|f| f.name == "content-length")',
     NET + ["signin::tests::a_rewritten_responses_framing_is_recomputed"]),
    ("signin: a coded body is not read", SIGNIN,
     "if coded || transfer {", "if false {",
     NET + ["signin::tests::a_coded_body_is_not_read"]),
    ("signin: a body that is no JSON text refused", SIGNIN,
     "let json = json(body).map_err(|NotJson| Refusal::NotJson)?;",
     "let Ok(json) = json(body) else {\n        return Ok(None);\n    };",
     NET + ["signin::tests::a_response_without_tokens_passes_and_one_that_cannot_be_read_is_refused"]),
    ("signin: a refresh answered outside for its own placeholder alone", SIGNIN,
     ".position(|s| s.refresh.as_ref().is_some_and(|r| same(r.as_bytes(), &token)))",
     ".position(|s| s.refresh.as_ref().is_some_and(|r| r.len() == token.len()))",
     NET + ["signin::tests::a_request_that_is_no_refresh_of_a_placeholder_passes"]),
    ("signin: a request naming a field twice passes", SIGNIN,
     "            if slot.is_some() {\n                return None;\n            }\n", "",
     NET + ["signin::tests::a_request_that_is_no_refresh_of_a_placeholder_passes"]),
    ("signin: a minting path read widely", SIGNIN,
     "let path = loose(path(target));", "let path = path(target).to_string();",
     NET + ["signin::tests::a_sign_ins_token_is_never_filled_toward_an_endpoint_that_mints_a_key"]),
    ("signin: a placeholder never prints", SIGNIN,
     'f.debug_struct("Placeholders")\n            .field("access", &format_args!("{} bytes", self.access.len()))',
     'f.debug_struct("Placeholders")\n            .field("access", &self.access)',
     NET + ["signin::tests::no_token_or_placeholder_prints"]),
]

# Slice 2a: the injector takes a sign-in's tokens outside and answers its refresh and revocation there.
DNS = ["cargo", "test", "-p", "shards-dns", "--lib", "--", "--exact"]
INJECT = "crates/net/src/inject.rs"
NETLIB = "crates/net/src/lib.rs"
LEASE = "crates/net/src/lease.rs"
CREDENTIAL = "crates/dns/src/credential.rs"
SIGN_IN_TEST = NET + ["inject::tests::a_sign_in_is_taken_outside_and_its_refresh_and_revocation_answered_there"]
REDS += [
    ("2a inject: a sign-in's response read whole before any of it goes (the real tokens through)", INJECT,
     "if self.sign_in.is_some() && !tunnel && !interim && framing != Framing::Length(0) {", "if false {",
     SIGN_IN_TEST),
    ("2a inject: a refresh answered outside, never sent on", INJECT,
     "            signin::Ask::Pass => return Ok(Outside::Pass),",
     "            signin::Ask::Pass | signin::Ask::Refresh(_) => return Ok(Outside::Pass),",
     SIGN_IN_TEST),
    ("2a inject: a revocation answered here, never sent on", INJECT,
     "            signin::Ask::Pass => return Ok(Outside::Pass),",
     "            signin::Ask::Pass | signin::Ask::Revoke(_) => return Ok(Outside::Pass),",
     SIGN_IN_TEST),
    ("2a inject: a refresh's answer given placeholders (the rotated tokens through)", INJECT,
     ".write_all(&signin::refreshed(&written))", ".write_all(&signin::refreshed(&body))",
     SIGN_IN_TEST),
    ("2a inject: a sign-in's request asks identity coding", INJECT,
     "edits.extend(identity(&head, &fields));", "",
     SIGN_IN_TEST),
    ("2a inject: no value toward an endpoint that mints a key", INJECT,
     "if mints(run, &reference, &host, &target) {", "if false {",
     NET + ["inject::tests::a_sign_ins_token_never_goes_to_the_endpoint_that_mints_a_key"]),
    ("2a net: a capture grants its access token", NETLIB,
     "c.grants.push(captured.grant(&grant.toward));", "",
     SIGN_IN_TEST),
    ("2a net: a refreshed token's held lease let go", NETLIB,
     "                c.leases.forget(&captured.access);\n", "",
     SIGN_IN_TEST),
    ("2a lease: a rotated value's ask under way kept", LEASE,
     "if matches!(self.each.get(reference), Some(Lease::Held { .. })) {", "if true {",
     NET + ["lease::tests::a_captured_credential_is_granted_and_a_rotated_ones_held_lease_let_go"]),
    ("2a signin: refreshes of one sign-in ask once", SIGNIN,
     "Asking::Refresh { index: i, waiting } if *i == index => Some(waiting),",
     "Asking::Refresh { waiting, .. } if false => Some(waiting),",
     NET + ["inject::tests::refreshes_of_one_sign_in_at_once_ask_once"]),
    ("2a signin: one sign-in of a CLI a run", SIGNIN,
     "            || grants.iter().any(|g| g.cli == cli || g.sign_in == sign_in)\n", "",
     NET + ["signin::tests::sign_in_grants_ride_their_message_and_a_cli_has_one"]),
    ("2a signin: a capture's placeholders checked", SIGNIN,
     "        if token(&c.access_placeholder)\n            && c.refresh_placeholder\n                .as_deref()\n"
     "                .is_none_or(|r| token(r) && r != c.access_placeholder)\n",
     "        if true\n",
     NET + ["signin::tests::a_captures_placeholders_that_are_no_tokens_or_anothers_are_declined"]),
    ("2a signin: a refresh's answer never prints its tokens", SIGNIN,
     '.field("result", &result.as_ref().map(|body| body.len()))',
     '.field("result", &result.as_ref().map(|body| String::from_utf8_lossy(body)))',
     NET + ["signin::tests::a_sign_ins_words_round_trip_and_print_no_token"]),
    ("2a dns: a sign-in's refresh placeholder one credential's alone", CREDENTIAL,
     ".find(|g| g.placeholder == *refresh)", ".find(|_| false)",
     DNS + ["credential::tests::a_sign_ins_placeholders_are_tokens_and_one_credentials_alone"]),
]

# Slice 2b: the run host's relay (shards-api's channel and session, shards' api-run), and ROOT's asks
# taken in shards-net (an optional refresh placeholder, the capture's inert ID token and scope).
API = ["cargo", "test", "-p", "shards-api", "--lib", "--", "--exact"]
SHARDS = ["cargo", "test", "-p", "shards", "--bin", "shards", "--", "--exact"]
APIRUN = "crates/shards/src/api_run.rs"
SESSION = "crates/api/src/image/session.rs"
IMAGE_WIRE = "crates/api/src/image/wire.rs"
LEDGER_TEST = SHARDS + ["api_run::tests::a_runs_ledger_relays_its_own_sign_ins_and_lets_a_refreshed_lease_go"]
SESSION_TEST = API + [
    "image::session::tests::"
    "a_sign_ins_capture_and_refresh_are_kept_by_the_broker_and_its_lease_released_at_the_runs_end"
]
REDS += [
    ("2b api-run: a run's sign-ins told its network process", APIRUN,
     "        if let Some(sign_ins) = sign_ins {\n", "        if let Some(sign_ins) = sign_ins.filter(|_| false) {\n",
     SHARDS + ["api_run::tests::"
               "a_run_whose_request_grants_a_sign_in_tells_its_network_process_and_one_that_grants_none_tells_none"]),
    ("2b api-run: a sign-in its egress cannot reach refused", APIRUN,
     "            .find(|h| !shards_net::reaches(&ports, &named, h))", "            .find(|_| false)",
     SHARDS + ["api_run::tests::"
               "a_run_whose_request_grants_a_sign_in_tells_its_network_process_and_one_that_grants_none_tells_none"]),
    ("2b api-run: a refreshed token's held lease let go", APIRUN,
     "            ledger.refreshed(index);\n", "",
     SHARDS + ["api_run::tests::"
               "a_sign_in_is_relayed_its_placeholders_kept_through_a_refresh_and_its_lease_released_at_the_runs_end"]),
    ("2b api-run: a capture's access token granted", APIRUN,
     "        if !self.granted.contains(&c.access) {", "        if false {", LEDGER_TEST),
    ("2b api-run: an ask after the run's release declined", APIRUN,
     "            return Ok(Err(Declined::Ended));", "            return Ok(Ok(()));", LEDGER_TEST),
    ("2b session: a captured access token leasable", SESSION,
     "                grant.captured = Some(c.clone());", "                let _ = c;", SESSION_TEST),
    ("2b session: a lease given before a refresh released", SESSION,
     "                rotated(&asking, &*lessor.0, &before);", "                let _ = &before;", SESSION_TEST),
    ("2b wire: a sign-in's CLI no version has refused", IMAGE_WIRE,
     "let cli = Cli::from_code(code).ok_or(WireError::Tag {",
     "let cli = Cli::from_code(code).or(Some(Cli::Codex)).ok_or(WireError::Tag {",
     API + ["image::wire::tests::a_runs_sign_ins_and_their_words_cross_the_channel"]),
    ("2b signin: a refresh token with no placeholder never reaches the guest", SIGNIN,
     "let refresh = with.refresh.as_ref().ok_or(Refusal::NoPlaceholder)?;",
     "let refresh = with.refresh.as_ref().unwrap_or(&with.access);",
     NET + ["signin::tests::"
            "a_capture_carries_its_inert_id_token_and_scope_and_a_sign_in_may_hold_no_refresh_placeholder"]),
    ("2b signin: the capture's ID token inert", SIGNIN,
     "id_token: tokens.id.as_ref().map(|t| inert(&t.value)).transpose()?,",
     "id_token: tokens.id.as_ref().map(|t| Zeroizing::new(t.value.to_vec())),",
     NET + ["signin::tests::"
            "a_capture_carries_its_inert_id_token_and_scope_and_a_sign_in_may_hold_no_refresh_placeholder"]),
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
