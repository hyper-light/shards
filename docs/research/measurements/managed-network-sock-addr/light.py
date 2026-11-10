#!/usr/bin/env python3
"""Pure source-linked units/compilation only; never runs a guest or socket mode."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys

root = Path.cwd()
proof = Path(sys.argv[1]).expanduser().resolve()
proof.mkdir(parents=True, exist_ok=False)
source = proof / 'source'
source.mkdir()
for relative in ['crates/testguest/src/managed_network.rs', 'crates/testguest/src/managed_network', 'crates/devcgroup/src', 'docs/research/measurements/managed-network-intercept/ipv4-wire-before.hex']:
    origin = root / relative
    target = source / relative
    target.parent.mkdir(parents=True, exist_ok=True)
    if origin.is_dir():
        shutil.copytree(origin, target)
    else:
        shutil.copyfile(origin, target)
shutil.copyfile(source/'crates/testguest/src/managed_network.rs', source/'crates/testguest/src/managed_network/mod.rs')
# Copying into NEW directories, not retaining sync mtimes, makes freshness explicit.
manifest = {str(p.relative_to(source)): hashlib.sha256(p.read_bytes()).hexdigest()
            for p in sorted(source.rglob('*')) if p.is_file()}
(proof / 'source-sha256.json').write_text(json.dumps(manifest, indent=2)+'\n')
helper = proof / 'helper'
helper.mkdir()
(helper / 'Cargo.toml').write_text('''[package]
name = "sock-addr-pure"
version = "0.0.0"
edition = "2024"
[workspace]
[lib]
path = "lib.rs"
[dependencies]
libc = "=0.2.189"
shards-devcgroup = { path = "../devcgroup" }
''')
(helper / 'lib.rs').write_text('#![allow(dead_code)]\n#[path = '+json.dumps(str(source/'crates/testguest/src/managed_network/mod.rs'))+']\nmod managed_network;\n')
dev = proof / 'devcgroup'
dev.mkdir()
(dev / 'Cargo.toml').write_text('[package]\nname="shards-devcgroup"\nversion="0.0.0"\nedition="2024"\n[lib]\npath='+json.dumps(str(source/'crates/devcgroup/src/lib.rs'))+'\n')
env = dict(os.environ, CARGO_TARGET_DIR=str(proof/'target'))
commands = [
    ['rustup','run','1.98.1','rustc','--version','--verbose'],
    ['cargo','+1.98.1','test','--offline','--manifest-path',str(helper/'Cargo.toml'),'--lib','-j4'],
    ['cargo','+1.98.1','test','--offline','--manifest-path',str(helper/'Cargo.toml'),'--lib','--target','x86_64-unknown-linux-musl','-j4'],
    ['cargo','+1.98.1','clippy','--offline','--manifest-path',str(helper/'Cargo.toml'),'--lib','--target','x86_64-unknown-linux-gnu','-j4','--','-D','warnings'],
    ['cargo','+1.98.1','clippy','--offline','--manifest-path',str(helper/'Cargo.toml'),'--lib','--target','x86_64-unknown-linux-musl','-j4','--','-D','warnings'],
]
results=[]
for index, command in enumerate(commands):
    (proof/f'{index}.command.json').write_text(json.dumps(command)+'\n')
    with (proof/f'{index}.log').open('wb') as out:
        result=subprocess.run(command,env=env,stdout=out,stderr=subprocess.STDOUT,check=False)
    results.append(result.returncode)
    print(index,result.returncode,flush=True)
    if result.returncode:
        print((proof/f'{index}.log').read_text(),flush=True)
        break
(proof/'results.json').write_text(json.dumps(results)+'\n')
sys.exit(any(results))
