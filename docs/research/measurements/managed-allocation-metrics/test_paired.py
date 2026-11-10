"""Synthetic data/ordinary private-file tests only; never launches a fixture."""

import copy
import hashlib
import json
import os
from pathlib import Path
import tempfile
import unittest

import paired as p


def manifest(variant="before", revision="a" * 40):
    artifacts = {name: {"path": f"/synthetic/{variant}/{name}", "sha256": "1" * 64} for name in (*p.ARTIFACTS, "build_record", "workload_record")}
    artifacts["testguest"]["path"] = f"/synthetic/{variant}/guests/guest/shards-testguest"
    return {"schema": "managed-output-cost-pair-manifest-v1", "build_profile": "release", "variant": variant, "source": {"directory": f"/synthetic/{variant}/source", "revision": revision, "tree": "b" * 40}, "artifacts": artifacts, "test_guests": f"/synthetic/{variant}/guests", "environment": {"PATH": "/usr/bin:/bin", "HOME": "/synthetic/home"}}


def synthetic_case(m, members=1):
    ids = {"observer_parent": [100, 101], "daemon": [200, 201], "vm": [300, 301]}
    ready = {"state": "NoManagedMembers" if members == 0 else "OwnerChannelsReady", "endpoints": 2 * members, "run": "cafe"}
    inputs = {name: {"path": item["path"], "sha256": "sha256:" + item["sha256"]} for name, item in m["artifacts"].items() if name in p.ARTIFACTS}
    inputs.update({"source": m["source"]["revision"], "members": members, "phase_deadline_ms": 123})
    snapshots = {"checkpoint": ready}
    held, exits = {}, {"both_valid_exit_events": True}
    for role, value in ids.items():
        snapshots[role] = {"pid": value[0], "expected_start": value[1], "before_identity": value[:], "after_identity": value[:], "identity_state": "MatchedBeforeAndAfter"}
        if role != "observer_parent":
            named = {"pid": value[0], "expected_started": value[1]}
            held[role] = {"state": "Watching", "identity": named, "readiness_identity": {"state": "Matched"}}
            exits[role] = {"state": "ExitReadyObservedAfterOwnerStopWithinCleanupDeadline", "identity": named}
    fds = {"baseline": {"state": "Observed", "count": 7}, "with_exit_objects": {"state": "Observed", "count": 12}, "after_release": {"state": "Observed", "count": 7}, "owned_observation_fds": 5, "exact_delta_and_return": True}
    terminal = {"state": "NoManagedMembers", "managed_channels": 0}
    if members:
        terminal = {"state": "OwnerChannelsAborted", "channels": [{"member": member, "channel": channel, "accepted": 8, "consumed": 0 if (member, channel) == (1, 1) else 8, "tail": "unknown"} for member in range(1, members + 1) for channel in (1, 2)]}
    def allocation_sample():
        value = {name: {"state": "Known", "value": 10} for name in (*p.CUMULATIVE, *p.GAUGES)}
        value.update({"state": "Observed", "semantics": "ConcurrentNonAtomic"})
        return value
    return {"inputs": inputs, "ready": ready, "snapshots": snapshots, "exit-watches.ready": held, "control": {"state": "MainProgressAfterObserverCancel", "members": members, "exact_payload_channels": 2 * members}, "teardown": {"cleanup_phase_deadline_ms": 123, "protocol_completed": True, "owner_stop_and_observer_join": {"Ok": None}, "host_process_exit": exits, "exit_observation_fds": fds}, "terminal": terminal, "allocation-phases": {phase: allocation_sample() for phase in p.PHASES}, "allocation-final": {"phase": "after_terminal_records_and_fixture_drop", "snapshot": allocation_sample()}}


