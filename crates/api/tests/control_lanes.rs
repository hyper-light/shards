//! An orchestrator steers a run's members through `shards_api::control` alone (D238, D252):
//! every type a member's lane gives or refuses with, and the admissions its controller dials
//! with, are reachable without naming shards-remote. A compile test: it fails to build where
//! one is no longer re-exported.

use shards_api::control::{
    Admissions, Execs, Fault, Member, MemberControl, MemberEnd, MemberSteering, Subscriptions, Unstarted,
    Unsteered, exec_admissions,
};

/// What an orchestrator does with a member's lane: signal it, then wait for how it ended.
fn signal_and_wait(execs: &Execs, member: u32, signal: u32) -> Result<i32, Fault> {
    match execs.member(member)? {
        Member::Ended(status) => Ok(status),
        Member::Running(MemberControl { mut steering, end }) => {
            steer(&mut steering, signal)?;
            wait(end)
        }
    }
}

fn steer(steering: &mut MemberSteering, signal: u32) -> Result<(), Fault> {
    match steering.signal(signal) {
        // A member whose command has ended already: its end says how.
        Ok(()) | Err(Fault::Unsteered(Unsteered::Ended)) => Ok(()),
        Err(other) => Err(other),
    }
}

fn wait(mut end: MemberEnd) -> Result<i32, Fault> {
    end.wait()
}

/// Whether a lane's refusal says its member runs no program (D238), as a caller matches it.
fn runs_nothing(fault: &Fault) -> bool {
    matches!(fault, Fault::Unstarted(Unstarted::Refused))
}

#[test]
fn a_members_lane_is_steered_through_shards_api_alone() {
    let admissions: Admissions = exec_admissions();
    let subscriptions = Subscriptions {
        outputs: &[],
        inputs: &[],
        execs: Some(admissions),
        sockets: None,
    };
    assert!(subscriptions.execs.is_some());
    assert!(runs_nothing(&Fault::Unstarted(Unstarted::Refused)));
    assert!(!runs_nothing(&Fault::Unstarted(Unstarted::Failed)));
    let _: fn(&Execs, u32, u32) -> Result<i32, Fault> = signal_and_wait;
}
