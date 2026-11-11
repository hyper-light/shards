#!/usr/bin/env python3
"""Exactly two 9c73 cases in a fresh historical8b1 compiler closure."""
import base64
import hashlib
import json
import os
from pathlib import Path
import platform
import resource
import selectors
import shutil
import signal
import stat
import subprocess
import sys
import time
import traceback

started = time.monotonic()
started_wall = time.time()
hard_deadline = started + 600
work_deadline = hard_deadline - 15
config = json.loads(base64.b64decode(sys.argv[1], validate=True))
root = Path(config['root'])
root.mkdir(mode=0o700, exist_ok=False)
for name in ['records', 'source', 'cargo-home', 'target', 'tmp', 'frozen']:
    (root / name).mkdir()
records = root / 'records'
source = root / 'source'
template = Path(config['template_root'])
template_source = template / 'source'
template_registry = template / 'cargo-home/registry'
results = []
expected_source = None
template_manifest = None
registry_before = None
old_binary_before = None
new_binary_before = None
error = None


def save(name, value):
    with (records / name).open('x') as output:
        json.dump(value, output, indent=2)
        output.write('\n')


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def limits():
    return {name: list(resource.getrlimit(getattr(resource, name))) for name in
            ['RLIMIT_AS', 'RLIMIT_DATA', 'RLIMIT_FSIZE', 'RLIMIT_NOFILE', 'RLIMIT_NPROC', 'RLIMIT_CORE']}


def tree_identity(base):
    rows = []
    for parent, dirs, files in os.walk(base, followlinks=False):
        parent = Path(parent)
        for name in dirs:
            if (parent / name).is_symlink():
                raise ValueError('nonregular registry directory')
        for name in sorted(files):
            path = parent / name
            info = path.lstat()
            if not stat.S_ISREG(info.st_mode):
                raise ValueError('nonregular registry file')
            rows.append(dict(path=path.relative_to(base).as_posix(), bytes=info.st_size,
                             sha256=digest(path), mode=stat.S_IMODE(info.st_mode),
                             mtime_ns=info.st_mtime_ns, inode=info.st_ino))
    return sorted(rows, key=lambda row: row['path'])


def source_identity(base, manifest):
    rows = []
    for row in manifest['files']:
        path = base / row['path']
        info = path.lstat()
        if not stat.S_ISREG(info.st_mode):
            raise ValueError('nonregular source: ' + row['path'])
        data = path.read_bytes()
        actual = hashlib.sha256(data).hexdigest()
        if len(data) != row['bytes'] or actual != row['sha256']:
            raise ValueError('source identity changed: ' + row['path'])
        rows.append(dict(path=row['path'], bytes=len(data), sha256=actual))
    return rows


def binary_identity(path):
    data = path.read_bytes()
    return dict(path=str(path), bytes=len(data), sha256=hashlib.sha256(data).hexdigest(),
                header_hex=data[:20].hex(), is_elf=data.startswith(b'\x7fELF'))


affinity = sorted(os.sched_getaffinity(0))
os.sched_setaffinity(0, affinity[:2])
limits_before = limits()
save('input.json', config)
save('environment.json', dict(uname=list(platform.uname()), system=platform.platform(),
     affinity_before=affinity, affinity_selected=sorted(os.sched_getaffinity(0)),
     limits_before=limits_before, requested_cpus=2, requested_memory='2GiB', requested_disk='4GiB',
     cargo_jobs=2, started_wall=started_wall, started_monotonic=started,
     hard_deadline_monotonic=hard_deadline, work_deadline_monotonic=work_deadline,
     hard_deadline_wall=started_wall + 600, work_seconds=585, reap_allowance_seconds=15,
     free_bytes=os.statvfs(root).f_bavail * os.statvfs(root).f_frsize,
     loader_variable_names_present=[name for name in os.environ if name.startswith(('LD_', 'DYLD_'))],
     accounting='Normal Kiln admission request; no hard-memory, backing or kernel-cost inference.'))
env = dict(os.environ, CARGO_HOME=str(root / 'cargo-home'), CARGO_TARGET_DIR=str(root / 'target'),
           CARGO_BUILD_JOBS='2', CARGO_INCREMENTAL='0', TMPDIR=str(root / 'tmp'))


