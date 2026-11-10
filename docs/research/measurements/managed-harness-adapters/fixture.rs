//! Native light codec tests with the product JSON types and production modules.
#![allow(dead_code)]
#[path = "../../../../crates/init/src/json.rs"]
pub(crate) mod json;
mod server {
    pub(crate) use crate::json;
}
#[path = "../../../../crates/server/src/harness/mod.rs"]
mod harness;
