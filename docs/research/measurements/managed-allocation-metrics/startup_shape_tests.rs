//! Private light harness includes these tests beside byte-extracted snapshot
//! structs/serializer from fixture.rs, with real serde_json/managed_alloc types.
//! No artifact closure, resolver, native probe or full caller is compiled here.
//! MANAGED_ALLOC is an unregistered private instance, not the process allocator;
//! these tests establish record types/shapes, never allocation measurements.

use super::*;

#[cfg(test)]
mod startup_shapes {
    use super::*;

    const ARTIFACTS: [&str; 5] = ["fixture", "cli", "testguest", "kernel", "init"];
    const BOUNDARIES: [&str; 3] = ["before_resolution", "after_resolution", "artifact_value_ready"];

    #[test]
    fn startup_initial_fields_distinguish_observed_from_not_reached() {
        let record = StartupAllocations::new().record();
        assert_eq!(record["test_entered"]["state"], "Observed");
        assert_eq!(record["test_entered"]["semantics"], "ConcurrentNonAtomic");
        assert_eq!(record["supplied_inputs_ready"]["state"], "NotReached");
        assert_eq!(record["inputs_recorded"]["state"], "NotReached");
        for artifact in ARTIFACTS {
            for boundary in BOUNDARIES {
                assert_eq!(record[artifact][boundary], json!({"state":"NotReached"}));
            }
        }
        // This constructor is rendered explicitly by the unit. It says nothing
        // about an early fixture panic, which still emits no startup record.
        assert!(record["coverage_start"].as_str().unwrap().contains("earlier"));
    }

    #[test]
    fn startup_reached_fields_keep_non_atomic_and_artifact_boundaries() {
        let snapshot = MANAGED_ALLOC.snapshot();
        let mut startup = StartupAllocations::new();
        startup.supplied_inputs_ready = Some(snapshot);
        startup.inputs_recorded = Some(snapshot);
        for artifact in [
            &mut startup.fixture,
            &mut startup.cli,
            &mut startup.testguest,
            &mut startup.kernel,
            &mut startup.init,
        ] {
            artifact.before_resolution = Some(snapshot);
            artifact.after_resolution = Some(snapshot);
            artifact.artifact_value_ready = Some(snapshot);
        }
        let record = startup.record();
        for artifact in ARTIFACTS {
            for boundary in BOUNDARIES {
                assert_eq!(record[artifact][boundary]["state"], "Observed");
                assert_eq!(record[artifact][boundary]["semantics"], "ConcurrentNonAtomic");
            }
        }
        assert_eq!(record["supplied_inputs_ready"]["state"], "Observed");
        assert_eq!(record["inputs_recorded"]["state"], "Observed");
        assert!(
            record["artifact_boundary"]
                .as_str()
                .unwrap()
                .contains("precedes outer inputs-JSON")
        );
        assert!(record["semantics"].as_str().unwrap().contains("no quiescence"));
    }

    #[test]
    fn startup_unknown_counts_remain_unknown_in_reached_fields() {
        let mut snapshot = MANAGED_ALLOC.snapshot();
        // Mutate a plain Snapshot value only; no counter reset, fake pointer,
        // invalid native operation, registration or allocation refusal.
        snapshot.allocation_requested_bytes = managed_alloc::Count::Unknown;
        snapshot.live_requested_bytes = managed_alloc::Count::Unknown;
        snapshot.peak_recorded_live_requested_bytes = managed_alloc::Count::Unknown;
        let mut startup = StartupAllocations::new();
        startup.fixture.before_resolution = Some(snapshot);
        let record = startup.record();
        let reached = &record["fixture"]["before_resolution"];
        assert_eq!(reached["state"], "Observed");
        for field in [
            "allocation_requested_bytes",
            "live_requested_bytes",
            "peak_recorded_live_requested_bytes",
        ] {
            assert_eq!(reached[field]["state"], "Unknown");
            assert!(reached[field].get("value").is_none());
            assert!(reached[field]["reason"].as_str().unwrap().contains("sticky"));
        }
        assert_eq!(record["fixture"]["after_resolution"]["state"], "NotReached");
    }
}
