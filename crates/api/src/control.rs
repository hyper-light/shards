//! The controller of a run's agent-control endpoint (D238, D252): Shards' own, re-exported, so
//! that an orchestrator dialing a run's endpoint writes no second implementation of its wire
//! ("a controller's side is Shards' code", D238). [`crate::image::ImageGuest::control`] says
//! where a run's endpoint is bound and which key to pin; [`Identity::generate`] makes the
//! controller's own key, whose [`Identity::fingerprint`] [`crate::image::ControlAsk`] names.
//!
//! - **Its link.** TLS 1.3 under `TLS13_AES_256_GCM_SHA384` alone, both ways (D239): a
//!   connection's [`Linked::wait`] says what each end negotiated.
//! - **Its socket lanes** (D251). [`Sockets::connect`] opens a lane to one of the run's own
//!   command's sockets by its name in the run's socket directory; a [`Socket`] is that socket's
//!   bytes both ways, each way's end its half-close.
//! - **Its members' own lanes** (D238). [`Execs::member`] gives a member's [`Member`]: its
//!   [`MemberSteering`] signals its command, each signal answered (or refused, typed
//!   [`Unsteered`]), and its [`MemberEnd`] gives how it ended, typed ([`exec::End`]: its exit's
//!   status, or the signal that killed it). [`exec_admissions`] is what
//!   [`Subscriptions::execs`] names.
//! - **Its members' MCP connections** (D254). [`Execs::mcp`] gives the next connection a
//!   member's harness makes to its MCP bridge, a [`McpConnection`]: the harness's bytes, as
//!   records ([`Output`], [`Got`]); the controller's to it ([`Input`]); and its end
//!   ([`ExecControl`]).

pub use shards_remote::control::exec::{Admissions, Unstarted, Unsteered};
pub use shards_remote::control::{
    Controller, Dialed, Ended, ExecControl, Execs, Fault, Got, Hangup, Input, Link, Linked, McpConnection,
    Member, MemberControl, MemberEnd, MemberSteering, Output, Socket, SocketAdmission, SocketReader,
    SocketWriter, Sockets, Subscriptions, exec,
};
pub use shards_remote::identity::{Fingerprint, Identity};
pub use shards_remote::managed::Reason;

use shards_abi::run::CHUNK;
use shards_remote::managed::{Admitted, DATA_HEAD, DATAGRAM_BYTES, input};

/// The most one TLS handshake message is on an agent-control connection: RFC 8446 §4,
/// Handshake's uint24 length and its four-byte type/length head.
pub const HANDSHAKE_BYTES: usize = (1 << 24) - 1 + 4;

/// The most one input record carries: one `STDIN` frame to the member's init, which carries
/// at most what the guest reads of a stream at once (`run::CHUNK`), as a command's and an
/// exec's stdin frames do.
pub const INPUT_ROOM: usize = CHUNK;

// A window is a usize sum widened to u64: lossless where usize is at most 64 bits, as on
// every Rust target, and a sum that overflows fails the build, not the run.
const _: () = assert!(usize::BITS <= u64::BITS);
const OUTPUT_WINDOW: u64 = (CHUNK + DATA_HEAD) as u64;
const INPUT_WINDOW: u64 = input::TAKEN_BYTES as u64;

/// What a run's endpoint admits each exec's, MCP connection's and member lane's streams (D238,
/// D254), the daemon's runs' and an orchestrator's alike, and what a controller's
/// [`Subscriptions::execs`] names to dial it: an output's record is what one of its guest's
/// output frames carries at most (`run::CHUNK`), its window that record with its data frame's
/// head, as a member's is; its stdin's is a member's stdin's ([`INPUT_ROOM`], its taken
/// frames' window). Their handshake and datagrams are a member stream's connection's.
pub fn exec_admissions() -> exec::Admissions {
    let link = |record_bytes, transport_bytes| Admitted {
        record_bytes,
        transport_bytes,
        handshake_bytes: HANDSHAKE_BYTES,
        datagram_bytes: DATAGRAM_BYTES,
    };
    exec::Admissions {
        output: link(CHUNK, OUTPUT_WINDOW),
        input: link(INPUT_ROOM, INPUT_WINDOW),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One source for what an endpoint admits and a controller names: each bound is its
    /// protocol's (TLS 1.3's handshake, the guest's chunk, the stream frames' heads).
    #[test]
    fn exec_admissions_are_the_run_protocols_bounds() {
        let admitted = exec_admissions();
        assert_eq!(admitted.output.record_bytes, 64 * 1024);
        assert_eq!(
            admitted.output.transport_bytes,
            u64::try_from(CHUNK + DATA_HEAD).unwrap()
        );
        assert_eq!(admitted.input.record_bytes, CHUNK);
        assert_eq!(
            admitted.input.transport_bytes,
            u64::try_from(input::TAKEN_BYTES).unwrap()
        );
        for link in [admitted.output, admitted.input] {
            assert_eq!(link.handshake_bytes, 16_777_219);
            assert_eq!(link.datagram_bytes, DATAGRAM_BYTES);
        }
    }
}
