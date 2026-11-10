#!/usr/bin/env python3
"""Exact inherited-writer source; surrounding integration stays inert and unexecuted."""
from pathlib import Path
import runpy
import shutil
import sys

repo, target = map(Path, sys.argv[1:])
sys.argv = ["prepare-host-light.py", str(repo), str(target)]
runpy.run_path(str(repo / "docs/research/measurements/managed-tenant-stdio/prepare-host-light.py"), run_name="__main__")
lib = target / "src/lib.rs"
lib.write_text(lib.read_text().replace("_: &str, _: Option<&[u16]>", "_: &str, _: Option<u32>, _: Option<&[u16]>").replace("mod helper {", "mod helper {\n    #![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::panic_in_result_fn, clippy::todo, clippy::unimplemented, clippy::unreachable, clippy::indexing_slicing, clippy::string_slice)]"))
shutil.copyfile(repo / "crates/shards/tests/managed_output/tenant_inherited.rs", target / "src/tenant_inherited.rs")
shutil.copyfile(repo / "crates/shards/tests/managed_output/tenant_inherited.rs", target / "original/tenant_inherited.rs")
host = (repo / "crates/shards/tests/managed_output.rs").read_text()
a = host.index("        let processes = agent_tasks")
b = host.index("        let (image, _) = common::served();", a)
x = host.index("        let run = |args: &[&str]|")
y = host.index("        assert_eq!(started.status, Some(0)", x)
agentfile = next(line for line in host.splitlines() if 'std::fs::write(self.context.join("Agentfile"), format!' in line)
(target / "src/publication.rs").write_text('''#![allow(dead_code)]
use std::time::{Duration, Instant};
use std::path::PathBuf;
struct Run { status: Option<i32> }
struct Fixture { context: PathBuf }
impl Fixture {
    fn command(&self, _: &[&str], _: Duration) -> Run { Run {status:None} }
    fn compile_only(&self, agent_tasks: Option<u32>, publish_ports: Option<&[u16]>, until: Instant) {
''' + host[a:b] + '''        let image = "compile-only";
        let directives = "";
''' + agentfile + '\n' + host[x:y] + '''        let _ = started.status;
    }
}
''')
