#!/usr/bin/env python3
"""Exact stdio host/native fixture compile source; surrounding integration remains inert."""
from pathlib import Path
import runpy
import shutil
import sys

repo, target = map(Path, sys.argv[1:])
sys.argv = ["prepare-light.py", str(repo), str(target)]
runpy.run_path(str(repo / "docs/research/measurements/managed-tenant-exit/prepare-light.py"), run_name="__main__")
lib = target / "src/lib.rs"
lib.write_text(lib.read_text().replace("_: &str, _: Instant)", "_: &str, _: Option<&[u16]>, _: Instant)") + "\nmod publication;\n")
shutil.copyfile(repo / "crates/shards/tests/managed_output/tenant_stdio.rs", target / "src/tenant_stdio.rs")
original = target / "original"
original.mkdir()
for rel, name in [("crates/shards/tests/managed_output.rs", "managed_output.rs"), ("crates/shards/tests/managed_output/tenant_exit.rs", "tenant_exit.rs"), ("crates/shards/tests/managed_output/tenant_stdio.rs", "tenant_stdio.rs")]:
    shutil.copyfile(repo / rel, original / name)
shutil.copyfile(repo / "rustfmt.toml", target / "rustfmt.toml")
host = (repo / "crates/shards/tests/managed_output.rs").read_text()
a = host.index("        let publication = publish_ports.map(")
b = host.index("        let (image, _) = common::served();", a)
x = host.index("        let run = |args: &[&str]|")
y = host.index("        assert_eq!(started.status, Some(0)", x)
(target / "src/publication.rs").write_text('''#![allow(dead_code)]
use std::time::{Duration, Instant};
struct Run { status: Option<i32> }
struct Fixture;
impl Fixture {
    fn command(&self, _: &[&str], _: Duration) -> Run { Run {status:None} }
    fn compile_only(&self, publish_ports: Option<&[u16]>, until: Instant) {
''' + host[a:b] + host[x:y] + '''        let _ = started.status;
    }
}
''')
