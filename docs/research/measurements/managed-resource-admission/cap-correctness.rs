// Included only as a private cfg(test) child of the exact admission module.
// Ordinary-file IO/state checks, not a kernel cgroup or actor-release witness.
use super::*;

fn controls(dir: &std::path::Path, omitted: &str) {
    for (name, value) in [
        ("pids.max", "0\n"),
        ("memory.max", "max\n"),
        ("memory.oom.group", "0\n"),
        ("cgroup.kill", ""),
        ("cgroup.events", "populated 0\n"),
    ] {
        if name != omitted {
            std::fs::write(dir.join(name), value).unwrap();
        }
    }
}

#[test]
fn unavailable_controller_refuses_before_subtree_write() {
    let dir = shards_testdir::TempDir::new("cap-controller").unwrap();
    let parent = File::open(&dir).unwrap();
    std::fs::write(dir.join("cgroup.controllers"), "pids\n").unwrap();
    std::fs::write(dir.join("cgroup.subtree_control"), "sentinel\n").unwrap();
    let refusal = enable(&parent).unwrap_err();
    assert_eq!(refusal.cause, Cause::ControllerUnavailable);
    assert_eq!(refusal.file, c"cgroup.controllers");
    assert_eq!(
        std::fs::read(dir.join("cgroup.subtree_control")).unwrap(),
        b"sentinel\n"
    );
    std::fs::write(dir.join("cgroup.controllers"), "memory pids\n").unwrap();
    std::fs::write(dir.join("cgroup.subtree_control"), "pids memory\n").unwrap();
    enable(&parent).unwrap();
    assert_eq!(
        std::fs::read(dir.join("cgroup.subtree_control")).unwrap(),
        b"pids memory\n"
    );
}

#[test]
fn each_missing_lifetime_control_prevents_commit() {
    for omitted in ["cgroup.kill", "cgroup.events"] {
        let dir = shards_testdir::TempDir::new("cap-controls").unwrap();
        let parent = File::open(&dir).unwrap();
        let group = Group::create(&parent, "member", false).unwrap();
        controls(&dir.join("member"), omitted);
        let refusal = Prepared { group, page: 4096 }.commit(Some(0), None).unwrap_err();
        assert_eq!(refusal.cause, Cause::Io { errno: libc::ENOENT });
        assert_eq!(refusal.file.to_bytes(), omitted.as_bytes());
        // Regular fixture files remain; never pretend this was a removed cgroup.
        assert_eq!(refusal.rollback, Rollback::Failed(libc::ENOTEMPTY));
    }
}

#[test]
fn explicit_zero_task_cap_stays_zero_after_commit() {
    let dir = shards_testdir::TempDir::new("cap-zero").unwrap();
    let parent = File::open(&dir).unwrap();
    let group = Group::create(&parent, "member", false).unwrap();
    controls(&dir.join("member"), "");
    let committed = Prepared { group, page: 4096 }.commit(Some(0), None).unwrap();
    verify(committed.group(), c"pids.max", Limit::Value(0)).unwrap();
    assert_eq!(
        verify(committed.group(), c"pids.max", Limit::Max)
            .unwrap_err()
            .cause,
        Cause::Readback
    );
    // No actor is launched; kernel denial at zero is a separate root-owned gate.
}

#[test]
fn oversized_or_overflowing_scalar_never_becomes_max_or_zero() {
    let dir = shards_testdir::TempDir::new("cap-scalar").unwrap();
    let parent = File::open(&dir).unwrap();
    std::fs::write(dir.join("pids.max"), "1".repeat(NUMBER_BYTES + 2)).unwrap();
    assert_eq!(
        read_limit(&parent, c"pids.max").unwrap_err().cause,
        Cause::InvalidNumber
    );
    std::fs::write(dir.join("pids.max"), "18446744073709551616\n").unwrap();
    assert_eq!(
        read_limit(&parent, c"pids.max").unwrap_err().cause,
        Cause::Overflow
    );
    std::fs::write(dir.join("pids.max"), format!("{}\n", u64::MAX)).unwrap();
    assert_eq!(read_limit(&parent, c"pids.max").unwrap(), Limit::Value(u64::MAX));
}

#[test]
fn creation_and_rollback_follow_held_parent_after_path_replacement() {
    let dir = shards_testdir::TempDir::new("cap-held").unwrap();
    std::fs::create_dir(dir.join("parent")).unwrap();
    let held = File::open(dir.join("parent")).unwrap();
    std::fs::rename(dir.join("parent"), dir.join("original")).unwrap();
    std::fs::create_dir(dir.join("parent")).unwrap();
    let group = Group::create(&held, "member", false).unwrap();
    assert!(dir.join("original/member").is_dir());
    assert!(!dir.join("parent/member").exists());
    drop(group);
    assert!(!dir.join("original/member").exists());
    assert!(dir.join("parent").is_dir());
}

#[test]
fn adopted_existing_parent_is_never_rolled_back_as_new() {
    let dir = shards_testdir::TempDir::new("cap-existing").unwrap();
    let parent = File::open(&dir).unwrap();
    std::fs::create_dir(dir.join("domains")).unwrap();
    let group = Group::create(&parent, "domains", true).unwrap();
    let refusal = group.refused(Refusal::new(c"memory.high", Cause::Readback));
    assert_eq!(refusal.rollback, Rollback::NotCreated);
    assert!(dir.join("domains").is_dir());
}
