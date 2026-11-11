#!/usr/bin/env python3
"""One GNU compiler/seven-writer-case/strict batch with one absolute deadline."""
import argparse
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import platform
import selectors
import signal
import subprocess
import tarfile
import time
import traceback

parser = argparse.ArgumentParser()
parser.add_argument('--input', type=Path, required=True)
parser.add_argument('--root', type=Path, required=True)
args = parser.parse_args()
began = time.monotonic()
deadline = began + 585  # User600s outer bound, final15s direct-child reap allowance.
args.root.mkdir(mode=0o700, exist_ok=False)
records = args.root / 'records'
records.mkdir()
source = args.root / 'source'
source.mkdir()
for name in ['target', 'cargo-home', 'tmp', 'frozen']:
    (args.root / name).mkdir()


def save(name, value):
    with (records / name).open('x') as output:
        json.dump(value, output, indent=2)
        output.write('\n')


manifest = json.loads((args.input / 'source.json').read_bytes())
archive = (args.input / 'source.tar.gz').read_bytes()
if len(archive) != manifest['archive_bytes'] or hashlib.sha256(archive).hexdigest() != manifest['archive_sha256']:
    raise ValueError('archive identity mismatch')
identities = {row['path']: row for row in manifest['files']}
seen = set()
with tarfile.open(fileobj=io.BytesIO(gzip.decompress(archive))) as tar:
    for member in tar:
        rel = Path(member.name)
        if not member.isfile() or rel.is_absolute() or '..' in rel.parts or member.name in seen:
            raise ValueError('unsafe or duplicate source member')
        seen.add(member.name)
        body = tar.extractfile(member).read()
        row = identities[member.name]
        if len(body) != row['bytes'] or hashlib.sha256(body).hexdigest() != row['sha256']:
            raise ValueError('source identity mismatch')
        target = source / member.name
        target.parent.mkdir(parents=True, exist_ok=True)
        with target.open('xb') as output:
            output.write(body)
        target.chmod(member.mode)
if seen != set(identities):
    raise ValueError('missing source input')
save('source.json', manifest)
affinity = sorted(os.sched_getaffinity(0))
os.sched_setaffinity(0, affinity[:2])
save('environment.json', dict(uname=list(platform.uname()), system=platform.platform(),
     affinity_before=affinity, affinity_selected=sorted(os.sched_getaffinity(0)),
     root=str(args.root), source_files=len(seen), outer_seconds=600, jobs=2,
     free_bytes=os.statvfs(args.root).f_bavail * os.statvfs(args.root).f_frsize,
     loader_variable_names_present=[name for name in os.environ if name.startswith(('LD_', 'DYLD_'))]))
env = dict(os.environ, CARGO_HOME=str(args.root / 'cargo-home'), CARGO_TARGET_DIR=str(args.root / 'target'),
           CARGO_BUILD_JOBS='2', CARGO_INCREMENTAL='0', TMPDIR=str(args.root / 'tmp'))
results = []
cases = [
    'cancellation_before_commit_and_empty_write_preserve_actual_file',
    'checked_range_refuses_overflow_without_opening_or_mutating_an_object',
    'directory_object_is_returned_and_never_used_for_write',
    'literal_positional_write_preserves_prefix_suffix_and_shared_offset',
    'prepared_object_survives_rename_and_does_not_write_replacement',
    'readonly_and_append_modes_refuse_before_any_bytes_change',
    'same_canonical_audit_reserves_intent_and_terminal_before_ordinary_effect',
]
cases = ['native_effects::write::tests::' + name for name in cases]


def command(name, argv, budget):
    start = time.monotonic()
    remaining = deadline - start
    if remaining < budget:
        raise TimeoutError('insufficient original deadline for ' + name)
    before = len(os.listdir('/proc/self/fd'))
    save(name + '.command.json', dict(argv=argv, cwd=str(source), budget_s=budget,
         remaining_original_s=remaining, env={key: env[key] for key in ['CARGO_HOME', 'CARGO_TARGET_DIR', 'CARGO_BUILD_JOBS', 'CARGO_INCREMENTAL', 'TMPDIR']}))
    with (records / (name + '.stdout')).open('xb') as out, (records / (name + '.stderr')).open('xb') as err:
        child = subprocess.Popen(argv, cwd=source, env=env, stdout=out, stderr=err, start_new_session=True)
        pidfd = os.pidfd_open(child.pid)
        selector = selectors.DefaultSelector()
        selector.register(pidfd, selectors.EVENT_READ)
        ready = bool(selector.select(budget))
        selector.close()
        if not ready:
            os.killpg(child.pid, signal.SIGKILL)
        code = child.wait(timeout=15)
        os.close(pidfd)
        try:
            os.waitpid(child.pid, os.WNOHANG)
            reaped = False
        except ChildProcessError:
            reaped = True
    result = dict(name=name, argv=argv, cwd=str(source), exit_code=code,
                  exit_ready_before_wait=ready, waited_pid=child.pid, direct_child_reaped_echild=reaped,
                  elapsed_seconds=time.monotonic() - start, parent_fd_before=before,
                  parent_fd_after=len(os.listdir('/proc/self/fd')))
    results.append(result)
    save(name + '.json', result)
    print(json.dumps(result), flush=True)
    if not ready or not reaped or before != result['parent_fd_after']:
        raise RuntimeError('deadline/direct-child/FD observation failed: ' + name)
    return result


