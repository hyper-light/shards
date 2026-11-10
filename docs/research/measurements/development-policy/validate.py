#!/usr/bin/env python3
"""Live baseline and deliberate policy-violation checks, with retained output."""

from pathlib import Path
import hashlib
import platform
import shutil
import subprocess
import sys
import tempfile


def main():
    root = Path(__file__).resolve().parents[4]
    files = ["AGENTS.md", "CLAUDE.md", "docs/development-rules.md",
             "docs/development-evidence.md", "resources/supported-targets.txt",
             "scripts/check-development-policy", "scripts/lint",
             ".github/workflows/ci.yml", ".github/workflows/release.yml", "Cargo.toml"]
    cases = [
        ("baseline", None, None, None, 0),
        ("panic-lint-relaxed", "Cargo.toml", 'panic = "deny"', 'panic = "warn"', 1),
        ("Intel-Mac-restored", ".github/workflows/ci.yml",
         "          - { target: aarch64-apple-darwin, runner: macos-15, test: true }",
         "          - { target: x86_64-apple-darwin, runner: macos-15, test: false }", 1),
        ("arm64-Windows-dropped", "resources/supported-targets.txt",
         "aarch64-pc-windows-msvc\n", "", 1),
        ("policy-pointer-removed", "CLAUDE.md", "docs/development-rules.md",
         "missing-policy.md", 1),
        ("label-only-edit-remains-valid", "CLAUDE.md", "[docs/development-rules.md](",
         "[policy](", 0),
        ("rule-omitted", "docs/development-rules.md", "## R16 —", "## Removed —", 1),
    ]
    revision = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()
    digest = hashlib.sha256((root / "docs/development-rules.md").read_bytes()).hexdigest()
    rows = ["Development-policy live checks", "Base revision " + revision,
            "Policy SHA256 " + digest, platform.platform(), "Python " + " ".join(sys.version.split())]
    for name, path, old, new, expected in cases:
        with tempfile.TemporaryDirectory(prefix="shards-policy-check-") as temp:
            dest = Path(temp)
            for file in files:
                target = dest / file
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(root / file, target)
            if path:
                target = dest / path
                data = target.read_text()
                if old not in data:
                    raise ValueError("mutation target missing: " + name)
                target.write_text(data.replace(old, new))
            result = subprocess.run([sys.executable, str(dest / "scripts/check-development-policy")],
                                    text=True, capture_output=True)
            rows.extend([name + ": exit=" + str(result.returncode),
                         result.stdout.strip(), result.stderr.strip()])
            if result.returncode != expected:
                rows.append("UNEXPECTED RESULT: " + name)
                break
    output = "\n".join(row for row in rows if row) + "\n"
    Path(__file__).with_name("validation.log").write_text(output)
    print(output, end="")
    return int(any(row.startswith("UNEXPECTED RESULT:") for row in rows))


if __name__ == "__main__":
    sys.exit(main())
