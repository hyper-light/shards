//! The seams kiln reaches every product through (the critical path's master plan §3, CP-M2), each a
//! trait with a simulator implementation and a conformance suite:
//!
//! | Seam | What it is | Its real implementation |
//! |---|---|---|
//! | [`Clock`](clock::Clock) | time: now, the wall clock, sleeps | kiln's runtime (`kiln::rt::RtClock`) |
//! | [`Spawn`](spawn::Spawn) | tasks, run on their own | kiln's runtime (`kiln::rt::RtSpawn`) |
//! | [`Records`](records::Records) | records, by family and key, with transactions and a change feed | Amphora (CP-T1.5) |
//! | [`Bus`](bus::Bus) | topics of messages, in order, from any offset | Quiver (CP-T2.1) |
//! | [`Blobs`](blobs::Blobs) | bytes, with S3's semantics | kiln's CAS, then Mantle (CP-T10.1) |
//! | [`Secrets`](secrets::Secrets) | secrets, versions, stages and leases | Veil (CP-T8.1) |
//! | [`Artifacts`](artifacts::Artifacts) | OCI repositories: blobs, manifests, tags, referrers | Reliquary (CP-T9.1) |
//! | [`Sandbox`](sandbox::Sandbox) | a harness launched in a boundary | `host`, then Shards (CP-T4.1), then Hex |
//! | [`Placement`](placement::Placement) | where work runs, by resources | Hex (CP-T7.1) |
//! | [`HarnessPort`](harness::HarnessPort) | kiln's agents: who they are, and commands to them | kiln's harness registry |
//! | [`Forge`](forge::Forge) | what a git host does with changes: stacks, its queue, updates, batched reviews, merges | githost: kiln's GitHub App, GitLab, Bitbucket (W26-R20) |
//! | [`Search`](search::Search) | a tree's files, searched by text or by code's shape | vorpal (`kiln-search`, W26-R07) |
//! | [`Sigstore`](sigstore::Sigstore) | an outside Fulcio and Rekor: keys certified, entries logged | kiln's client of the Fulcio and Rekor it's configured with |
//! | [`DependencySources`](dependencies::DependencySources) | dependency identity, revisions, history, files, images and runtimes | kiln's source adapter (W29-R23) |
//!
//! Each module holds the trait, its simulator (`Sim*`: deterministic, in memory, bounded) and its
//! conformance suite (`conformance::cases`, run with [`conformance::run`] against a fixture that
//! makes a fresh implementation per case). A track's real implementation passes the same suite.
//! docs/seams.md states each contract, clause by clause, under the ids the cases carry; a
//! doc-truth test (`tests/doc_truth.rs`) holds the two together.
//!
//! The bar is kiln's: no panics (the workspace lints), no unchecked arithmetic outside tests,
//! every queue and table bounded with its overflow stated, and typed refusals, never silent drops.

#![cfg_attr(not(test), deny(clippy::arithmetic_side_effects))]

pub mod artifacts;
pub mod blobs;
pub mod bus;
pub mod clock;
pub mod conformance;
pub mod dependencies;
pub mod forge;
pub mod harness;
pub mod placement;
pub mod records;
pub mod sandbox;
pub mod search;
pub mod secrets;
pub mod sigstore;
pub mod spawn;

pub use artifacts::Artifacts;
pub use blobs::Blobs;
pub use bus::Bus;
pub use clock::Clock;
pub use dependencies::DependencySources;
pub use forge::Forge;
pub use harness::HarnessPort;
pub use placement::Placement;
pub use records::Records;
pub use sandbox::Sandbox;
pub use search::Search;
pub use secrets::Secrets;
pub use sigstore::Sigstore;
pub use spawn::Spawn;

/// The hex SHA-256 of `bytes`: the digest Blobs, Artifacts and the rest name content by.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    hex::encode(sha2::Sha256::digest(bytes))
}
