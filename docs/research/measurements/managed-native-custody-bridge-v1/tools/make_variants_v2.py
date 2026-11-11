"""v2: route all six new custody StartFailure variants to RefusedDomain (runtime owner
decision). Derive GREEN-v2, RED-v10d, RED-v1 and MUTANT-v2 run.rs with ONE final test text."""
import hashlib, json, sys
from pathlib import Path
m = Path(sys.argv[1])
sha = lambda t: hashlib.sha256(t.encode()).hexdigest()
red = (m / 'candidate-red/crates/init/src/run.rs').read_text()
v1 = (m / 'variants/green/run.rs').read_text()
assert sha(red) == 'f765d3308bee8afac9a6305a3f26113fd6a5af85cd1bde9426087525a04f8a86'
assert sha(v1) == 'd1a383f8d2942a62d06910245b516f32089a6c3e0c5285b95fac7115e14d5976'

def once(text, old, new):
    assert text.count(old) == 1, (old[:90], text.count(old))
    return text.replace(old, new)

tests = '''    #[test]
    fn member_start_refusals_report_system_error_and_not_run() {
        let (conn, mut reader) = connection();
        let refusals = [
            crate::domains::StartFailure::Clone {
                member: 7,
                actual: -1,
                cause: io::Error::from_raw_os_error(libc::EPERM),
            },
            crate::domains::StartFailure::Pid {
                member: 8,
                actual: libc::c_long::MAX,
            },
            crate::domains::StartFailure::StartupState,
            crate::domains::StartFailure::StartupAllocation,
            crate::domains::StartFailure::MetadataAllocation { member: 10 },
        ];
        let mut answers = Vec::new();
        for cause in refusals {
            let expected = run::render_error(&cause);
            let failure = domain_start_failed(cause, &Spec::default());
            let answer = report_startup_failure(&conn, &failure);
            if answer.is_some() {
                assert_eq!(system_error(&mut reader), expected.as_bytes());
            }
            answers.push(answer);
        }
        assert_eq!(
            answers,
            [Some(NOT_RUN); 5],
            "typed member refusal must answer SYSTEM_ERR with NOT_RUN"
        );
        let unknown = domain_start_failed(crate::domains::StartFailure::MissingNativeOwner, &Spec::default());
        assert_eq!(report_startup_failure(&conn, &unknown), None);
        reader.set_nonblocking(true).unwrap();
        let mut byte = [0];
        assert_eq!(
            reader.read(&mut byte).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }
'''
case1_start = '    #[test]\n    fn rejected_parent_group_survives_system_error_until_owner_drop()'
v1_case1 = v1[v1.index(case1_start):]
def with_tests(base):
    start = base.index('    #[test]\n    fn clone_and_pid_refusals_report_system_error_and_not_run()')
    out = base[:start] + tests + '\n' + v1_case1
    assert out.endswith('}\n}\n')
    return out

red_v10d = with_tests(red)
red_v1 = with_tests(v1)
green = once(red_v1, '''    /// A member's typed start refusal whose untyped predecessor answered
    /// SYSTEM_ERR with NOT_RUN. The original failure, including a rejected
    /// parent group, stays owned here until power_off; reporting borrows it.
''', '''    /// A member's typed start refusal. SYSTEM_ERR with NOT_RUN says the run
    /// failed to start, not that no member was born or that cleanup finished.
    /// The original failure, including a rejected parent group, stays owned
    /// here until power_off; reporting borrows it.
''')
green = once(green, '''        failure @ (crate::domains::StartFailure::Clone { .. }
        | crate::domains::StartFailure::Pid { .. }''', '''        failure @ (crate::domains::StartFailure::StartupState
        | crate::domains::StartFailure::StartupAllocation
        | crate::domains::StartFailure::MetadataAllocation { .. }
        | crate::domains::StartFailure::Clone { .. }
        | crate::domains::StartFailure::Pid { .. }''')
mutant = once(green, ') => StartupFailure::RefusedDomain(failure),', ') => StartupFailure::from(run::render_error(&failure)),')
out = {}
for name, text in [('green-v2', green), ('red-v10d', red_v10d), ('red-v1', red_v1), ('mutant-v2', mutant)]:
    d = m / 'variants' / name
    d.mkdir(parents=True, exist_ok=False)
    (d / 'run.rs').write_text(text)
    out[name] = sha(text)
print(json.dumps(out, indent=2))
