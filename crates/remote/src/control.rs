//! The agent-control endpoint (plan step 11, D238): one QUIC connection from a microVM's
//! controller (kiln, Hex) to the microVM, its lanes the streams the controller opens on it.
//!
//! - **Who reaches it.** TLS 1.3 under [`tls`]'s provider, TLS13_AES_256_GCM_SHA384 alone both
//!   ways, early data off. The endpoint presents the microVM's own key, an Ed25519 key made where
//!   TLS ends that never leaves there (RFC 7250's raw public key, as D180's homes present
//!   theirs), and takes only the key its controller enrolled; the controller takes only the key
//!   it pinned for that microVM. Either side refuses any other key at the handshake, TLS's
//!   access_denied (RFC 8446 §6.2, the alert this crate's verifiers raise), before QUIC gives
//!   the controller any stream the endpoint would read (RFC 9001 §5.7: no 1-RTT packet of the
//!   client's is processed before the handshake completes): a peer without the enrolled key
//!   reaches nothing.
//! - **Its lanes.** Each lane is a bidirectional stream the controller opens, named by its
//!   first frame ([`lane`]); the endpoint opens none. A member's stdin, stdout and stderr are a
//!   lane each, opened by step 5's subscription, byte for byte (`managed::subscribe`,
//!   `managed::input::subscribe`), and carrying step 5's frames after it, so that a stream
//!   stalled on one member's reader holds up no other lane (RFC 9000 §2.2, §4.1). A lane the
//!   endpoint will not serve is refused alone ([`refused`]): the connection and its other
//!   lanes go on.

use std::mem::size_of;

use hyper_quic::rustls::AlertDescription;
use hyper_quic::{ClientConfig, ConnectionError, ServerConfig, TransportConfig, TransportErrorCode};
use shards_abi::run::kind;

use crate::frame::{self, Tag};
use crate::identity::{Fingerprint, Identity};
use crate::managed::{Reason, RunId};
use crate::tls;

/// The application protocol of a control connection (RFC 7301), apart from a context's
/// (`shards/1`) and the managed output's: a peer offering another is refused at its handshake.
pub const ALPN: &[u8] = b"shards-control/1";

/// The endpoint's side: the microVM's own key presented, and `controller`'s, the key its
/// controller enrolled, the only one taken (D180's `owner_server`).
pub fn endpoint(
    vm: &Identity,
    controller: Fingerprint,
    transport: TransportConfig,
) -> Result<ServerConfig, crate::Error> {
    tls::owner_server(vm, controller, transport, ALPN)
}

/// The controller's side: its own key presented, and the microVM's only where its key's SHA-256
/// is `vm`, the one the controller pinned for it.
pub fn controller(
    own: &Identity,
    vm: Fingerprint,
    transport: TransportConfig,
) -> Result<ClientConfig, crate::Error> {
    tls::client_with(own, vm, transport, ALPN)
}

/// Which side ended a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum By {
    /// This side.
    This,
    /// Its peer.
    Peer,
}

/// Why a control connection ended, read from the codes QUIC carries (RFC 9000 §20, RFC 9001
/// §4.8, which puts a TLS alert in CRYPTO_ERROR's range): what callers match, never its words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// A key that is not the one enrolled or pinned: access_denied, which this crate's verifiers
    /// raise for any key but the pinned one (hyper-tls error.rs: `ApplicationVerificationFailure`).
    NotEnrolled(By),
    /// Nothing in common at the handshake, TLS13_AES_256_GCM_SHA384 among the rest
    /// (handshake_failure, RFC 8446 §4.1.1).
    Handshake(By),
    /// Another application protocol (no_application_protocol, RFC 7301 §3.2).
    Protocol(By),
    /// Closed with a reason of the endpoint's protocol.
    Closed(By, Reason),
    /// Its peer unheard for its idle timeout (RFC 9000 §10.1).
    Silent,
    /// Closed by this side ([`hyper_quic::ConnectionError::LocallyClosed`]).
    Local,
    /// Anything else its transport said: a code of no meaning here, a reset, a version.
    Transport(By),
}

