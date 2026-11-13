//! An orchestrator steers a run's members through `shards_api::control` alone (D238, D252):
//! every type a member's lane gives or refuses with, its typed end among them
//! (`exec::End`), and the admissions its controller dials with, are reachable without naming
//! shards-remote. A compile test: it fails to build where
//! one is no longer re-exported.

use shards_api::control::{
    Admissions, ExecControl, Execs, Fault, Got, Input, McpConnection, Member, MemberControl, MemberEnd,
    MemberSteering, Output, Subscriptions, Unstarted, Unsteered, exec, exec_admissions,
};

/// What an orchestrator does with a member's lane: signal it, then wait for how it ended.
fn signal_and_wait(execs: &Execs, member: u32, signal: u32) -> Result<exec::End, Fault> {
    match execs.member(member)? {
        Member::Ended(end) => Ok(end),
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

fn wait(mut end: MemberEnd) -> Result<exec::End, Fault> {
    end.wait()
}

/// Whether a member died by `signal`, apart from an exit of 128 plus it.
fn killed_by(end: exec::End, signal: u32) -> bool {
    end == exec::End::Signaled(signal)
}

/// What an orchestrator reads of a member's MCP connection (D254): the harness's first record.
fn first_said(execs: &Execs, member: u32) -> Result<Vec<u8>, Fault> {
    let McpConnection { mut from, to, end } = execs.mcp(member)?;
    let (_, _): (&Input, &ExecControl) = (&to, &end);
    let from: &mut Output = &mut from;
    match from.next_record()? {
        Got::Record { bytes, .. } => Ok(bytes),
        Got::Finished => Ok(Vec::new()),
    }
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
    let _: fn(&Execs, u32, u32) -> Result<exec::End, Fault> = signal_and_wait;
    let _: fn(&Execs, u32) -> Result<Vec<u8>, Fault> = first_said;
    assert!(killed_by(exec::End::Signaled(15), 15));
    assert!(!killed_by(exec::End::Exited(143), 15));
}
