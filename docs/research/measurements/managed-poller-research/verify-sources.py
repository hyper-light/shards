#!/usr/bin/env python3
"""Light documentation identity check; never executes the researched Rust paths.

Run from this checkout. Fetch upstream bytes in memory, retaining identities only.
Matching bytes/URLs does not establish the meaning of a citation or a runtime gate.
"""

import concurrent.futures
import datetime
import hashlib
import io
import json
from pathlib import Path
import re
import subprocess
import sys
import tarfile
import urllib.request


def sha(data):
    return hashlib.sha256(data).hexdigest()


def git(*args):
    return subprocess.check_output(["git", *args])


def fetch(record):
    result = {"id": record["id"], "url": record["url"]}
    try:
        request = urllib.request.Request(record["url"], headers={"User-Agent": "Shards-source-identity"})
        with urllib.request.urlopen(request, timeout=30) as response:
            data = response.read()
            result.update(status=response.status, resolved_url=response.geturl())
        result.update(bytes=len(data), sha256=sha(data))
        result["matches_manifest"] = len(data) == record["bytes"] and sha(data) == record["sha256"]
        if record.get("members"):
            result["members"] = []
            with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as archive:
                for member in record["members"]:
                    stream = archive.extractfile(member["path"])
                    if stream is None:
                        raise ValueError("missing archive member " + member["path"])
                    content = stream.read()
                    match = len(content) == member["bytes"] and sha(content) == member["sha256"]
                    result["members"].append({"path": member["path"], "bytes": len(content),
                                              "sha256": sha(content), "matches_manifest": match})
                    result["matches_manifest"] &= match
    except Exception as error:
        result.update(matches_manifest=False, error=str(error))
    return result


def main():
    root = Path(git("rev-parse", "--show-toplevel").decode().strip())
    lane = Path(__file__).resolve().parent
    manifest_bytes = (lane / "source-identity.json").read_bytes()
    manifest = json.loads(manifest_bytes)
    base = manifest["base"]
    result = {
        "kind": "Light documentation/source identity checks only; no runtime/performance work",
        "checked_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "source_base": base,
        "head": git("rev-parse", "HEAD").decode().strip(),
        "manifest_sha256": sha(manifest_bytes),
        "checker_sha256": sha(Path(__file__).read_bytes()),
        "local_git_blobs": [],
        "external_citations": [],
        "relative_links": [],
        "limits": ["Byte identity is not semantic citation review or runtime validation.",
                   "Mutable manuals are compared with the retained historical snapshot hash.",
                   "No upstream source/archive is extracted or retained on disk.",
                   "All candidate cases, reproductions, mutations and costs remain unrun here."],
    }
    for record in manifest["local_git_blobs"]:
        path = record["path"]
        data = git("show", base + ":" + path)
        current = git("show", "HEAD:" + path)
        blob = git("rev-parse", base + ":" + path).decode().strip()
        present = (root / path).is_file()
        working_match = (root / path).read_bytes() == data if present else None
        result["local_git_blobs"].append({
            "path": path, "git_blob": blob, "bytes": len(data), "sha256": sha(data),
            "matches_manifest": blob == record["git_blob"] and len(data) == record["bytes"]
                                and sha(data) == record["sha256"],
            "head_matches_base": current == data,
            "working_file_matches_base": working_match,
            "checkout": "materialized" if present else "sparse; checked Git blob",
        })
    sources = manifest["sources"]
    projects = {p["id"]: p for p in manifest["projects"]}
    result["pinned_source_identities"] = []
    for record in sources:
        if "revision" not in record:
            continue
        project = projects[record["project"]]
        expected = project["repository"].replace("https://github.com/", "https://raw.githubusercontent.com/")
        expected += "/" + project["commit"] + "/" + record["path"]
        result["pinned_source_identities"].append({"id": record["id"], "matches_manifest":
            record["revision"] == project["commit"] and record["url"] == expected})
    known = {record["url"] for record in sources} | {manifest["libev"]["url"]}
    known.update("https://raw.githubusercontent.com/hyper-light/shards/" + base + "/" + r["path"]
                 for r in manifest["local_git_blobs"])
    note_path = root / "docs/research/managed-poller-capacity.md"
    note = note_path.read_text()
    for url in sorted(set(re.findall(r"\]\((https?://[^)]+)\)", note))):
        canonical = re.sub(r"https://github.com/([^/]+/[^/]+)/blob/", r"https://raw.githubusercontent.com/\1/", url.split("#", 1)[0])
        result["external_citations"].append({"url": url, "matches_manifest": canonical in known})
    for document in [note_path, lane / "README.md", lane / "STEP2.md"]:
        for target in re.findall(r"\]\(([^)]+)\)", document.read_text()):
            if re.match(r"https?://|#", target):
                continue
            resolved = (document.parent / target.split("#", 1)[0]).resolve()
            available = resolved.exists()
            if not available:
                relative = str(resolved.relative_to(root))
                available = subprocess.run(["git", "cat-file", "-e", "HEAD:" + relative],
                                           capture_output=True).returncode == 0
            result["relative_links"].append({"document": str(document.relative_to(root)),
                                             "target": target, "available": available})
    # I/O concurrency only, within this small, fixed citation list; no agent/runner jobs.
    with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
        result["upstream_bytes"] = list(pool.map(fetch, [*sources, manifest["libev"]]))
    result["passed"] = (
        all(r["matches_manifest"] and r["head_matches_base"]
            and r["working_file_matches_base"] is not False for r in result["local_git_blobs"])
        and all(r["matches_manifest"] for r in result["pinned_source_identities"])
        and all(r["matches_manifest"] for r in result["external_citations"])
        and all(r["available"] for r in result["relative_links"])
        and all(r["matches_manifest"] for r in result["upstream_bytes"])
    )
    print(json.dumps(result, indent=2))
    return 0 if result["passed"] else 1


if __name__ == "__main__":
    sys.exit(main())
