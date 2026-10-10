//! A rule's else branches are a chain as long as the policy makes it (OPA's parser takes
//! any number): everything done to a whole rule walks the chain without a frame a branch.

use shards_rego::ast::Rule;
use shards_rego::compare::rule_compare;
use shards_rego::parser::parse_module;

/// A rule of 100000 branches parses on a thread of 1 MiB: the parser takes one branch
/// after another, as OPA's takes them one inside another.
#[test]
fn a_rule_of_100000_branches_parses_on_a_small_stack() {
    let mut src = String::from("package p\n\np := 0 if input.never ");
    for i in 1..100_000 {
        src.push_str(&format!("else := {i} if input.never "));
    }
    src.push_str("else := 100000\n");
    std::thread::Builder::new()
        .stack_size(1 << 20)
        .spawn(move || {
            let m = parse_module("p.rego", &src).unwrap();
            assert_eq!(m.rules[0].branches().count(), 100_001);
        })
        .unwrap()
        .join()
        .unwrap();
}

/// A rule of a million branches is cloned, compared, printed and dropped on a thread of
/// 256 KiB, a stack too small for a frame a branch.
#[test]
fn a_rule_of_a_million_branches_is_handled_on_a_small_stack() {
    std::thread::Builder::new()
        .stack_size(256 << 10)
        .spawn(|| {
            let m = parse_module("p.rego", "package p\n\np := 1 if input.a else := 2 if input.b\n").unwrap();
            let branch = m.rules[0].else_.as_deref().unwrap().branch();
            let mut chain = m.rules[0].branch();
            let mut tail: Option<Box<Rule>> = None;
            for _ in 0..1_000_000 {
                let mut b = branch.branch();
                b.else_ = tail;
                tail = Some(Box::new(b));
            }
            chain.else_ = tail;
            let copy = chain.clone();
            assert_eq!(copy.branches().count(), 1_000_001);
            assert_eq!(rule_compare(&chain, &copy), std::cmp::Ordering::Equal);
            assert!(chain.to_string().len() > 1_000_000);
            assert!(format!("{chain:?}").len() > 1_000_000);
            drop(copy);
            drop(chain);
        })
        .unwrap()
        .join()
        .unwrap();
}
