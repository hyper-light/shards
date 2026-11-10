#!/usr/bin/env python3
"""Archive retained textual receipts from explicitly owned, finished scopes."""
import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import tarfile


def sha(body):
    return hashlib.sha256(body).hexdigest()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--scope", type=Path, action="append", required=True)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    # Source is in the original verified input.json. Regenerable build output,
    # registries and retained executable copies stay untouched on the runner.
    excluded = {"target", "cargo-home", "source", "frozen", "tmp", "crates", ".cargo", "__pycache__"}
    payloads = []
    for index, scope in enumerate(args.scope):
        scope = scope.resolve(strict=True)
        for directory, children, files in os.walk(scope, followlinks=False):
            children[:] = sorted(name for name in children if name not in excluded)
            for name in sorted(files):
                path = Path(directory) / name
                if path.is_symlink() or name in {"frozen-api-test"}:
                    continue
                relative = path.relative_to(scope)
                if name not in {"stdout", "stderr"} and path.suffix not in {".json", ".stdout", ".stderr", ".py", ".txt", ".b64"}:
                    continue
                body = path.read_bytes()
                archive_name = str(Path("scope-" + str(index)) / relative)
                payloads.append((archive_name, body, {"path": archive_name, "original": str(path),
                                                     "bytes": len(body), "sha256": sha(body)}))
    manifest = {"scopes": [str(p.resolve()) for p in args.scope],
                "excluded_directory_names": sorted(excluded),
                "payloads": [row for _, _, row in payloads]}
    encoded = (json.dumps(manifest, indent=2) + "\n").encode()
    with args.out.open("xb") as output:
        with tarfile.open(fileobj=output, mode="w:gz") as archive:
            for name, body in [(name, body) for name, body, _ in payloads] + [("manifest.json", encoded)]:
                info = tarfile.TarInfo(name)
                info.size = len(body)
                info.mode = 0o600
                archive.addfile(info, io.BytesIO(body))
    receipt = {"archive": str(args.out), "archive_bytes": args.out.stat().st_size,
               "archive_sha256": sha(args.out.read_bytes()), "payloads": len(payloads),
               "manifest_sha256": sha(encoded)}
    with args.out.with_suffix(".receipt.json").open("x") as output:
        json.dump(receipt, output, indent=2)
        output.write("\n")
    print(json.dumps(receipt))


if __name__ == "__main__":
    main()
