#!/usr/bin/env python3
"""Compile actual host fixture with inert type-matching common seam; no VM helper runs."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys

root=Path.cwd()
proof=Path(sys.argv[1]).expanduser().resolve()
proof.mkdir(parents=True,exist_ok=False)
actual=root/'crates/shards/tests/managed_network.rs'
(proof/'fixture.rs').write_bytes(actual.read_bytes())
(proof/'source-sha256.json').write_text(json.dumps({str(actual.relative_to(root)):hashlib.sha256(actual.read_bytes()).hexdigest()})+'\n')
# These are compile scaffolding, not the actual VM helper implementation. Run.status
# exactly matches common::Run (Option<i32>); all VM entrypoints are inert panics.
(proof/'common.rs').write_text('''#![allow(dead_code)]
use std::{ffi::OsStr,path::{Path,PathBuf},time::Duration,fmt};
pub struct Run {pub status:Option<i32>,pub signal:Option<i32>,pub stdout:String,pub stderr:String,pub elapsed:Duration}
impl fmt::Display for Run {fn fmt(&self,f:&mut fmt::Formatter<'_>)->fmt::Result {write!(f,"light-only")}}
pub struct TempDir(PathBuf);
impl TempDir {pub fn new(_: &str)->Self {panic!("VM helper forbidden in light check")}}
impl std::ops::Deref for TempDir {type Target=Path;fn deref(&self)->&Path {&self.0}}
pub fn test_guest()->&'static Path {Path::new("/LIGHT-ONLY-NO-GUEST")}
pub fn shards()->&'static Path {Path::new("/LIGHT-ONLY-NO-CLI")}
pub fn sha256_digest(_: &[u8])->String {panic!("VM identity helper forbidden in light check")}
pub fn cannot_run_vms()->bool {true}
pub fn run_shards_env<S:AsRef<OsStr>>(_: &[&str],_: &[S],_: &[(&str,&OsStr)],_:Duration)->Run {panic!("VM execution forbidden in light check")}
''')
(proof/'Cargo.toml').write_text('''[package]
name="sock-addr-host-pure"
version="0.0.0"
edition="2024"
[workspace]
[lib]
path="fixture.rs"
[dependencies]
serde_json="=1.0.151"
''')
env=dict(os.environ,CARGO_TARGET_DIR=str(proof/'target'))
commands=[
 ['cargo','+1.98.1','test','--offline','--manifest-path',str(proof/'Cargo.toml'),'--lib','--no-run','-j4'],
 ['cargo','+1.98.1','test','--offline','--manifest-path',str(proof/'Cargo.toml'),'--lib','socket_address_record_parser_keeps_actual_tuple_and_failed_verdict','--','--exact'],
]
results=[]
for index,command in enumerate(commands):
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
