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
