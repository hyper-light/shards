//! Harness-only access to the exact managed Service, parser and state machine.
//! Invoked alone in release, with libtest overhead reported separately.

#[path = "src/main.rs"]
mod measurement;

#[test]
#[ignore = "explicit admitted native performance campaign only"]
fn native_authenticated_comparison() {
    let arguments = std::env::var("MANAGED_ARGS").expect("operator supplies MANAGED_ARGS");
    let mut args = vec!["managed-performance".to_owned()];
    args.extend(arguments.split_whitespace().map(str::to_owned));
    measurement::run_with(args).expect("native authenticated measurement");
}