/// What `error` says of why its connection ended.
pub fn ended(error: &ConnectionError) -> Ended {
    let (code, by) = match error {
        ConnectionError::TransportError(e) => (e.code, By::This),
        ConnectionError::ConnectionClosed(close) => (close.error_code, By::Peer),
        ConnectionError::ApplicationClosed(close) => {
            return u32::try_from(close.error_code.into_inner())
                .ok()
                .and_then(|code| Reason::try_from(code).ok())
                .map_or(Ended::Transport(By::Peer), |reason| {
                    Ended::Closed(By::Peer, reason)
                });
        }
        ConnectionError::TimedOut => return Ended::Silent,
        ConnectionError::LocallyClosed => return Ended::Local,
        ConnectionError::VersionMismatch | ConnectionError::Reset => return Ended::Transport(By::Peer),
        ConnectionError::CidsExhausted => return Ended::Transport(By::This),
    };
    let alert = |alert: AlertDescription| TransportErrorCode::crypto(alert.into());
    if code == alert(AlertDescription::AccessDenied) {
        Ended::NotEnrolled(by)
    } else if code == alert(AlertDescription::HandshakeFailure) {
        Ended::Handshake(by)
    } else if code == alert(AlertDescription::NoApplicationProtocol) {
        Ended::Protocol(by)
    } else {
        Ended::Transport(by)
    }
}

/// A member's stream a lane carries: the run protocol's channel numbers (abi `run::kind`), as
/// step 5's subscriptions name them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Stdio {
    Stdin = kind::STDIN,
    Stdout = kind::STDOUT,
    Stderr = kind::STDERR,
}

/// What a lane is for, as its first frame says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    /// A member's stdin, stdout or stderr, from `offset`: the first byte the controller has not
    /// seen taken (stdin) or consumed (output), as step 5's subscription says it.
    Stdio {
        run: RunId,
        generation: u64,
        member: u32,
        stream: Stdio,
        offset: u64,
    },
}

const FRAME_HEAD: usize = size_of::<u8>() + size_of::<u32>();
/// Step 5's subscription after its frame head: the run (32), its generation (u64), the member
/// (u32), the channel (u8) and the offset (u64).
const SUBSCRIPTION: usize =
    size_of::<RunId>() + size_of::<u64>() + size_of::<u32>() + size_of::<u8>() + size_of::<u64>();
/// The most a lane's first frame takes: the endpoint reads no more of a lane before it knows
/// what the lane is.
pub const LANE_MOST: usize = FRAME_HEAD + SUBSCRIPTION;

/// The lane the frame at the front of `bytes` opens, and how many bytes that frame took; none
/// until it has come whole. A frame of another kind or length, or of a channel no member has,
/// is the controller's error ([`Reason::Protocol`]).
pub fn lane(bytes: &[u8]) -> Result<Option<(Lane, usize)>, Reason> {
    let Some((tag, len)) = frame::peek(bytes).map_err(|_| Reason::Protocol)? else {
        return Ok(None);
    };
    if tag != Tag::Exec || len != SUBSCRIPTION {
        return Err(Reason::Protocol);
    }
    let Some(payload) = bytes.get(FRAME_HEAD..LANE_MOST) else {
        return Ok(None);
    };
    let (run, rest) = payload.split_first_chunk::<32>().ok_or(Reason::Protocol)?;
    let (generation, rest) = rest.split_first_chunk::<8>().ok_or(Reason::Protocol)?;
    let (member, rest) = rest.split_first_chunk::<4>().ok_or(Reason::Protocol)?;
    let (channel, rest) = rest.split_first().ok_or(Reason::Protocol)?;
    let offset = rest.first_chunk::<8>().ok_or(Reason::Protocol)?;
    let stream = match *channel {
        kind::STDIN => Stdio::Stdin,
        kind::STDOUT => Stdio::Stdout,
        kind::STDERR => Stdio::Stderr,
        _ => return Err(Reason::Protocol),
    };
    let lane = Lane::Stdio {
        run: RunId(*run),
        generation: u64::from_be_bytes(*generation),
        member: u32::from_be_bytes(*member),
        stream,
        offset: u64::from_be_bytes(*offset),
    };
    Ok(Some((lane, LANE_MOST)))
}

