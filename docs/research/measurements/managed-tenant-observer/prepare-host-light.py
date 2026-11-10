#!/usr/bin/env python3
"""Compile exact reconnect leaf against explicit inert current/proposed host seams."""
from pathlib import Path
import runpy
import shutil
import sys

repo, target = map(Path, sys.argv[1:])
sys.argv = ["prepare-light.py", str(repo), str(target)]
runpy.run_path(str(repo / "docs/research/measurements/managed-tenant-exit/prepare-light.py"), run_name="__main__")
lib = target / "src/lib.rs"
text = lib.read_text().replace("_: &str, _: Instant)", "_: &str, _: Option<u16>, _: Instant)")
text = text.replace("    End,\n}", "    End,\n    Lost(Option<u64>),\n    Failed(String),\n}", 1)
text = text.replace("    fn main_progress(", "    fn observe_at(&mut self, _: u32, _: Channel, _: Option<Scope>, _: bool, _: u64, _: Instant) -> usize { panic!(\"proposed cursor seam; compile-only stub\") }\n    fn main_progress(")
lib.write_text(text)
original = target / "original"
original.mkdir()
for name in ["tenant_exit.rs", "tenant_reconnect.rs", "tenant_observer.rs"]:
    source = repo / "crates/shards/tests/managed_output" / name
    shutil.copyfile(source, original / name)
    shutil.copyfile(source, target / "src" / name)
shutil.copyfile(repo / "rustfmt.toml", target / "rustfmt.toml")
