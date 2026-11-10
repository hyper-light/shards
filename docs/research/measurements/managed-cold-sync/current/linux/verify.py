#!/usr/bin/env python3
"""Verify every archived regular member without extracting or rewriting it."""
import base64
import hashlib
import io
import json
from pathlib import Path, PurePosixPath
import tarfile


def sha(body):
    return hashlib.sha256(body).hexdigest()


def main():
    here = Path(__file__).resolve().parent
    records = json.loads((here / "archives.json").read_text())
    for label, record in records["archives"].items():
        body = base64.b64decode("".join((here / record["local_file"]).read_text().split()), validate=True)
        assert len(body) == record["archive_bytes"]
        assert sha(body) == record["archive_sha256"]
        with tarfile.open(fileobj=io.BytesIO(body), mode="r:gz") as archive:
            members = archive.getmembers()
            names = [m.name for m in members]
            assert len(names) == len(set(names))
            for member in members:
                path = PurePosixPath(member.name)
                assert member.isreg() and not path.is_absolute() and ".." not in path.parts
            manifest_bytes = archive.extractfile("manifest.json").read()
            assert sha(manifest_bytes) == record["manifest_sha256"]
            manifest = json.loads(manifest_bytes)
            assert len(manifest["payloads"]) == record["payloads"]
            assert set(names) == {p["path"] for p in manifest["payloads"]} | {"manifest.json"}
            for payload in manifest["payloads"]:
                data = archive.extractfile(payload["path"]).read()
                assert len(data) == payload["bytes"] and sha(data) == payload["sha256"]
                if payload["path"].endswith("/input.json"):
                    bundle = json.loads(data)
                    versions = bundle.get("versions", [bundle.get("version")])
                    for version in versions:
                        for value in version["files"].values():
                            assert sha(value["text"].encode()) == value["sha256"]
        print(json.dumps({"archive": label, "sha256": record["archive_sha256"],
                          "payloads_verified": record["payloads"], "all_safe_unique_regular": True}))


if __name__ == "__main__":
    main()