cargo = ['cargo', '+1.98.1']
common = ['-p', 'shards-server', '--bin', 'shards-server', '--locked', '--target', 'x86_64-unknown-linux-gnu', '--profile', 'test-release', '-j2']
error = None
try:
    for name, argv in [('rust-version', ['rustc', '+1.98.1', '-Vv']), ('cargo-version', cargo + ['-Vv'])]:
        if command(name, argv, 15)['exit_code']:
            raise RuntimeError('tool query failed')
    argv = cargo + ['test'] + common + ['--no-run', '--message-format=json']
    if command('compile', argv, 180)['exit_code']:
        raise RuntimeError('actual original-path server test graph failed before writer cases')
    artifacts = []
    for line in (records / 'compile.stdout').read_text().splitlines():
        try:
            item = json.loads(line)
        except ValueError:
            continue
        if item.get('reason') == 'compiler-artifact' and item.get('profile', {}).get('test') and item.get('executable'):
            artifacts.append(item['executable'])
    if len(artifacts) != 1:
        raise RuntimeError('one exact server test executable required')
    binary = Path(artifacts[0])
    frozen = args.root / 'frozen' / 'writer-tests'
    body = binary.read_bytes()
    with frozen.open('xb') as output:
        output.write(body)
    frozen.chmod(0o700)
    save('binary.json', dict(artifact=str(binary), frozen=str(frozen), bytes=len(body), sha256=hashlib.sha256(body).hexdigest()))
    if command('list', [str(frozen), '--list'], 15)['exit_code']:
        raise RuntimeError('exact executable listing failed')
    listed = [line[:-len(': test')] for line in (records / 'list.stdout').read_text().splitlines() if line.endswith(': test')]
    actual = [case for case in listed if case.startswith('native_effects::write::tests::')]
    if actual != cases:
        raise RuntimeError('original seven case predicates differ')
    save('precase.json', dict(binary_sha256=hashlib.sha256(frozen.read_bytes()).hexdigest(),
         exact_cases=cases, compiled_unselected_cases=[name for name in listed if name not in cases],
         no_held_native_write_record_tests_linked=not any('native_write::tests::' in name for name in listed),
         run_count_each=1, source_manifest_sha256=hashlib.sha256((records / 'source.json').read_bytes()).hexdigest()))
    print('EXACT_SOURCE_BINARY_LIST_BEFORE_FIRST_CASE', flush=True)
    for i, case in enumerate(cases):
        name = 'case-' + str(i)
        result = command(name, [str(frozen), case, '--exact', '--nocapture', '--test-threads=1'], 15)
        output = (records / (name + '.stdout')).read_text()
        if result['exit_code'] or '1 passed; 0 failed; 0 ignored' not in output or 'SKIP' in output:
            raise RuntimeError('first actual ordinary writer failure; no retry: ' + case)
    if command('strict-tests', cargo + ['clippy'] + common + ['--tests', '--', '-D', 'warnings'], 180)['exit_code']:
        raise RuntimeError('actual original-path server test strict failure')
    if command('format-owned', ['rustfmt', '+1.98.1', '--check', '--edition', '2024', '--config', 'skip_children=true',
               'crates/server/src/native_effects/write.rs', 'crates/server/src/native_effects/write/tests.rs'], 15)['exit_code']:
        raise RuntimeError('owned source format failure')
except BaseException as exc:
    error = str(exc)
    (records / 'driver-error.txt').write_text(traceback.format_exc())
finally:
    changed = []
    for name, row in identities.items():
        path = source / name
        if not path.is_file() or hashlib.sha256(path.read_bytes()).hexdigest() != row['sha256']:
            changed.append(name)
    save('summary.json', dict(results=results, error=error, source_unchanged=not changed,
         changed_paths=changed, source_files=len(identities), selected_cases=cases,
         actual_writer_cases=sum(result['name'].startswith('case-') for result in results),
         held_native_write_record_cases_executed=0, native_owner_admission_proved=False,
         faults_or_partial_writes_injected=False, no_production_semantics_change=True,
         tmp_entries=[str(path.relative_to(args.root / 'tmp')) for path in sorted((args.root / 'tmp').rglob('*'))],
         elapsed_seconds=time.monotonic() - began))
    rows = []
    for path in sorted(records.iterdir()):
        if path.is_file():
            data = path.read_bytes()
            rows.append(dict(path=path.name, bytes=len(data), sha256=hashlib.sha256(data).hexdigest()))
    save('manifest.json', rows)
raise SystemExit(1 if error or changed else 0)
