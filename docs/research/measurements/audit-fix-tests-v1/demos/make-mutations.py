# Writes one patch per guard: applied with `patch -p1`, it puts back the code the finding
# replaced (or removes the new guard), so the guard's own test must fail for its reason.
# Run from the worktree root: python3 -I <this> <output dir>
import difflib
import os
import sys

out = sys.argv[1]
os.makedirs(out, exist_ok=True)
MUTATIONS = {
    "m01-identity-check-blind": (
        "crates/shards/tests/common/managed_capture.rs",
        "        .filter(|socket| ours.contains_key(&socket.inode) || listed.contains(&socket.inode))\n",
        "        .filter(|socket| false && (ours.contains_key(&socket.inode) || listed.contains(&socket.inode)))\n",
    ),
    "m03-status-only-verdict": (
        "crates/shards/tests/managed_network.rs",
        """    assert_eq!(run.status, Some(0), "{what}: {run}");
    let lines = || run.stdout.lines().map(str::trim_end);
    assert!(
        !lines().any(|line| line.contains("SHARDS-TEST FAIL")),
        "{what}: the guest's verdict is FAIL: {run}"
    );
    assert_eq!(
        lines().filter(|line| *line == "SHARDS-TEST PASS").count(),
        1,
        "{what}: exactly one guest PASS line: {run}"
    );
""",
        """    assert_eq!(run.status, Some(0), "{what}: {run}");
""",
    ),
    "m06-late-write-no-retry": (
        "crates/testguest/src/managed_network/intercept/tcp_forward.rs",
        """        match writer.write(bytes) {
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) =>
            {
                Err(error)
            }
            answer => Ok(answer),
        }
""",
        """        Ok(writer.write(bytes))
""",
    ),
    "m08-next-hides-driver": (
        "crates/shards/tests/common/managed_capture.rs",
        """        match self
            .events
            .recv_timeout(until.saturating_duration_since(Instant::now()))
        {
            Ok(event) => event,
            // The handler holds the event sender and drops with the driver once it ends.
            Err(mpsc::RecvTimeoutError::Disconnected) => match self.outcome(Duration::ZERO) {
                Some(Ok(())) => panic!("observer events ended before deadline: driver ended Ok"),
                Some(Err(error)) => panic!("observer events ended before deadline: driver {error}"),
                None => panic!("observer events ended before deadline: driver result not yet sent"),
            },
            Err(error) => panic!("observer event before deadline: {error}"),
        }
""",
        """        self.events
            .recv_timeout(until.saturating_duration_since(Instant::now()))
            .unwrap_or_else(|error| panic!("observer event before deadline: {error}"))
""",
    ),
    "m09-no-positive-control": (
        "crates/shards/tests/managed_output/tenant_exit.rs",
        """    if !of(neighbor).iter().any(|row| !zombie(row)) {
        return Err(
            "no live row of the continuing neighbor: the snapshot cannot show the ended member's absence"
                .into(),
        );
    }
""",
        "",
    ),
    "m10-udp-accepts-local-origins": (
        "crates/testguest/src/managed_network/intercept/udp.rs",
        """            "local" | "forward" => {}
            "local-loopback" | "local-self" => return Err(invalid(SAME_NAMESPACE_REPLY)),
""",
        """            "local" | "forward" | "local-loopback" | "local-self" => {}
""",
    ),
    "m12-gate-admits-any-peer": (
        "crates/shards/tests/managed_output/broker_effects.rs",
        "            Ok((peer, address)) if ours(address.ip()) => return (peer, address, refused),\n",
        "            Ok((peer, address)) if ours(address.ip()) || true => return (peer, address, refused),\n",
    ),
    "m13-abort-is-a-stop": (
        "crates/testguest/src/managed_network/intercept.rs",
        """                        Err(e)
                            if e.kind() == io::ErrorKind::ConnectionAborted
                                && deadline
                                    .cancel
                                    .is_some_and(|cancel| cancellation_observed(&cancel)) =>
                        {
                            return Ok(());
                        }
""",
        """                        Err(e) if e.kind() == io::ErrorKind::ConnectionAborted => return Ok(()),
""",
    ),
}
for name, (path, old, new) in MUTATIONS.items():
    with open(path) as f:
        text = f.read()
    assert text.count(old) == 1, (name, text.count(old))
    mutated = text.replace(old, new)
    diff = difflib.unified_diff(
        text.splitlines(keepends=True),
        mutated.splitlines(keepends=True),
        fromfile=f"a/{path}",
        tofile=f"b/{path}",
    )
    with open(os.path.join(out, f"{name}.patch"), "w") as f:
        f.writelines(diff)
    print(name, path)
