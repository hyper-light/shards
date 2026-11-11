"""ROOT's one-guard real QUIC mutation at frozen capture source."""
from pathlib import Path
import hashlib
import json
import os
import shutil
import signal
import subprocess
import time

repo = Path.cwd()
out = Path.home() / 'root-managed-capture-guard-09a-v1'
out.mkdir(exist_ok=False)
source = '09a187d2a55522d59a79b0fc21ac9378f25c0fb9'
assert subprocess.check_output(['git', '-C', str(repo), 'rev-parse', 'HEAD'], text=True).strip() == source
assert not subprocess.check_output(['git', '-C', str(repo), 'status', '--porcelain', '--untracked-files=no'])
assert shutil.disk_usage(out).free >= 30 * 1024**3
original = repo / 'crates/remote/src/managed.rs'
before = original.read_bytes()
guard = b'''        if bytes.capacity() < self.shared.record_bytes.load(Ordering::Acquire) {
            return Err(Error::Admission(AdmissionError::Capacity));
        }
'''
assert before.count(guard) == 1
mutant = before.replace(guard, b'', 1)
fixture = repo / 'crates/remote/tests/producer_boundary_root.rs'
assert not fixture.exists()
template = repo / 'docs/research/measurements/managed-resource-enforcement/producer-boundary.rs'
rendered = template.read_text().replace('@OBSERVER_PATH@', json.dumps(str(repo / 'crates/shards/tests/common/managed_capture.rs')))
fixture.write_text(rendered)
(out / 'fixture.rs').write_bytes(fixture.read_bytes())
(out / 'guarded.rs').write_bytes(before)
(out / 'mutant.rs').write_bytes(mutant)
(out / 'executed-driver.py').write_bytes(Path(__file__).read_bytes())
(out / 'empty-docker').mkdir()
environment = os.environ.copy()
environment.update(CARGO_BUILD_JOBS='2', CARGO_TARGET_DIR=str(out / 'target'),
                   DOCKER_CONFIG=str(out / 'empty-docker'), SHARDS_CAPTURE_API_CASE_MS='10000')
identity = {'role': 'coordinator', 'source': source,
            'rules_sha256': hashlib.sha256((repo / 'docs/development-rules.md').read_bytes()).hexdigest(),
            'guarded_sha256': hashlib.sha256(before).hexdigest(), 'mutant_sha256': hashlib.sha256(mutant).hexdigest(),
            'fixture_sha256': hashlib.sha256(fixture.read_bytes()).hexdigest(),
            'observer_sha256': hashlib.sha256((repo / 'crates/shards/tests/common/managed_capture.rs').read_bytes()).hexdigest(),
            'lock_sha256': hashlib.sha256((repo / 'Cargo.lock').read_bytes()).hexdigest(),
            'uname': list(os.uname()), 'admission': '2CPU/4GiB/12GiB, j2, outer900s',
            'case_ms': 10000, 'parent_seconds': 15, 'whole_population_kernel_return': 'Unknown'}
(out / 'identity.json').write_text(json.dumps(identity, indent=2) + '\n')


def run(name, command, timeout):
    assert shutil.disk_usage(out).free >= 30 * 1024**3
    (out / (name + '.command.json')).write_text(json.dumps(command) + '\n')
    began = time.monotonic()
    timed = False
    with (out / (name + '.stdout')).open('xb') as stdout, (out / (name + '.stderr')).open('xb') as stderr:
        child = subprocess.Popen(command, cwd=repo, env=environment, stdout=stdout, stderr=stderr, start_new_session=True)
        try:
            status = child.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            child.wait(timeout=15)
            status, timed = 124, True
    result = {'exit_code': status, 'timeout': timed, 'elapsed_seconds': time.monotonic() - began}
    (out / (name + '.result.json')).write_text(json.dumps(result, indent=2) + '\n')
    print(name, json.dumps(result), flush=True)
    return status


def case(phase):
    build = ['lockf', '/tmp/g121-mac.lock', 'cargo', 'test', '--locked', '-p', 'shards-remote',
             '--test', 'producer_boundary_root', '--no-run', '--message-format=json', '-j2']
    assert run(phase + '-build', build, 180) == 0
    artifacts = [json.loads(line) for line in (out / (phase + '-build.stdout')).read_text().splitlines()]
    binaries = [a['executable'] for a in artifacts if a.get('executable')]
    assert len(binaries) == 1
    binary = Path(binaries[0])
    (out / (phase + '-binary.json')).write_text(json.dumps({'path': str(binary), 'sha256': hashlib.sha256(binary.read_bytes()).hexdigest()}) + '\n')
    status = run(phase + '-case', [str(binary), '--exact', 'a_small_owned_submission_cannot_lend_less_storage_than_its_next_credit', '--nocapture', '--test-threads=1'], 15)
    stdout = (out / (phase + '-case.stdout')).read_text()
    stderr = (out / (phase + '-case.stderr')).read_text()
    assert 'CAPTURE_API_BOUNDARY event=cleanup joined=true' in stdout
    if phase == 'mutant':
        assert status == 101 and 'event=loan advertised=58 actual=1 acknowledged=1' in stdout
        assert 'public producer loan is smaller than its advertised capacity' in stderr
    else:
        assert status == 0 and 'event=typed-refusal advertised=58 acknowledged=1' in stdout
        assert '1 passed; 0 failed; 0 ignored;' in stdout


try:
    case('baseline')
    original.write_bytes(mutant)
    case('mutant')
finally:
    original.write_bytes(before)
    assert original.read_bytes() == before
    (out / 'restored-source.json').write_text(json.dumps({'restored_sha256': hashlib.sha256(original.read_bytes()).hexdigest()}) + '\n')
case('restored')
assert subprocess.check_output(['git', '-C', str(repo), 'diff', '--', 'crates/remote/src/managed.rs']) == b''
(out / 'completed.json').write_text(json.dumps({'source': source, 'baseline': 0, 'mutant': 101, 'restored': 0, 'actors_joined_each': True, 'whole_population_kernel_return': 'Unknown'}, indent=2) + '\n')
print('COMPLETE', source, flush=True)
