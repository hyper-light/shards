//! The toolchain's own correctness on a shape shards' code uses: nested `impl Iterator` chains
//! (`flatten`, `flat_map`, `chain`) boxed as a `dyn Iterator`. Rust 1.98.0 compiles this program's
//! vtable with a null slot, so the boxed iterator's `next` jumps to address 0 and the test binary
//! dies of SIGSEGV, at opt-level 0 and 3 alike; 1.98.1 runs it (aarch64-apple-darwin, 2026-10-16).
//! The program is focal's reduction of the failure it met moving to 1.98.0, as ../hyper-raft
//! carries it (crates/hyper-raft/tests/toolchain.rs): run on every target CI builds, it keeps a
//! stale toolchain or image from building shards again (rust-toolchain.toml).
// The program keeps focal's shapes exactly, the lints' simpler forms included: the fault is in how
// these chains compile, and a simpler chain need not carry it.
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros,
    clippy::unnecessary_filter_map,
    clippy::map_identity
)]

use std::collections::BTreeMap;

struct Store {
    rows: BTreeMap<u64, u64>,
}
impl Store {
    fn entries_from(&self, k: u64) -> impl Iterator<Item = (&u64, &u64)> + '_ {
        self.rows.range(k..)
    }
    fn entries(&self) -> impl Iterator<Item = (&u64, &u64)> + '_ {
        self.rows.iter()
    }
}
struct Part {
    value: Store,
}
struct Parts {
    parts: Vec<Part>,
}
impl Parts {
    fn entries_from<'a>(&'a self, key: u64) -> impl Iterator<Item = (&'a u64, &'a u64)> + use<'a> {
        let index = 0usize;
        let first = self.parts.get(index).map(|p| p.value.entries_from(key));
        let rest = self
            .parts
            .iter()
            .skip(index.saturating_add(1))
            .flat_map(|p| p.value.entries());
        first.into_iter().flatten().chain(rest)
    }
    fn entries(&self) -> impl Iterator<Item = (&u64, &u64)> + '_ {
        self.parts.iter().flat_map(|p| p.value.entries())
    }
}
struct Ranges {
    stores: Vec<Parts>,
}
impl Ranges {
    fn entries_from<'a>(&'a self, key: u64) -> impl Iterator<Item = (&'a u64, &'a u64)> + use<'a> {
        let index = 0usize;
        let first = self.stores.get(index).map(|s| s.entries_from(key));
        let rest = self
            .stores
            .get(index.saturating_add(1)..)
            .into_iter()
            .flatten()
            .flat_map(Parts::entries);
        first.into_iter().flatten().chain(rest)
    }
}
struct Core {
    rows: Ranges,
}
impl Core {
    fn objects_from(&self, after: Option<u64>) -> impl Iterator<Item = u64> + '_ {
        self.rows
            .entries_from(after.unwrap_or(0))
            .take_while(|(k, _)| **k < 1_000_000)
            .filter(|(_, v)| **v % 2 == 0)
            .filter_map(|(k, _)| Some(*k))
    }
    fn claims_from(&self, after: Option<u64>) -> impl Iterator<Item = u64> + '_ {
        self.objects_from(after).map(|x| x)
    }
    fn claim_evals(&self, claim: u64, after: Option<u64>) -> impl Iterator<Item = u64> + '_ {
        let start = after.unwrap_or(claim);
        self.rows
            .entries_from(start)
            .take_while(move |(k, _)| **k / 10 == claim / 10)
            .filter_map(|(k, _)| Some(*k))
    }
    fn evals(&self, claim: Option<u64>, after: Option<u64>) -> Box<dyn Iterator<Item = u64> + '_> {
        if let Some(c) = claim {
            return Box::new(self.claim_evals(c, after));
        }
        let current = after;
        let head = current.map(|c| self.claim_evals(c, after)).into_iter().flatten();
        let rest = self
            .claims_from(current)
            .flat_map(move |c| self.claim_evals(c, None));
        Box::new(head.chain(rest))
    }
}
#[test]
fn nested_iterator_chains_boxed_as_dyn_iterator_run() {
    let rows: BTreeMap<u64, u64> = (0..200u64).map(|i| (i, i)).collect();
    let core = Core {
        rows: Ranges {
            stores: vec![Parts {
                parts: vec![
                    Part {
                        value: Store { rows: rows.clone() },
                    },
                    Part {
                        value: Store { rows },
                    },
                ],
            }],
        },
    };
    let all: Vec<u64> = core.evals(None, None).collect();
    assert_eq!(all.len(), 1200);
}