def command(name, argv, budget):
    began = time.monotonic()
    remaining = work_deadline - began
    if remaining < budget:
        raise TimeoutError('original absolute deadline lacks full budget: ' + name)
    before = len(os.listdir('/proc/self/fd'))
    save(name + '.command.json', dict(argv=argv, cwd=str(source), budget_s=budget,
         remaining_original_s=remaining, started_wall=time.time(),
         env={key: env[key] for key in ['CARGO_HOME', 'CARGO_TARGET_DIR', 'CARGO_BUILD_JOBS', 'CARGO_INCREMENTAL', 'TMPDIR']}))
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
                  parent_fd_before=before, parent_fd_after=len(os.listdir('/proc/self/fd')),
                  elapsed_seconds=time.monotonic() - began, finished_wall=time.time(),
                  finished_before_original_work_deadline=time.monotonic() < work_deadline)
    results.append(result)
    save(name + '.json', result)
    print(json.dumps(result), flush=True)
    if code or not ready or not reaped or before != result['parent_fd_after']:
        raise RuntimeError('first setup/compiler/format/case failure; no retry: ' + name)


try:
    data = (template / 'records/source.json').read_bytes()
    if hashlib.sha256(data).hexdigest() != config['template_manifest_sha256']:
        raise ValueError('historical template manifest mismatch')
    template_manifest = json.loads(data)
    if len(template_manifest['files']) != config['template_file_count']:
        raise ValueError('historical template file count mismatch')
    save('template-source-before.json', source_identity(template_source, template_manifest))
    old_binary_before = binary_identity(template / 'frozen/writer-tests')
    if (old_binary_before['sha256'], old_binary_before['bytes']) != (config['template_binary_sha256'], config['template_binary_bytes']):
        raise ValueError('historical frozen executable mismatch')
    save('template-binary-before.json', old_binary_before)
    registry_before = tree_identity(template_registry)
    if not registry_before:
        raise ValueError('historical private registry cache is absent')
    save('template-registry-before.json', registry_before)
    # Read/copy only the original public registry cache. New Cargo metadata and
    # locks are created in the fresh CARGO_HOME, never in the historical cache.
    for row in registry_before:
        src = template_registry / row['path']
        dst = root / 'cargo-home/registry' / row['path']
        dst.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(src, dst, follow_symlinks=False)
        if digest(dst) != row['sha256']:
            raise ValueError('private registry copy mismatch')
    for row in template_manifest['files']:
        src = template_source / row['path']
        dst = source / row['path']
        dst.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(src, dst, follow_symlinks=False)
    expected_source = json.loads(json.dumps(template_manifest))
    rows = {row['path']: row for row in expected_source['files']}
    for row in config['overlays']:
        body = base64.b64decode(row['base64'], validate=True)
        if len(body) != row['bytes'] or hashlib.sha256(body).hexdigest() != row['sha256']:
            raise ValueError('exact9c73 overlay identity mismatch')
        (source / row['path']).write_bytes(body)
        rows[row['path']].update(bytes=len(body), sha256=row['sha256'], origin=config['revision'] + ': exact signed leaf')
    save('source.json', expected_source)
    save('source-before.json', source_identity(source, expected_source))
    for name, expected in config['original_matching_input_pins'].items():
        if digest(source / name) != expected:
            raise ValueError('original matching historical manifest/lock changed')
    if (source / 'crates/server/src/broker/audit/native_write.rs').exists():
        raise ValueError('unexpected held native-write record module')
    save('historical-input-pins.json', dict(pins=config['original_matching_input_pins'],
         note='Actual original8b1 matching manifest+Cargo.lock; not current product lock, never regenerated.',
         only_overlaid_paths=[row['path'] for row in config['overlays']], held4819_graph_used=False))
    rustup = Path(os.environ.get('RUSTUP_HOME', str(Path.home() / '.rustup')))
    if not (rustup / 'toolchains/1.98.1-x86_64-unknown-linux-gnu/bin/rustc').is_file():
        raise ValueError('original1.98.1 toolchain absent; no installation permitted')
    cargo = ['cargo', '+1.98.1']
    common = ['-p', 'shards-server', '--locked', '--target', 'x86_64-unknown-linux-gnu', '--profile', 'test-release', '-j2']
    command('rust-version', ['rustc', '+1.98.1', '-Vv'], 15)
    command('cargo-version', cargo + ['-Vv'], 15)
    command('compile', cargo + ['test'] + common + ['--bin', 'shards-server', '--no-run', '--message-format=json'], 180)
    artifacts = []
    for line in (records / 'compile.stdout').read_text().splitlines():
        try:
            item = json.loads(line)
        except ValueError:
            continue
        if item.get('reason') == 'compiler-artifact' and item.get('profile', {}).get('test') and item.get('executable'):
            artifacts.append(item['executable'])
    if len(artifacts) != 1:
        raise ValueError('one exact whole-server TEST executable required')
    frozen = root / 'frozen/owned-writer-tests'
    shutil.copyfile(artifacts[0], frozen)
    frozen.chmod(0o700)
    new_binary_before = binary_identity(frozen)
    if not new_binary_before['is_elf']:
        raise ValueError('actual frozen test artifact is not ELF')
    new_binary_before['compiled_artifact'] = artifacts[0]
    save('binary-before.json', new_binary_before)
    command('list', [str(frozen), '--list'], 15)
    listed = [line[:-len(': test')] for line in (records / 'list.stdout').read_text().splitlines() if line.endswith(': test')]
    cases = config['exact_cases']
    if len(cases) != 2 or any(listed.count(case) != 1 for case in cases):
        raise ValueError('exact TWO NEW cases not present once in actual ELF')
    if any('native_write::tests::' in case for case in listed):
        raise ValueError('held audit-record cases unexpectedly linked')
    save('precase.json', dict(binary=new_binary_before, exact_cases=cases,
         allsource=source_identity(source, expected_source), compiled_unselected_cases=[case for case in listed if case not in cases],
         original_matching_input_pins=config['original_matching_input_pins'], original_seven_selected=False,
         held_audit_cases_linked=False, held4819_graph_used=False, run_count_each=1,
         source_manifest_sha256=digest(records / 'source.json'), recorded_before_any_case=True))
    print('ACTUAL_ALLSOURCE_ELF_LIST_BEFORE_EXACT_TWO_NEW_CASES', flush=True)
    for index, case in enumerate(cases):
        name = 'case-' + str(index)
        command(name, [str(frozen), case, '--exact', '--nocapture', '--test-threads=1'], 15)
        stdout = (records / (name + '.stdout')).read_text()
        if '1 passed; 0 failed; 0 ignored' not in stdout or 'SKIP' in stdout:
            raise ValueError('original exact single-case predicate failed; no retry')
    command('strict-tests', cargo + ['clippy'] + common + ['--tests', '--', '-D', 'warnings'], 180)
    command('format-owned', ['rustfmt', '+1.98.1', '--check', '--edition', '2024', '--config', 'skip_children=true',
            'crates/server/src/native_effects/write.rs', 'crates/server/src/native_effects/write/tests.rs'], 15)