/// The endpoint's refusal of one lane, whole: a `Refused` frame of its reason's code (a
/// big-endian u32, `managed::Reason`'s), then the lane's end. The connection and its other lanes
/// go on.
pub const REFUSED_BYTES: usize = FRAME_HEAD + size_of::<u32>();

/// `reason`'s refusal frame.
pub fn refused(reason: Reason) -> [u8; REFUSED_BYTES] {
    let mut out = [0; REFUSED_BYTES];
    let code = (reason as u32).to_be_bytes();
    let len = (size_of::<u32>() as u32).to_be_bytes();
    for (slot, byte) in out
        .iter_mut()
        .zip(std::iter::once(&(Tag::Refused as u8)).chain(&len).chain(&code))
    {
        *slot = *byte;
    }
    out
}

/// The reason of the refusal frame at the front of `bytes`, once it has come whole; none
/// before. Another frame there is no refusal: `Err` with what it is, for the lane's own reader.
pub fn read_refused(bytes: &[u8]) -> Result<Option<Reason>, Option<Tag>> {
    let Some((tag, len)) = frame::peek(bytes).map_err(|_| None)? else {
        return Ok(None);
    };
    if tag != Tag::Refused || len != size_of::<u32>() {
        return Err(Some(tag));
    }
    let Some(code) = bytes
        .get(FRAME_HEAD..REFUSED_BYTES)
        .and_then(|c| <[u8; 4]>::try_from(c).ok())
    else {
        return Ok(None);
    };
    Reason::try_from(u32::from_be_bytes(code))
        .map(Some)
        .map_err(|_| None)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use hyper_quic::rustls::CipherSuite;

    use super::*;
    use crate::handshake::{Side, connect};
    use crate::managed::{self, Scope};

    fn transport(serves: bool) -> TransportConfig {
        let mut transport = TransportConfig::default();
        transport.max_concurrent_uni_streams(hyper_quic::VarInt::from_u32(0));
        if !serves {
            transport.max_concurrent_bidi_streams(hyper_quic::VarInt::from_u32(0));
        }
        transport
    }

    /// A controller under `own` dialing an endpoint of `vm` that enrolled `enrolled`, the
    /// controller having pinned `pinned`.
    fn dial(vm: &Identity, enrolled: Fingerprint, own: &Identity, pinned: Fingerprint) -> (Side, Side) {
        connect(
            endpoint(vm, enrolled, transport(true)).unwrap(),
            controller(own, pinned, transport(false)).unwrap(),
        )
    }

    /// Why each side's connection ended, as [`ended`] reads it.
    fn why(side: &Side) -> Ended {
        ended(side.lost.as_ref().unwrap_or_else(|| panic!("still connected")))
    }

    /// The controller the microVM enrolled reaches it, and the microVM its controller pinned:
    /// TLS13_AES_256_GCM_SHA384 at both ends, on the control protocol, each side holding the
    /// other's own key.
    #[test]
    fn an_enrolled_controller_and_its_pinned_vm_connect_on_aes_256_gcm() {
        let (vm, kiln) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let (endpoint, controller) = dial(&vm, kiln.fingerprint(), &kiln, vm.fingerprint());
        assert!(
            endpoint.connected && controller.connected,
            "{:?} {:?}",
            endpoint.lost,
            controller.lost
        );
        for side in [&endpoint, &controller] {
            assert_eq!(side.suite(), Some(CipherSuite::TLS13_AES_256_GCM_SHA384));
            assert_eq!(side.alpn().as_deref(), Some(ALPN));
        }
        assert_eq!(endpoint.peer_key().as_deref(), Some(kiln.spki()));
        assert_eq!(controller.peer_key().as_deref(), Some(vm.spki()));
    }

    /// A controller of another key than the one enrolled is refused at its handshake, typed at
    /// both ends; the endpoint never counts it connected, so no lane of it is ever read.
    #[test]
    fn a_controller_not_enrolled_is_refused_at_its_handshake() {
        let (vm, kiln, stranger) = (
            Identity::generate().unwrap(),
            Identity::generate().unwrap(),
            Identity::generate().unwrap(),
        );
        let (endpoint, controller) = dial(&vm, kiln.fingerprint(), &stranger, vm.fingerprint());
        assert!(!endpoint.connected, "the endpoint took a key it never enrolled");
        assert_eq!(why(&endpoint), Ended::NotEnrolled(By::This));
        assert_eq!(why(&controller), Ended::NotEnrolled(By::Peer));
    }

    /// An endpoint presenting another key than the microVM's pinned one, a stand-in's or
    /// another microVM's, is refused by the controller at its handshake, typed at both ends.
    #[test]
    fn an_endpoint_not_the_pinned_vm_is_refused_at_its_handshake() {
        let (vm, other, kiln) = (
            Identity::generate().unwrap(),
            Identity::generate().unwrap(),
            Identity::generate().unwrap(),
        );
        let (endpoint, controller) = dial(&other, kiln.fingerprint(), &kiln, vm.fingerprint());
        assert!(!controller.connected && !endpoint.connected);
        assert_eq!(why(&controller), Ended::NotEnrolled(By::This));
        assert_eq!(why(&endpoint), Ended::NotEnrolled(By::Peer));
    }

    /// A peer of another protocol, a context's client of the same keys, is refused at its
    /// handshake for its protocol, typed: no context command reaches a control endpoint.
    #[test]
    fn a_peer_of_another_protocol_is_refused() {
        let (vm, kiln) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let (endpoint, client) = connect(
            endpoint(&vm, kiln.fingerprint(), transport(true)).unwrap(),
            tls::client(&kiln, vm.fingerprint()).unwrap(),
        );
        assert!(!endpoint.connected && !client.connected);
        assert_eq!(why(&endpoint), Ended::Protocol(By::This));
        assert_eq!(why(&client), Ended::Protocol(By::Peer));
    }

    /// Codes a connection closes with read back as what they are.
    #[test]
    fn a_close_reads_as_its_cause() {
        use hyper_quic::{TransportError, VarInt};
        let local = |alert: AlertDescription| {
            ConnectionError::TransportError(TransportError {
                code: TransportErrorCode::crypto(alert.into()),
                frame: None,
                reason: String::new(),
            })
        };
        assert_eq!(
            ended(&local(AlertDescription::HandshakeFailure)),
            Ended::Handshake(By::This)
        );
        assert_eq!(
            ended(&local(AlertDescription::BadCertificate)),
            Ended::Transport(By::This)
        );
        let closed = |code: u32| {
            ConnectionError::ApplicationClosed(hyper_quic::ApplicationClose {
                error_code: VarInt::from_u32(code),
                reason: bytes::Bytes::new(),
            })
        };
        assert_eq!(
            ended(&closed(Reason::Stale as u32)),
            Ended::Closed(By::Peer, Reason::Stale)
        );
        assert_eq!(ended(&closed(99)), Ended::Transport(By::Peer));
        assert_eq!(ended(&ConnectionError::TimedOut), Ended::Silent);
        assert_eq!(ended(&ConnectionError::LocallyClosed), Ended::Local);
        assert_eq!(ended(&ConnectionError::Reset), Ended::Transport(By::Peer));
        assert_eq!(ended(&ConnectionError::CidsExhausted), Ended::Transport(By::This));
    }

    fn scope() -> (RunId, u64, u32) {
        (RunId([7; 32]), 3, 2)
    }

    /// Step 5's own subscriptions open a member's lanes, byte for byte: its stdin's and each
    /// output's, at their offsets; nothing is taken until the frame is whole.
    #[test]
    fn step_5s_subscriptions_open_a_members_lanes() {
        let (run, generation, member) = scope();
        let output = |channel| {
            managed::subscribe(
                Scope {
                    run,
                    generation,
                    member,
                    channel,
                },
                41,
            )
        };
        let input = managed::input::subscribe(
            managed::input::Scope {
                run,
                generation,
                member,
            },
            9,
        );
        for (bytes, stream, offset) in [
            (input, Stdio::Stdin, 9),
            (
                output(shards_abi::run::managed::Channel::Stdout),
                Stdio::Stdout,
                41,
            ),
            (
                output(shards_abi::run::managed::Channel::Stderr),
                Stdio::Stderr,
                41,
            ),
        ] {
            let opened = Lane::Stdio {
                run,
                generation,
                member,
                stream,
                offset,
            };
            assert_eq!(lane(&bytes), Ok(Some((opened, LANE_MOST))));
            let mut more = bytes.to_vec();
            more.extend_from_slice(b"what follows is the lane's");
            assert_eq!(lane(&more), Ok(Some((opened, LANE_MOST))));
            for cut in 0..bytes.len() {
                assert_eq!(lane(&bytes[..cut]), Ok(None), "cut at {cut}");
            }
        }
        assert_eq!(LANE_MOST, managed::CONTROL_BYTES);
        assert_eq!(LANE_MOST, managed::input::SUBSCRIBE_BYTES);
    }

    /// A first frame of another kind or length, or of no member's channel, is refused as
    /// the controller's error before the endpoint reads what follows it.
    #[test]
    fn a_lane_of_no_kind_is_refused() {
        let (run, generation, member) = scope();
        let good = managed::input::subscribe(
            managed::input::Scope {
                run,
                generation,
                member,
            },
            0,
        );
        let mut other_tag = good;
        other_tag[0] = Tag::Stdin as u8;
        assert_eq!(lane(&other_tag), Err(Reason::Protocol));
        let mut no_channel = good;
        no_channel[FRAME_HEAD + 32 + 8 + 4] = kind::SYSTEM_ERR;
        assert_eq!(lane(&no_channel), Err(Reason::Protocol));
        let mut longer = Vec::new();
        frame::put(&mut longer, Tag::Exec, &[0; SUBSCRIPTION + 1]).unwrap();
        assert_eq!(lane(&longer[..FRAME_HEAD]), Err(Reason::Protocol));
        assert_eq!(lane(&[0xee, 0, 0, 0, 0]), Err(Reason::Protocol));
    }

    /// A lane's refusal reads back as its reason, whole or not yet; anything else there is
    /// the lane's own frame.
    #[test]
    fn a_lanes_refusal_reads_back_as_its_reason() {
        for reason in [
            Reason::Protocol,
            Reason::Scope,
            Reason::Stale,
            Reason::Closed,
            Reason::Auth,
        ] {
            let frame = refused(reason);
            assert_eq!(read_refused(&frame), Ok(Some(reason)));
            for cut in 0..frame.len() {
                assert_eq!(read_refused(&frame[..cut]), Ok(None));
            }
        }
        let mut data = Vec::new();
        frame::put(&mut data, Tag::Stdout, b"x").unwrap();
        assert_eq!(read_refused(&data), Err(Some(Tag::Stdout)));
        let mut unknown = refused(Reason::Scope);
        unknown[REFUSED_BYTES - 1] = 0;
        assert_eq!(read_refused(&unknown), Err(None));
    }
}
