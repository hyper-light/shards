// Finding 2: managed_limits.rs:310-313 returned from the ignored campaign when
// common::cannot_run_vms() (which prints SKIP and answers true without a hypervisor unless
// SHARDS_REQUIRE_VMS=1). libtest reports a returning test as passed. Built with
// `rustc --test`; the host check is simulated as a host without a hypervisor.
fn cannot_run_vms() -> bool {
    eprintln!("SKIP: simulated host without a hypervisor");
    true
}

#[test]
#[ignore = "campaign"]
fn old_campaign_returns_early() {
    if cannot_run_vms() {
        return;
    }
    panic!("the campaign body would run here");
}

#[test]
#[ignore = "campaign"]
fn fixed_campaign_asserts_its_hypervisor() {
    assert!(!cannot_run_vms(), "real hypervisor required");
    panic!("the campaign body would run here");
}