except BaseException as exc:
    error = str(exc)
    (records / 'driver-error.txt').write_text(traceback.format_exc())
finally:
    verification_error = None
    try:
        if expected_source is not None:
            save('source-after.json', source_identity(source, expected_source))
        if template_manifest is not None:
            save('template-source-after.json', source_identity(template_source, template_manifest))
        if registry_before is not None:
            after = tree_identity(template_registry)
            save('template-registry-after.json', after)
            if after != registry_before:
                raise ValueError('historical read-only registry template changed')
        if old_binary_before is not None:
            after = binary_identity(template / 'frozen/writer-tests')
            save('template-binary-after.json', after)
            if after != old_binary_before:
                raise ValueError('historical frozen binary changed')
        if new_binary_before is not None:
            after = binary_identity(Path(new_binary_before['path']))
            save('binary-after.json', after)
            if any(after[key] != new_binary_before[key] for key in after):
                raise ValueError('new frozen binary changed')
    except BaseException as exc:
        verification_error = str(exc)
        (records / 'verification-error.txt').write_text(traceback.format_exc())
    save('summary.json', dict(results=results, error=error, verification_error=verification_error,
         actual_new_case_count=sum(row['name'].startswith('case-') for row in results), exact_cases=config['exact_cases'],
         old_seven_cases_executed=0, held_audit_record_cases_executed=0, held4819_graph_used=False,
         source_after_verified=expected_source is not None and verification_error is None,
         template_source_cache_binary_unchanged=template_manifest is not None and registry_before is not None and verification_error is None,
         new_frozen_binary_unchanged=new_binary_before is not None and verification_error is None,
         limits_before=limits_before, limits_after=limits(), limits_unchanged=limits_before == limits(),
         elapsed_seconds=time.monotonic() - started, finished_wall=time.time(),
         original_hard_deadline_wall=started_wall + 600, finished_before_original_deadline=time.monotonic() < hard_deadline,
         original_work_deadline_not_reset=True, requested_scope=config['scope'],
         tmp_entries=[str(path.relative_to(root / 'tmp')) for path in sorted((root / 'tmp').rglob('*'))],
         copy_backing_authority_readiness_or_leak_freedom_inferred=False))
    save('manifest.json', [dict(path=path.name, bytes=path.stat().st_size, sha256=digest(path))
         for path in sorted(records.iterdir()) if path.is_file()])
raise SystemExit(1 if error or verification_error else 0)
