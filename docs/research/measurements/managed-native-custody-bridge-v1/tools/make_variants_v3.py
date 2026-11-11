"""v3 (runtime owner F10 decision): every pre-birth typed refusal answers SYSTEM_ERR + NOT_RUN;
storage shutdown covers RefusedDomain too. Derive GREEN-v3 and RED-v2 (v2 production, v3 tests)."""
import hashlib, json, sys
from pathlib import Path
m = Path(sys.argv[1])
sha = lambda t: hashlib.sha256(t.encode()).hexdigest()
v2 = (m / 'variants/green-v2/run.rs').read_text()
assert sha(v2) == 'dff63046236f145c9034af4b481fe1e0e6f756bd9a50798ac9e52e63960a7c4f'
def once(text, old, new):
    assert text.count(old) == 1, (old[:90], text.count(old))
    return text.replace(old, new)
# Tests: the unreported control becomes a post-birth failure; add the pre-birth case.
tests = once(v2, '''        let unknown = domain_start_failed(crate::domains::StartFailure::MissingNativeOwner, &Spec::default());''',
'''        let unknown = domain_start_failed(
            crate::domains::StartFailure::NativeOwner {
                member: 11,
                cause: crate::managed::NativeFailure::State,
            },
            &Spec::default(),
        );''')
case1 = '    #[test]\n    fn rejected_parent_group_survives_system_error_until_owner_drop()'
tests = once(tests, case1, '''    #[test]
    fn pre_birth_refusals_report_system_error_and_not_run() {
        let (conn, mut reader) = connection();
        let refusals = [
            crate::domains::StartFailure::MissingStartup,
            crate::domains::StartFailure::MissingNativeOwner,
            crate::domains::StartFailure::OwnerPreparation {
                member: 12,
                cause: crate::managed::PreparationFailure::State,
            },
            crate::domains::StartFailure::InputPreparation {
                member: 13,
                cause: crate::input::startup::Failure::Member,
            },
            crate::domains::StartFailure::HelperPreparation {
                member: 14,
                cause: crate::domains::HelperPreparationFailure::Map(shards_ipc::child_fds::Failure::Capacity),
            },
            crate::domains::StartFailure::Preparation {
                member: 15,
                cause: crate::domains::DeclaredPathFailure::Nul,
            },
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
            [Some(NOT_RUN); 6],
            "pre-birth refusal must answer SYSTEM_ERR with NOT_RUN"
        );
    }

''' + case1)
red_v2 = tests
green = once(tests, '''    /// A member's typed start refusal. SYSTEM_ERR with NOT_RUN says the run
    /// failed to start, not that no member was born or that cleanup finished.
    /// The original failure, including a rejected parent group, stays owned
    /// here until power_off; reporting borrows it.''', '''    /// A member's typed start refusal. SYSTEM_ERR with NOT_RUN says the run
    /// failed to start, not that no member was born or that cleanup finished.
    /// The original failure, including a rejected parent group or retained
    /// native storage, stays owned here until power_off; reporting borrows it.''')
green = once(green, '''        failure @ (crate::domains::StartFailure::StartupState''', '''        failure @ (crate::domains::StartFailure::MissingStartup
        | crate::domains::StartFailure::MissingNativeOwner
        | crate::domains::StartFailure::OwnerPreparation { .. }
        | crate::domains::StartFailure::HelperPreparation { .. }
        | crate::domains::StartFailure::Preparation { .. }
        | crate::domains::StartFailure::NativePreparation { .. }
        | crate::domains::StartFailure::InputPreparation { .. }
        | crate::domains::StartFailure::StartupState''')
green = once(green, '''    if let Err(StartupFailure::UnreportedDomain(failure)) = &mut outcome {''',
             '''    if let Err(StartupFailure::UnreportedDomain(failure) | StartupFailure::RefusedDomain(failure)) = &mut outcome {''')
out = {}
for name, text in [('green-v3', green), ('red-v2-on-v3-tests', red_v2)]:
    d = m / 'variants' / name; d.mkdir(parents=True, exist_ok=False)
    (d / 'run.rs').write_text(text); out[name] = sha(text)
print(json.dumps(out, indent=2))
