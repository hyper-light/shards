#!/usr/bin/env python3
"""Compile full owned fixture bodies; execute ONLY extracted effect-free byte calculations."""
from pathlib import Path
import runpy
import shutil
import sys

repo, target = map(Path, sys.argv[1:])
sys.argv = ["prepare-inherited-light.py", str(repo), str(target)]
runpy.run_path(str(repo / "docs/research/measurements/managed-tenant-stdio/prepare-inherited-light.py"), run_name="__main__")
host = (repo / "crates/shards/tests/managed_output/tenant_inherited.rs").read_text()
guest = (repo / "crates/testguest/src/workload.rs").read_text()
(target / "src/host-envelope.rs").write_text(host[host.index("fn inherited_report_bytes("):host.index("impl InheritedInputs")])
(target / "src/guest-envelope.rs").write_text(guest[guest.index("fn managed_capture_inherited_required("):guest.index("#[derive(Clone, Copy)]",guest.index("fn managed_capture_inherited_required("))])
shutil.copyfile(repo / "docs/research/measurements/managed-tenant-stdio/envelope-tests.rs",target / "src/envelope_tests.rs")
lib = target / "src/lib.rs"
lib.write_text(lib.read_text() + '\nuse std::io;\ninclude!("host-envelope.rs");\ninclude!("guest-envelope.rs");\n#[cfg(test)]\nmod envelope_tests;\n')
