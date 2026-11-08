//! The commit accounting measurement (docs/research/measurements/commit/commit.rs), an
//! ignored test for the helpers E2E tests use.

#![cfg(unix)]
#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

mod common;

#[path = "../../../docs/research/measurements/commit/commit.rs"]
mod commit;
