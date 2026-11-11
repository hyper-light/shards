#!/usr/bin/env python3
"""Finish test-only lint/format inside the original writer batch's deadline."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import selectors
import signal
import subprocess
import time

p = argparse.ArgumentParser()
p.add_argument('--source-root', type=Path, required=True)
p.add_argument('--out', type=Path, required=True)
a = p.parse_args()
# Root's final initial mkdir precedes source extraction and the first command.
# Subtract15s from the original driver's585s work deadline, rather than reset it.
original_cutoff_wall = a.source_root.stat().st_mtime + 570
remaining = original_cutoff_wall - time.time()
cutoff = time.monotonic() + remaining
if remaining < 180 + 15 + 15:
    raise SystemExit('original deadline lacks full lint/format/reap budgets')
a.out.mkdir(mode=0o700, exist_ok=False)
for name in ['records', 'target', 'cargo-home', 'tmp']:
    (a.out / name).mkdir()
records = a.out / 'records'
source = a.source_root / 'source'
manifest = json.loads((a.source_root / 'records/source.json').read_bytes())
binary = json.loads((a.source_root / 'records/binary.json').read_bytes())


def verify():
    for row in manifest['files']:
        if hashlib.sha256((source / row['path']).read_bytes()).hexdigest() != row['sha256']:
            raise ValueError('frozen source changed: ' + row['path'])
    if hashlib.sha256(Path(binary['frozen']).read_bytes()).hexdigest() != binary['sha256']:
        raise ValueError('frozen seven-case executable changed')


verify()
os.sched_setaffinity(0, sorted(os.sched_getaffinity(0))[:2])
env = dict(os.environ, CARGO_HOME=str(a.out / 'cargo-home'), CARGO_TARGET_DIR=str(a.out / 'target'),
           CARGO_BUILD_JOBS='2', CARGO_INCREMENTAL='0', TMPDIR=str(a.out / 'tmp'))
rows = []


def save(name, value):
    with (records / name).open('x') as output:
        json.dump(value, output, indent=2)
        output.write('\n')


save('deadline.json', dict(original_root=str(a.source_root), root_initial_mtime=a.source_root.stat().st_mtime,
     conservative_cutoff_wall=original_cutoff_wall, original_driver_work_seconds=585,
     conservative_work_seconds=570, remaining_at_entry=remaining, new_runtime_cases=0,
     scope='Compiler/formatter only; original seven cases already ran once. All original targets/raw/binary untouched; new output target/cache. No production source/caller/suppression change.'))


def command(name, argv, seconds):
    if cutoff - time.monotonic() < seconds:
        raise TimeoutError('insufficient original remaining deadline: ' + name)
    before = len(os.listdir('/proc/self/fd'))
    start = time.monotonic()
    save(name + '.command.json', dict(argv=argv, cwd=str(source), budget_s=seconds,
         remaining_original_s=cutoff - start,
         env={key: env[key] for key in ['CARGO_HOME', 'CARGO_TARGET_DIR', 'CARGO_BUILD_JOBS', 'CARGO_INCREMENTAL', 'TMPDIR']}))
    with (records / (name + '.stdout')).open('xb') as out, (records / (name + '.stderr')).open('xb') as err:
        child = subprocess.Popen(argv, cwd=source, env=env, stdout=out, stderr=err, start_new_session=True)
        pidfd = os.pidfd_open(child.pid)
        selector = selectors.DefaultSelector()
        selector.register(pidfd, selectors.EVENT_READ)
        ready = bool(selector.select(seconds))
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
    row = dict(name=name, exit_code=code, exit_ready_before_wait=ready, waited_pid=child.pid,
               direct_child_reaped_echild=reaped, parent_fd_before=before,
               parent_fd_after=len(os.listdir('/proc/self/fd')), elapsed_seconds=time.monotonic() - start)
    rows.append(row)
    save(name + '.json', row)
    print(json.dumps(row), flush=True)
    if code or not ready or not reaped or before != row['parent_fd_after']:
        raise RuntimeError('lint/format failed: ' + name)


error = None
try:
    command('strict-test-only', ['cargo', '+1.98.1', 'clippy', '-p', 'shards-server', '--tests', '--locked',
            '--target', 'x86_64-unknown-linux-gnu', '--profile', 'test-release', '-j2', '--', '-D', 'warnings'], 180)
    command('format-owned', ['rustfmt', '+1.98.1', '--check', '--edition', '2024', '--config', 'skip_children=true',
            'crates/server/src/native_effects/write.rs', 'crates/server/src/native_effects/write/tests.rs'], 15)
except BaseException as exc:
    error = str(exc)
finally:
    verify()
    save('summary.json', dict(results=rows, error=error, source_unchanged=True,
         frozen_binary_unchanged=True, original_target_untouched=True, actual_new_runtime_cases=0,
         original_cutoff_wall=original_cutoff_wall, finished_wall=time.time(),
         finished_before_conservative_original_cutoff=time.time() < original_cutoff_wall))
    save('manifest.json', [dict(path=path.name, bytes=path.stat().st_size,
         sha256=hashlib.sha256(path.read_bytes()).hexdigest()) for path in sorted(records.iterdir()) if path.is_file()])
raise SystemExit(1 if error else 0)
