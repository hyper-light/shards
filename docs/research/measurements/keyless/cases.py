"""Inside shards-dind, after setup.sh: keyless signing held to cosign v3.1.3 against the
local Sigstore (D126, PM M184). For each signing config (Rekor v2 with the timestamp
authority, Rekor v1 with it, Rekor v1 alone): an agent made and pushed by shards; signed
keylessly by shards with a token of the fake issuer, and pushed; verified by `cosign
verify` with that identity (and refused with another); signed keylessly by cosign; each
signature's bundle read back from the registry and kept, with the trusted root, for
crates/sigstore/tests/keyless.rs. Then `shards sign` and `cosign sign` timed, alternated.

    python3 -I cases.py WORK SHARDS COSIGN OUT [N]
"""
import json
import os
import re
import shutil
import statistics
import subprocess
import sys
import time
import urllib.request

work, shards_bin, cosign_bin, out_dir = sys.argv[1:5]
n_timed = int(sys.argv[5]) if len(sys.argv) > 5 else 10
REGISTRY = "localhost:5000"
IDENTITY = "foo@bar.com"
ISSUER = "http://fakeoidc:8080"

env_file = dict(l.strip().split("=", 1) for l in open(f"{work}/stack.env") if "=" in l)
root_path = env_file["TRUSTED_ROOT"]
base_config = json.load(open(env_file["SIGNING_CONFIG"]))


def token():
    """A fresh token of the fake issuer (each lives 30 minutes)."""
    return urllib.request.urlopen(f"{env_file['OIDC_URL']}/token", timeout=10).read().decode()


def run(cmd, env=None, check=True):
    t0 = time.perf_counter()
    p = subprocess.run(cmd, env=env, capture_output=True, text=True)
    took = time.perf_counter() - t0
    if check and p.returncode != 0:
        raise SystemExit(f"{cmd[:4]}: exit {p.returncode}\n{p.stdout}\n{p.stderr}")
    return p, took


def config_with(logs, tsa):
    c = dict(base_config)
    c["rekorTlogUrls"] = [s for s in base_config["rekorTlogUrls"] if s["majorApiVersion"] in logs]
    if not tsa:
        c["tsaUrls"] = []
    path = f"{work}/signing-{'-'.join(map(str, logs))}{'-tsa' if tsa else ''}.json"
    with open(path, "w") as f:
        json.dump(c, f)
    return path


def registry_json(path, accept):
    req = urllib.request.Request(f"http://{REGISTRY}{path}", headers={"Accept": accept})
    return urllib.request.urlopen(req, timeout=10).read()


def bundles(repo, digest):
    """Each referrer the tag schema lists for `digest`, by its manifest's digest, and its
    bundle."""
    index = json.loads(registry_json(f"/v2/{repo}/manifests/{digest.replace(':', '-')}",
                                     "application/vnd.oci.image.index.v1+json"))
    got = {}
    for m in index.get("manifests", []):
        man = json.loads(registry_json(f"/v2/{repo}/manifests/{m['digest']}", m["mediaType"]))
        for layer in man.get("layers", []):
            got[m["digest"]] = registry_json(f"/v2/{repo}/blobs/{layer['digest']}", layer["mediaType"]).decode()
    return got


def shape(bundle):
    """What a bundle holds, without what differs signature to signature."""
    b = json.loads(bundle)
    vm = b["verificationMaterial"]
    return {
        "mediaType": b["mediaType"],
        "material": sorted(k for k in vm if k not in ("tlogEntries", "timestampVerificationData")),
        "tlog": [{"kindVersion": e.get("kindVersion"), "fields": sorted(e)} for e in vm.get("tlogEntries", [])],
        "timestamps": len(vm.get("timestampVerificationData", {}).get("rfc3161Timestamps", [])),
        "content": sorted(k for k in b if k not in ("mediaType", "verificationMaterial")),
    }


