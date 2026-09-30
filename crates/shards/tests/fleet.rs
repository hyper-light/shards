//! The fleet measurement (docs/research/measurements/fleet/fleet.rs), an ignored test for
//! the helpers E2E tests use.

#![cfg(unix)]
#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

mod common;

#[path = "../../../docs/research/measurements/fleet/fleet.rs"]
mod fleet;
