"""Derive GREEN, RED-v2 and MUTANT run.rs from the exact red run.rs (f765d330)."""
import hashlib, json, shutil, sys
from pathlib import Path
m = Path(sys.argv[1])
red_path = m / 'candidate-red/crates/init/src/run.rs'
red = red_path.read_text()
assert hashlib.sha256(red.encode()).hexdigest() == 'f765d3308bee8afac9a6305a3f26113fd6a5af85cd1bde9426087525a04f8a86'

def once(text, old, new):
    assert text.count(old) == 1, (old[:80], text.count(old))
    return text.replace(old, new)

# Tests: observe the rejected group's lifetime through its socket peer, not a
# reusable descriptor number, and give each guard a distinct failure message.
old_tests = red[red.index('    #[test]\n    fn clone_and_pid_refusals_report_system_error_and_not_run()'):]
new_tests = '''    #[test]
    fn clone_and_pid_refusals_report_system_error_and_not_run() {
        let (conn, mut reader) = connection();
        for cause in [
            crate::domains::StartFailure::Clone {
                member: 7,
                actual: -1,
                cause: io::Error::from_raw_os_error(libc::EPERM),
            },
            crate::domains::StartFailure::Pid {
                member: 8,
                actual: libc::c_long::MAX,
            },
        ] {
            let expected = run::render_error(&cause);
            let failure = domain_start_failed(cause, &Spec::default());
            assert_eq!(
                report_startup_failure(&conn, &failure),
                Some(NOT_RUN),
                "typed member refusal must answer SYSTEM_ERR with NOT_RUN"
            );
            assert_eq!(system_error(&mut reader), expected.as_bytes());
        }
        let unknown = domain_start_failed(crate::domains::StartFailure::MissingNativeOwner, &Spec::default());
        assert_eq!(report_startup_failure(&conn, &unknown), None);
        reader.set_nonblocking(true).unwrap();
        let mut byte = [0];
        assert_eq!(
            reader.read(&mut byte).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn rejected_parent_group_survives_system_error_until_owner_drop() {
        let (group, mut peer) = UnixStream::pair().unwrap();
        peer.set_nonblocking(true).unwrap();
        let mut pending = crate::managed::PendingMember::new();
        // The real vacant-phase guard rejects the actual descriptor before
        // inspecting the PID; no member/child/cgroup or authority is fabricated.
        let cause = match pending.parent(0, File::from(OwnedFd::from(group))) {
            Ok(_) => panic!("vacant custody accepted a parent"),
            Err(cause) => cause,
        };
        assert!(pending.vacant());
        let original = crate::domains::StartFailure::OwnerParent { member: 9, cause };
        let expected = run::render_error(&original);
        let failure = domain_start_failed(original, &Spec::default());
        let (conn, mut reader) = connection();
        assert_eq!(
            report_startup_failure(&conn, &failure),
            Some(NOT_RUN),
            "typed member refusal must answer SYSTEM_ERR with NOT_RUN"
        );
        assert_eq!(system_error(&mut reader), expected.as_bytes());
        // The peer reads EOF only once the socket itself closes: this observes
        // the rejected group's object, never a reusable descriptor number.
        let mut byte = [0];
        assert_eq!(
            peer.read(&mut byte).map_err(|e| e.kind()),
            Err(io::ErrorKind::WouldBlock),
            "rejected group was released before its startup failure"
        );
        drop(failure);
        assert_eq!(
            peer.read(&mut byte).map_err(|e| e.kind()),
            Ok(0),
            "dropping the startup failure must release the rejected group"
        );
    }
}
'''
red_v2 = red.replace(old_tests, new_tests)
assert red_v2 != red and red_v2.endswith(new_tests)

variant = '''    UnreportedDomain(crate::domains::StartFailure),
    UnreportedLaunch {'''
green = once(red_v2, variant, '''    /// A member's typed start refusal whose untyped predecessor answered
    /// SYSTEM_ERR with NOT_RUN. The original failure, including a rejected
    /// parent group, stays owned here until power_off; reporting borrows it.
    RefusedDomain(crate::domains::StartFailure),
''' + variant)
route = '''        failure => StartupFailure::UnreportedDomain(failure),
    }
}'''
green = once(green, route, '''        failure @ (crate::domains::StartFailure::Clone { .. }
        | crate::domains::StartFailure::Pid { .. }
        | crate::domains::StartFailure::OwnerParent { .. }) => StartupFailure::RefusedDomain(failure),
''' + route)
report = '''        StartupFailure::Management(report) => {
            let _ = send(conn, kind::MANAGEMENT_REFUSED, &report.encode());
            Some(NOT_RUN)
        }
'''
green = once(green, report, report + '''        StartupFailure::RefusedDomain(failure) => {
            let _ = send(conn, kind::SYSTEM_ERR, run::render_error(failure).as_bytes());
            Some(NOT_RUN)
        }
''')
# MUTANT: same answer, but the owned rendering drops the typed failure (and
# any rejected group) inside domain_start_failed, before reporting/shutdown.
mutant = once(green, ') => StartupFailure::RefusedDomain(failure),',
              ') => StartupFailure::from(run::render_error(&failure)),')
out = {}
for name, text in [('red-v2', red_v2), ('green', green), ('mutant-custody', mutant)]:
    d = m / 'variants' / name
    d.mkdir(parents=True, exist_ok=False)
    (d / 'run.rs').write_text(text)
    out[name] = hashlib.sha256(text.encode()).hexdigest()
print(json.dumps(out, indent=2))
