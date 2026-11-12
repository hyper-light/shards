// The never-dialed experiment (logs/never-dialed.log): appended to the tests module of
// crates/shards/src/workload.rs at the fix's revision, and run there alone:
//   cargo test -p shards-vm-process --bin shards-vm --profile test-release -- \
//     workload::tests::never_dialed_experiment --nocapture
// Not on main.

    /// Experiment, not for main: how long an exec waits where its guest never dials the
    /// signal port.
    #[test]
    fn never_dialed_experiment() {
        static TO: ToGuest = ToGuest::new(Signals::new());
        let (_serial, _port, _acceptor) = exec_port_test();
        will_run(&TO);
        let t = std::time::Instant::now();
        let heard = exec_ends(&TO);
        let waited = heard.recv_timeout(std::time::Duration::from_secs(20));
        eprintln!("NEVER-DIALED after {:?}: {waited:?}, its link {:?}", t.elapsed(), lock(&TO).link);
        hellos::close();
        let ended = heard.recv_timeout(std::time::Duration::from_secs(5));
        eprintln!("NEVER-DIALED at the port's close, after {:?}: {ended:?}", t.elapsed());
    }
