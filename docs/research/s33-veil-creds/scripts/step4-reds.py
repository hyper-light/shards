"""Step 4's R10 reds, each shown once: a guard removed by hand (the replacement below), its
one test run and expected to fail, the file restored. Run from the repository's root."""
import pathlib
import re
import subprocess
import sys

NET = ["cargo", "test", "-p", "shards-net", "--lib", "--", "--exact"]
API = ["cargo", "test", "-p", "shards-api", "--lib", "--", "--exact"]
SHARDS = ["cargo", "test", "-p", "shards", "--bin", "shards", "--", "--exact"]

LEASE = "crates/net/src/lease.rs"
NETLIB = "crates/net/src/lib.rs"
SESSION = "crates/api/src/image/session.rs"
IMAGE = "crates/api/src/image.rs"
WIRE = "crates/api/src/image/wire.rs"
RUN = "crates/shards/src/api_run.rs"

REDS = [
    ("net: no grant asks nothing", LEASE,
     "        if !self.granted.iter().any(|g| g == reference) {\n"
     "            return Asked::Refused(Refusal::Ungranted);\n"
     "        }\n", "",
     NET + ["lease::tests::a_run_with_no_grant_asks_nothing"]),
    ("net: one ask for every waiting flow", LEASE,
     "        if !self.each.contains_key(reference) {\n            let id = self.fresh();",
     "        if true {\n            let id = self.fresh();",
     NET + ["lease::tests::a_value_is_asked_on_first_use_and_held_while_its_lease_lives"]),
    ("net: released at its lease's end", LEASE,
     "        self.each.retain(|_, l| !l.lapsed(now));",
     "        self.each.retain(|_, _| true);",
     NET + ["lease::tests::a_value_is_asked_on_first_use_and_held_while_its_lease_lives"]),
    ("net: the run's end releases every lease", LEASE,
     "            if let Lease::Asked { waiting, .. } = lease {\n"
     "                self.resume(&reference, waiting, &Err(Refusal::Ended));\n"
     "            }\n",
     "            match lease {\n"
     "                Lease::Asked { waiting, .. } => self.resume(&reference, waiting, &Err(Refusal::Ended)),\n"
     "                held => {\n                    self.each.insert(reference, held);\n                }\n"
     "            }\n",
     NET + ["lease::tests::the_runs_end_releases_every_lease"]),
    ("net: the run host's control alone answers a lease", NETLIB,
     "                    | shards_ipc::kind::NET_CREDENTIALS\n                    | shards_ipc::kind::NET_LEASE\n",
     "                    | shards_ipc::kind::NET_CREDENTIALS\n",
     NET + ["tests::each_control_socket_says_only_its_own"]),
    ("net: no control, the run's leases end", NETLIB,
     "    if !sent {\n        c.leases.end();\n    }\n", "",
     NET + ["tests::a_lease_is_asked_on_the_run_hosts_control_and_taken_as_its_answer"]),
    ("api: a lease of no life refused", WIRE,
     "                let lives = NonZeroU64::new(r.u64()?).ok_or(WireError::Tag {\n"
     "                    what: \"a lease's life\",\n"
     "                    tag: 0,\n"
     "                })?;\n",
     "                let lives = NonZeroU64::new(r.u64()?).unwrap_or(NonZeroU64::MIN);\n",
     API + ["image::wire::tests::leases_round_trip_and_bad_ones_are_refused"]),
    ("api: grants need a lessor", IMAGE,
     "    if !credentials.is_empty() && spec.lessor.is_none() {",
     "    if false {",
     API + ["image::tests::a_run_with_credential_grants_needs_a_lessor"]),
    ("api: released as the host says", SESSION,
     "                .map(|()| {\n                    release(shared);\n                    false\n                })\n",
     "                .map(|()| false)\n",
     API + ["image::session::tests::a_runs_leases_are_given_as_asked_and_released_at_each_end"]),
    ("api: released as the channel ends", SESSION,
     "    release(shared);\n    let mut state = lock(&shared.state);\n    if let Some(why) = failed {",
     "    let mut state = lock(&shared.state);\n    if let Some(why) = failed {",
     API + ["image::session::tests::a_runs_leases_are_given_as_asked_and_released_at_each_end"]),
    ("api: a lease given late released at once", SESSION,
     "    drop(state);\n    lessor.release(&leased.lease);\n    Err(refused)",
     "    drop(state);\n    Err(refused)",
     API + ["image::session::tests::a_lease_given_after_the_runs_release_is_released_as_it_comes"]),
    ("api: an ungranted ask breaks the channel", SESSION,
     "    if !shared.granted.contains(&ask.reference) {\n"
     "        return Err(format!(\n"
     "            \"a lease of {}, which no grant of the run names\",\n"
     "            ask.reference\n"
     "        ));\n"
     "    }\n", "",
     API + ["image::session::tests::a_lease_the_run_was_not_granted_breaks_the_channel"]),
    ("run host: no grant, no lease", RUN,
     "        if !self.granted.iter().any(|g| g == reference) {\n"
     "            return Err(Refusal::Ungranted);\n"
     "        }\n", "",
     SHARDS + ["api_run::tests::a_runs_ledger_holds_one_lease_of_a_credential_and_releases_them_all"]),
    ("run host: one lease of a credential at a time", RUN,
     "        if self.live.values().any(|e| e.reference == reference) {\n"
     "            return Err(Refusal::Live);\n"
     "        }\n", "",
     SHARDS + ["api_run::tests::a_runs_ledger_holds_one_lease_of_a_credential_and_releases_them_all"]),
    ("run host: nothing asked after the release", RUN,
     "        if self.released {\n            return Err(Refusal::Ended);\n        }\n", "",
     SHARDS + ["api_run::tests::a_runs_ledger_holds_one_lease_of_a_credential_and_releases_them_all"]),
    ("run host: a frame wiped as it is let go of", RUN,
     "        zeroize::Zeroize::zeroize(&mut *self.payload);\n", "",
     SHARDS + ["api_run::tests::a_leased_frame_leaves_nothing_of_its_value_in_the_channels_buffer"]),
    ("run host: every end releases its leases", RUN,
     "    if let Some((held, asked)) = leasing.and_then(|l| lock(&l.ledger).release()) {\n"
     "        say(format_args!(\"{}\", Audit::Released { held, asked }));\n"
     "        let _ = answers.send(wire::to_client::RELEASED, &[]);\n"
     "    }\n",
     "    let _ = leasing;\n",
     SHARDS + ["api_run::tests::a_granted_runs_lease_is_relayed_on_first_use_and_released_as_it_ends"]),
]

KEEP = re.compile(r"^(test result|test .* FAILED|thread '.*' .*panicked at|error(\[|:))")


def main() -> int:
    bad = 0
    for name, path, old, new, cmd in REDS:
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
    print(f"STEP reds: {len(REDS) - bad} of {len(REDS)} red", flush=True)
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
