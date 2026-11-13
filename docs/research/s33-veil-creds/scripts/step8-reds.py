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
     "if !credential::placeholder(&with.access) || !credential::placeholder(&with.refresh) {", "if false {",
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
     ".position(|s| same(s.refresh.as_bytes(), &token))",
     ".position(|s| s.refresh.len() == token.len())",
     NET + ["signin::tests::a_request_that_is_no_refresh_of_a_placeholder_passes"]),
    ("signin: a request naming a field twice passes", SIGNIN,
     "            if slot.is_some() {\n                return None;\n            }\n", "",
     NET + ["signin::tests::a_request_that_is_no_refresh_of_a_placeholder_passes"]),
    ("signin: a minting path read widely", SIGNIN,
     "let path = loose(path(target));", "let path = path(target).to_string();",
     NET + ["signin::tests::a_sign_ins_token_is_never_filled_toward_an_endpoint_that_mints_a_key"]),
    ("signin: a placeholder never prints", SIGNIN,
     '.field("access", &format_args!("{} bytes", self.access.len()))',
     '.field("access", &self.access)',
     NET + ["signin::tests::no_token_or_placeholder_prints"]),
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