def main():
    shutil.rmtree(out_dir, ignore_errors=True)
    os.makedirs(out_dir)
    shutil.copy(root_path, f"{out_dir}/trusted_root.json")
    home = f"{work}/home"
    shutil.rmtree(home, ignore_errors=True)
    env = dict(os.environ, SHARDS_HOME=home)
    agent = f"{work}/agent"
    os.makedirs(agent, exist_ok=True)
    with open(f"{agent}/run.sh", "w") as f:
        f.write("#!/bin/sh\necho agent\n")
    with open(f"{agent}/agent.json", "w") as f:
        f.write('{"name":"main","version":"1.0.0"}')
    shards = lambda *a, e=env: run([shards_bin, *a], e)
    results = {"cases": {}}
    recorded = []
    for label, logs, tsa in (("rekor v2, timestamp", [2], True), ("rekor v1, timestamp", [1], True),
                             ("rekor v1", [1], False)):
        config = config_with(logs, tsa)
        repo = f"keyless/{'-'.join(map(str, logs))}{'-tsa' if tsa else ''}"
        name = f"{REGISTRY}/{repo}:1"
        made, _ = shards("build", "agent", agent, "-t", name)
        digest = made.stdout.strip()
        shards("push", "agent", name)
        signer_env = dict(env, SIGSTORE_ID_TOKEN=token())
        signed, took = shards("sign", "agent", name, "--signing-config", config, "--trusted-root", root_path,
                              "--yes", e=signer_env)
        ours = signed.stdout.strip().rsplit(" ", 1)[-1]
        shards("push", "agent", name)
        ref = f"{REGISTRY}/{repo}@{digest}"
        verify = lambda identity: run([cosign_bin, "verify", "--trusted-root", root_path, "--certificate-identity",
                                       identity, "--certificate-oidc-issuer", ISSUER, "--allow-http-registry",
                                       ref], env, check=False)[0]
        v = verify(IDENTITY)
        wrong = verify("someone@else.example")
        run([cosign_bin, "sign", "--signing-config", config, "--trusted-root", root_path, "--identity-token",
             token(), "--allow-http-registry", "--yes", ref], env)
        found = bundles(repo, digest)
        ours_bundle = [b for d, b in found.items() if d == ours]
        theirs = [b for d, b in found.items() if d != ours]
        results["cases"][label] = {
            "shards sign": {"said": signed.stdout.strip(), "stderr": signed.stderr.strip().splitlines()[-2:],
                            "ms": round(took * 1e3)},
            "cosign verify of shards' signature": {"exit": v.returncode, "checks": [
                l.strip() for l in v.stderr.splitlines() if l.strip().startswith("-")]},
            "cosign verify, another identity": {"exit": wrong.returncode,
                                                "said": wrong.stderr.strip().splitlines()[-1:]},
            "shards' bundle": shape(ours_bundle[0]) if ours_bundle else None,
            "cosign's bundle": shape(theirs[0]) if theirs else None,
        }
        for who, b in (("shards", ours_bundle), ("cosign", theirs)):
            if b:
                file = f"{repo.split('/')[1]}-{who}.json"
                with open(f"{out_dir}/{file}", "w") as f:
                    f.write(b[0])
                recorded.append({"bundle": file, "signer": who, "digest": digest, "logs": logs, "tsa": tsa,
                                 "identity": IDENTITY, "issuer": ISSUER})
    # Signing in (D126): at Dex, the provider Fulcio's own compose runs (as
    # oauth2.sigstore.dev does), in a browser (the harness's xdg-open) and by a device code
    # (typed in by device-login), each identity then verified by cosign.
    dex = "http://dex-idp:8888/auth"
    dex_identity = "kilgore@kilgore.trout"
    sc = json.load(open(config_with([1], True)))
    sc["oidcUrls"] = [{"url": dex, "majorApiVersion": 1, "validFor": {"start": "2025-05-25T00:00:00Z"},
                       "operator": "dex"}]
    dex_config = f"{work}/signing-dex.json"
    with open(dex_config, "w") as f:
        json.dump(sc, f)
    flags = ["--signing-config", dex_config, "--trusted-root", root_path, "--oidc-client-id", "fulcio",
             "--oidc-disable-ambient-providers", "--yes"]
    for label, flow in (("signed in, in a browser", "normal"), ("signed in, by a device code", "device")):
        repo = f"keyless/{flow}"
        name = f"{REGISTRY}/{repo}:1"
        made, _ = shards("build", "agent", agent, "-t", name)
        digest = made.stdout.strip()
        shards("push", "agent", name)
        argv = [shards_bin, "sign", "agent", name, "--fulcio-auth-flow", flow, *flags]
        t0 = time.perf_counter()
        if flow == "normal":
            browser_env = dict(env, PATH=f"{work}/browser:{os.environ['PATH']}")
            p = subprocess.run(argv, env=browser_env, capture_output=True, text=True, timeout=180)
            out, err, code = p.stdout, p.stderr, p.returncode
        else:
            p = subprocess.Popen(argv, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            said = []
            for line in p.stderr:
                said.append(line)
                m = re.search(r"Enter the verification code (\S+) in your browser at: (\S+)", line)
                if m:
                    subprocess.run([f"{work}/browser/device-login", m.group(1)], check=True, timeout=60)
                    break
            out, rest = p.communicate(timeout=180)
            err, code = "".join(said) + rest, p.returncode
        took = time.perf_counter() - t0
        if code != 0:
            raise SystemExit(f"{label}: exit {code}\n{out}\n{err}")
        ours = out.strip().rsplit(" ", 1)[-1]
        shards("push", "agent", name)
        ref = f"{REGISTRY}/{repo}@{digest}"
        v = run([cosign_bin, "verify", "--trusted-root", root_path, "--certificate-identity", dex_identity,
                 "--certificate-oidc-issuer", dex, "--allow-http-registry", ref], env, check=False)[0]
        found = bundles(repo, digest)
        results["cases"][label] = {
            "shards sign": {"said": out.strip(), "stderr": [l for l in err.splitlines() if l.strip()][:6],
                            "ms": round(took * 1e3)},
            "cosign verify of shards' signature": {"exit": v.returncode, "checks": [
                l.strip() for l in v.stderr.splitlines() if l.strip().startswith("-")]},
            "shards' bundle": shape(found[ours]) if ours in found else None,
        }
        if ours in found:
            file = f"{flow}-shards.json"
            with open(f"{out_dir}/{file}", "w") as f:
                f.write(found[ours])
            recorded.append({"bundle": file, "signer": "shards", "digest": digest, "logs": [1], "tsa": True,
                             "identity": dex_identity, "issuer": dex})
    with open(f"{out_dir}/cases.json", "w") as f:
        json.dump(recorded, f, indent=1)
    # Signing timed: each side signs one artifact, alternated (Rekor v1 and the
    # timestamp authority, as the public instance's config asks).
    config = config_with([1], True)
    name = f"{REGISTRY}/keyless/timed:1"
    made, _ = shards("build", "agent", agent, "-t", name)
    digest = made.stdout.strip()
    shards("push", "agent", name)
    ref = f"{REGISTRY}/keyless/timed@{digest}"
    ours, theirs = [], []
    for _ in range(n_timed):
        e = dict(env, SIGSTORE_ID_TOKEN=token())
        ours.append(run([shards_bin, "sign", "agent", name, "--signing-config", config, "--trusted-root",
                         root_path, "--yes"], e)[1])
        theirs.append(run([cosign_bin, "sign", "--signing-config", config, "--trusted-root", root_path,
                           "--identity-token", token(), "--allow-http-registry", "--yes", ref], env)[1])
    pct = lambda xs, p: sorted(xs)[min(len(xs) - 1, int(p * (len(xs) - 1) + 0.5))]
    q = lambda xs: {"n": len(xs), "p50_ms": round(statistics.median(xs) * 1e3),
                    "p90_ms": round(pct(xs, 0.9) * 1e3), "p99_ms": round(pct(xs, 0.99) * 1e3),
                    "max_ms": round(max(xs) * 1e3)}
    results["shards sign (keyless, stored)"] = q(ours)
    results["cosign sign (keyless, pushed)"] = q(theirs)
    results["versions"] = {"cosign": run([cosign_bin, "version"], env, check=False)[0].stdout.split("\n")[3:5]}
    print(json.dumps(results, indent=1))


if __name__ == "__main__":
    main()
