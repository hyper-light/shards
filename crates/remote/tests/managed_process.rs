#![cfg(target_os = "linux")]
#[path = "../../../docs/research/measurements/managed-performance/process-compat/role.rs"]
mod fixture;

#[test]
#[allow(clippy::expect_used)]
#[ignore = "explicit admitted separate-process compatibility fixture"]
fn managed_process_role() {
    fixture::run().expect("managed process role");
}
