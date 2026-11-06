#!/usr/bin/env python3
"""Compare the unchanged 6000-rule compiler workload, without editing either source.

Usage: wide-policy.py WORKSPACE BASELINE_REV NEW_SCRATCH [--n 30]
Builds isolated production-release binaries, then alternates their execution order.
The scratch directory must not exist. It retains sources, locks, binaries and raw rows.
"""
import argparse
import hashlib
import io
import json
import math
from pathlib import Path
import platform
import shutil
import subprocess
import tarfile
import tomllib


def command(args, **kwargs):
    return subprocess.check_output(args, text=True, **kwargs).strip()


def digest_tree(path):
    digest = hashlib.sha256()
    for file in sorted(path.rglob('*')):
        if file.is_file():
            digest.update(str(file.relative_to(path)).encode())
            digest.update(b'\0')
            digest.update(file.read_bytes())
    return digest.hexdigest()


def build(study, source, versions, harness):
    study.mkdir()
    for name in ('rego', 'template'):
        shutil.copytree(source / 'crates' / name / 'src', study / name / 'src')
    (study / 'template' / 'Cargo.toml').write_text('''[package]
name = "shards-template"
version = "0.1.0"
edition = "2024"
''')
    (study / 'rego' / 'Cargo.toml').write_text('''[package]
name = "shards-rego"
version = "0.1.0"
edition = "2024"
[dependencies]
num-bigint = "=NUM"
regex = "=REGEX"
serde_json = "=JSON"
shards-template = { path = "../template" }
[target.'cfg(unix)'.dependencies]
libc = "=LIBC"
[target.'cfg(windows)'.dependencies]
windows-sys = { version = "=WIN", features = ["Win32_System_Memory"] }
'''.replace('NUM', versions['num-bigint']).replace('REGEX', versions['regex'])
        .replace('JSON', versions['serde_json']).replace('LIBC', versions['libc'])
        .replace('WIN', versions['windows-sys']))
    (study / 'src').mkdir()
    (study / 'src' / 'main.rs').write_text(harness)
    (study / 'Cargo.toml').write_text('''[package]
name = "wide-policy"
version = "0.1.0"
edition = "2024"
[workspace]
[dependencies]
shards-rego = { path = "rego" }
[profile.release]
opt-level = 3
lto = "fat"
codegen-units = 1
''')
    with (study / 'build.log').open('w') as out:
        subprocess.run(['cargo', 'build', '--release', '--offline', '-j', '1'],
                       cwd=study, stdout=out, stderr=subprocess.STDOUT, check=True)
    return study / 'target' / 'release' / ('wide-policy.exe' if platform.system() == 'Windows' else 'wide-policy')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('workspace', type=Path)
    parser.add_argument('baseline_revision')
    parser.add_argument('scratch', type=Path)
    parser.add_argument('--n', type=int, default=30)
    args = parser.parse_args()
    if args.n < 1:
        parser.error('--n must be positive')
    root, scratch = args.workspace.resolve(), args.scratch.resolve()
    scratch.mkdir()  # Refuse to overwrite a previous experiment.
    baseline = command(['git', 'rev-parse', '--verify', args.baseline_revision + '^{commit}'], cwd=root)
    data = subprocess.check_output(['git', 'archive', baseline, 'crates/rego/src', 'crates/template/src'], cwd=root)
    before = scratch / 'before-source'
    before.mkdir()
    with tarfile.open(fileobj=io.BytesIO(data)) as archive:
        archive.extractall(before, filter='data')
    packages = tomllib.loads((root / 'Cargo.lock').read_text())['package']
    versions = {}
    for name in ('num-bigint', 'regex', 'serde_json', 'libc', 'windows-sys'):
        matches = {p['version'] for p in packages if p['name'] == name}
        if name == 'windows-sys':
            version = tomllib.loads((root / 'Cargo.toml').read_text())['workspace']['dependencies'][name]['version'].removeprefix('=')
            matches = {v for v in matches if v == version or v.startswith(version + '.')}
        if len(matches) != 1:
            raise SystemExit(f'Expected one locked version of {name}: {matches}')
        versions[name] = matches.pop()
    test = (root / 'crates/rego/tests/compile.rs').read_text()
    host = test[test.index('fn host()'):test.index('\n#[test]')]
    layers = test[test.index('fn layers('):test.index('\n/// The recursion check')]
    harness = '''use std::collections::BTreeMap;
use shards_rego::compile::{Compiler, Function};
use shards_rego::parser::parse_module;
use shards_rego::types::Type;
HOST
LAYERS
fn main() {
    let row = std::thread::Builder::new().stack_size(123 << 20).spawn(|| {
        let start = std::time::Instant::now();
        let mut modules = BTreeMap::new();
        modules.insert("builtin/defaults.rego".to_owned(), parse_module("builtin/defaults.rego", include_str!("../rego/src/defaults.rego")).unwrap());
        modules.insert("layers.rego".to_owned(), parse_module("layers.rego", &layers(500, 12)).unwrap());
        let mut compiler = Compiler::new(modules, host(), true);
        let compile = std::time::Instant::now();
        compiler.compile();
        let compile_us = compile.elapsed().as_secs_f64() * 1e6;
        let total_us = start.elapsed().as_secs_f64() * 1e6;
        assert!(compiler.errors.is_empty(), "{:?}", compiler.errors);
        (compile_us, total_us)
    }).unwrap().join().unwrap();
    println!("{{\\"compile_us\\":{},\\"total_us\\":{}}}", row.0, row.1);
}
'''.replace('HOST', host).replace('LAYERS', layers)
    binaries = {}
    for name, source in [('before', before), ('after', root)]:
        print('Building ' + name, flush=True)
        binaries[name] = build(scratch / name, source, versions, harness)
    metadata = {
        'host': platform.node(), 'os': platform.platform(), 'machine': platform.machine(),
        'baseline_revision': baseline,
        'candidate_head': command(['git', 'rev-parse', 'HEAD'], cwd=root),
        'candidate_diff_sha256': hashlib.sha256(subprocess.check_output(['git', 'diff', 'HEAD', '--', 'crates/rego', 'crates/template'], cwd=root)).hexdigest(),
        'rustc': {name: command(['rustc', '--version'], cwd=scratch / name) for name in binaries},
        'versions': versions, 'n_per_build': args.n,
        'profile': 'release; opt-level=3; fat LTO; codegen-units=1',
        'workload': 'compile.rs host() and layers(500,12), including parse; 123 MiB stack',
        'harness_sha256': hashlib.sha256(harness.encode()).hexdigest(),
        'source_sha256': {name: digest_tree(scratch / name / 'rego' / 'src') for name in binaries},
    }
    (scratch / 'metadata.json').write_text(json.dumps(metadata, indent=2) + '\n')
    for binary in binaries.values():
        command([str(binary)])  # One unrecorded process per build to warm file pages.
    rows = []
    with (scratch / 'rows.jsonl').open('w') as out:
        for iteration in range(args.n):
            order = ['before', 'after'] if iteration % 2 == 0 else ['after', 'before']
            for name in order:
                row = json.loads(command([str(binaries[name])]))
                row.update(build=name, iteration=iteration)
                rows.append(row)
                out.write(json.dumps(row) + '\n')
                out.flush()
    summary = {}
    for name in binaries:
        summary[name] = {}
        for field in ('compile_us', 'total_us'):
            values = sorted(r[field] for r in rows if r['build'] == name)
            percentile = lambda p: values[max(0, math.ceil(len(values) * p) - 1)]
            summary[name][field] = {'n': len(values), 'p50': percentile(.5), 'p90': percentile(.9), 'p99': percentile(.99), 'max': values[-1]}
    (scratch / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
    print(json.dumps(summary, indent=2), flush=True)


if __name__ == '__main__':
    main()
