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

pub use shards_remote::control::{
    Controller, Dialed, Ended, Fault, Hangup, Link, Linked, Socket, SocketAdmission, SocketReader,
    SocketWriter, Sockets, Subscriptions,
};
pub use shards_remote::identity::{Fingerprint, Identity};
pub use shards_remote::managed::Reason;