class PurePairedTests(unittest.TestCase):
    def refuses_case(self, data, m=None, members=1):
        with self.assertRaises(p.Refusal):
            p.validate_case(m or manifest(), members, 123, data)

    def test_manifest_requires_exact_source_roles_public_env_and_prebuilt_guest(self):
        valid = manifest()
        self.assertEqual(p.validate_manifest(valid), valid)
        for path, bad in [(('build_profile',), 'test-release'), (('source', 'revision'), 'main'), (('environment', 'TOKEN'), 'synthetic-secret'), (('artifacts', 'fixture', 'sha256'), 'missing'), (('artifacts', 'testguest', 'path'), '/different/path')]:
            value = copy.deepcopy(valid)
            owner = value
            for key in path[:-1]:
                owner = owner[key]
            owner[path[-1]] = bad
            with self.assertRaises(p.Refusal):
                p.validate_manifest(value)

    def test_manifest_pairs_require_matching_environment_workload_and_kernel(self):
        a, b = manifest(), manifest('after', 'c' * 40)
        p.validate_pair(a, b)
        for role in ('kernel', 'workload_record'):
            changed = copy.deepcopy(b)
            changed['artifacts'][role]['sha256'] = '2' * 64
            with self.assertRaises(p.Refusal):
                p.validate_pair(a, changed)
        b['environment']['LANG'] = 'C'
        with self.assertRaises(p.Refusal):
            p.validate_pair(a, b)

    def test_schedule_is_serial_alternating_and_odd_imbalance_is_explicit(self):
        self.assertEqual(list(p.schedule(3)), [(0, 0, 'A'), (0, 1, 'B'), (1, 0, 'B'), (1, 1, 'A'), (2, 0, 'A'), (2, 1, 'B')])
        self.assertEqual(p.summarize([], 3)['order_pairs_requested'], {'AB': 2, 'BA': 1})
        for bad in (0, -1, True):
            with self.assertRaises(p.Refusal):
                list(p.schedule(bad))

    def test_zero_and_nonzero_readiness_terminal_contracts_pass_separately(self):
        m = manifest()
        for members in (0, 1, 2):
            result = p.validate_case(m, members, 123, synthetic_case(m, members))
            self.assertEqual(result['vm'], (300, 301))
        bad = synthetic_case(m, 0)
        bad['ready']['state'] = 'OwnerChannelsReady'
        self.refuses_case(bad, m, 0)

    def test_case_rejects_artifact_source_n_and_boolean_numeric_substitution(self):
        m = manifest()
        for field, bad in [('source', 'c' * 40), ('members', 2), ('members', True), ('phase_deadline_ms', 124)]:
            data = synthetic_case(m)
            data['inputs'][field] = bad
            self.refuses_case(data, m)
        data = synthetic_case(m)
        data['inputs']['fixture']['sha256'] = '2' * 64
        self.refuses_case(data, m)

    def test_case_rejects_partial_identity_wrong_exit_identity_and_exact_fd_miss(self):
        m = manifest()
        data = synthetic_case(m)
        data['snapshots']['vm']['after_identity'][1] += 1
        self.refuses_case(data, m)
        data = synthetic_case(m)
        data['exit-watches.ready']['daemon']['readiness_identity']['state'] = 'Unverified'
        self.refuses_case(data, m)
        data = synthetic_case(m)
        # Deep-copy this role because the synthetic construction shares its
        # expected identity with the held record, unlike separate JSON files.
        data['teardown']['host_process_exit']['vm']['identity'] = {'pid': 300, 'expected_started': 999}
        self.refuses_case(data, m)
        data = synthetic_case(m)
        data['teardown']['exit_observation_fds']['after_release']['count'] += 1
        self.refuses_case(data, m)

    def test_case_rejects_failed_cleanup_deadline_and_wrong_member_cursors(self):
        m = manifest()
        for field, bad in [('protocol_completed', False), ('owner_stop_and_observer_join', {'Err': 'synthetic'}), ('cleanup_phase_deadline_ms', 124)]:
            data = synthetic_case(m)
            data['teardown'][field] = bad
            self.refuses_case(data, m)
        data = synthetic_case(m)
        data['terminal']['channels'][0]['consumed'] = 8
        self.refuses_case(data, m)
        data = synthetic_case(m)
        data['terminal']['channels'][1]['member'] = 0
        self.refuses_case(data, m)

    def test_unknown_counts_and_missing_native_samples_remain_visible(self):
        data = synthetic_case(manifest())
        data['allocation-phases']['owner_ready_before_recording']['alloc_successes'] = {'state': 'Unknown', 'reason': 'synthetic overflow'}
        metrics = p.allocation_metrics(data)
        self.assertEqual(metrics['observer_rust.owner_ready_before_recording.alloc_successes.since_before_fixture']['state'], 'Unknown')
        self.assertEqual(metrics['native_faults.vm.start_to_ready']['state'], 'Unknown')
        self.assertEqual(p.counter_difference({'state': 'Known', 'value': 20}, {'state': 'Known', 'value': 10})['state'], 'Unknown')
        with self.assertRaises(p.Refusal):
            p.counter({'state': 'Known', 'value': (1 << 64) - 1})

    def test_native_conditional_records_preserve_role_identity_and_zero_delta(self):
        data = synthetic_case(manifest())
        native = {}
        for value, role in enumerate(p.ROLES):
            sample = data['snapshots'][role]
            native[role] = {'state': 'ConditionalDifference', 'value': value, 'source': 'LinuxMinfltPlusMajfltViaPlatformUsage', 'native_bits': 64, 'identity': [sample['pid'], sample['expected_start']], 'caveat': 'synthetic conditional record'}
        data['native-fault-phases'] = {'start_to_ready': native}
        metrics = p.allocation_metrics(data)
        for value, role in enumerate(p.ROLES):
            actual = metrics[f'native_faults.{role}.start_to_ready']
            self.assertEqual(actual, native[role] | {'state': 'Conditional'})
            self.assertEqual(actual['value'], value)

    def test_native_invalid_source_width_identity_and_delta_are_unknown(self):
        valid = {'state': 'ConditionalDifference', 'value': 0, 'source': 'LinuxMinfltPlusMajfltViaPlatformUsage', 'native_bits': 64, 'identity': [300, 301]}
        for field, bad in [('state', 'Unknown'), ('source', 'MacProcTaskInfoFaultsViaPlatformUsage'), ('native_bits', 32), ('identity', [300, 999]), ('value', True), ('value', -1), ('value', 1 << 64)]:
            with self.subTest(field=field, bad=bad):
                data = synthetic_case(manifest())
                data['native-fault-phases'] = {'start_to_ready': {'vm': valid | {field: bad}}}
                actual = p.allocation_metrics(data)['native_faults.vm.start_to_ready']
                self.assertEqual(actual['state'], 'Unknown')

    def test_native_width_and_identity_require_exact_json_integer_types(self):
        m = manifest()
        for bits, named, expected_start in [(64.0, [300, 301], 301), (64, [300.0, 301], 301), (64, [300, 301.0], 301), (64, [300, True], 1)]:
            with self.subTest(bits=bits, named=named):
                data = synthetic_case(m)
                sample = data['snapshots']['vm']
                sample['expected_start'] = expected_start
                sample['before_identity'][1] = expected_start
                sample['after_identity'][1] = expected_start
                data['exit-watches.ready']['vm']['identity']['expected_started'] = expected_start
                data['teardown']['host_process_exit']['vm']['identity']['expected_started'] = expected_start
                data['native-fault-phases'] = {'start_to_ready': {'vm': {'state': 'ConditionalDifference', 'value': 0, 'source': 'LinuxMinfltPlusMajfltViaPlatformUsage', 'native_bits': bits, 'identity': named}}}
                data = json.loads(json.dumps(data))
                p.validate_case(m, 1, 123, data)
                self.assertEqual(p.allocation_metrics(data)['native_faults.vm.start_to_ready']['state'], 'Unknown')

    def test_empirical_quantiles_and_signed_paired_differences(self):
        samples = [{'state': 'Known', 'value': n} for n in range(1, 101)]
        values = p.distribution(samples, 100)
        self.assertEqual([values[key] for key in ('n', 'p50', 'p90', 'p99', 'max')], [100, 50, 90, 99, 100])
        single = p.distribution([{'state': 'Known', 'value': -7}], 1)
        self.assertEqual([single[key] for key in ('p50', 'p90', 'p99', 'max')], [-7] * 4)
        rows = [{'pair': 0, 'variant': 'A', 'metrics': {'x': {'state': 'Known', 'value': 10}}}, {'pair': 0, 'variant': 'B', 'metrics': {'x': {'state': 'Conditional', 'value': 8}}}]
        result = p.summarize(rows, 1)['paired_B_minus_A']['x']
        self.assertEqual((result['p50'], result['coverage']), (-2, 'Conditional'))
        with self.assertRaises(p.Refusal):
            p.summarize(rows + [rows[0]], 1)

    def test_incomplete_and_unknown_coverage_is_not_silently_dropped(self):
        result = p.distribution([{'state': 'Known', 'value': 3}, p.unknown('synthetic')], 3)
        self.assertEqual((result['n'], result['n_unknown'], result['n_missing'], result['coverage']), (1, 1, 1, 'Incomplete'))

    def test_private_file_hashing_and_json_validation_without_execution(self):
        with tempfile.TemporaryDirectory(dir=os.environ['SHARDS_METRICS_LIGHT_TMP']) as directory:
            path = Path(directory) / 'ordinary-file'
            path.write_bytes(b'synthetic')
            self.assertEqual(p.file_digest(path), hashlib.sha256(b'synthetic').hexdigest())
            path.write_text('{"x":1,"x":2}')
            with self.assertRaises(p.Refusal):
                p.load_json(path)
            path.write_text('{"x":NaN}')
            with self.assertRaises(p.Refusal):
                p.load_json(path)


if __name__ == '__main__':
    unittest.main()
