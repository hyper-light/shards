#!/usr/bin/env python3
"""Package exact sandbox + quote sources; retain the pinned small native closure."""
from pathlib import Path
import hashlib
import json
import re
import shutil
import subprocess
import sys
import tarfile

repo, dest = map(Path, sys.argv[1:])
dest.mkdir(exist_ok=False)
(dest / "src").mkdir()
shutil.copyfile(repo / "crates/server/src/sandbox.rs", dest / "src/sandbox.rs")
server = (repo / "crates/server/src/server.rs").read_text()
quote = server[server.index("pub(crate) fn quote("):server.index("\nfn http(")]
(dest / "src/server.rs").write_text(quote)
shutil.copyfile(Path(__file__).with_name("helper-main.rs"), dest / "src/main.rs")
(dest / "src/lib.rs").write_text("pub mod sandbox;\nmod server;\n")
for name in ["zig-cc", "zig-ar"]:
    target = dest / "scripts" / name
    target.parent.mkdir(exist_ok=True)
    shutil.copyfile(repo / "scripts" / name, target)
    target.chmod(0o700)
shutil.copyfile(repo / "clippy.toml", dest / "clippy.toml")
for relative in subprocess.check_output(
    ["git", "ls-files", "vendor/rquickjs-sys"], cwd=repo, text=True
).splitlines():
    target = dest / relative
    target.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(repo / relative, target)
(dest / "Cargo.toml").write_text('''[package]
name = "managed-helper-ownership"
version = "0.1.0"
edition = "2024"

[dependencies]
libc = "=0.2.189"
rquickjs = { version = "=0.14.0", default-features = false }

[patch.crates-io]
rquickjs-sys = { path = "vendor/rquickjs-sys" }

[profile.release]
opt-level = 2
debug = false
codegen-units = 1
''')
workspace = (repo / "Cargo.toml").read_text()
lints = workspace[workspace.index("[workspace.lints.rust]"):workspace.index("# The TLS provider")]
with (dest / "Cargo.toml").open("a") as manifest:
    manifest.write("\n" + lints.replace("[workspace.lints.", "[lints."))
blocks = (repo / "Cargo.lock").read_text().split("[[package]]\n")[1:]
by_name = {}
for block in blocks:
    name = re.search(r'^name = "([^"]+)"', block, re.M).group(1)
    by_name.setdefault(name, []).append(block)
# Exact feature-reduced lock produced by the retained offline metadata step;
# every version/source/checksum matches the repository, including cc without
# the unrelated workspace users' parallel feature. The initial rejected lock
# and the rejected generate-lockfile version update remain historical evidence.
frozen = Path(__file__).with_name("light-Cargo.lock")
assert hashlib.sha256(frozen.read_bytes()).hexdigest() == "b85862e582dae5b92ca1ad2daf5fe9be2769c5fc1985573acd56570d470c8f1f"
selected = []
for block in frozen.read_text().split("[[package]]\n")[1:]:
    name = re.search(r'^name = "([^"]+)"', block, re.M).group(1)
    version = re.search(r'^version = "([^"]+)"', block, re.M).group(1)
    if name == "managed-helper-ownership":
        continue
    matches = [b for b in by_name[name] if f'version = "{version}"\n' in b]
    assert len(matches) == 1, (name, version)
    for field in ["source", "checksum"]:
        pattern = rf'^{field} = "([^"]+)"'
        assert re.findall(pattern, block, re.M) == re.findall(pattern, matches[0], re.M)
    selected.append((name, version))
shutil.copyfile(frozen, dest / "Cargo.lock")
files = {str(p.relative_to(dest)): hashlib.sha256(p.read_bytes()).hexdigest()
         for p in sorted(dest.rglob("*")) if p.is_file()}
(dest / "source-hashes.json").write_text(json.dumps(files, indent=2) + "\n")
archive = dest.with_suffix(".tar.gz")
with tarfile.open(archive, "w:gz") as tar:
    for path in sorted(dest.rglob("*")):
        if path.is_file():
            tar.add(path, arcname=str(path.relative_to(dest)))
print(json.dumps({"archive": str(archive), "bytes": archive.stat().st_size,
                  "sha256": hashlib.sha256(archive.read_bytes()).hexdigest(),
                  "source_files": len(files), "packages": sorted(selected)}))
