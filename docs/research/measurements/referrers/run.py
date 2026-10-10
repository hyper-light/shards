"""PM M135: OSI artifacts' signatures as OCI 1.1 referrers (D116), shards held to cosign
v3.1.3 on two registries run on this host: distribution v3.1.2, which has no referrers
API, and zot v2.1.22, which has it.

For each registry: an agent made and pushed by shards; signed by shards with a key cosign
made, and pushed; that signature verified by `cosign verify --key`; signed again by cosign;
both pulled by shards; cosign's deleted by shards (cosign still verifies, shards'
remaining); shards' deleted (cosign then finds none). The registry's requests for a pushed
signature are kept from its access log. Then `shards sign` timed, n times, against cosign
signing the same artifact.

    python3 -I run.py --shards PATH --cosign PATH --registry PATH --zot PATH [--n 20]

Nothing leaves the host: the registries listen on loopback, cosign signs with an empty
signing config (no Rekor, Fulcio or TSA), and every process started is stopped.
"""
import argparse
import json
import os
import shutil
import statistics
import subprocess
import tempfile
import time
import urllib.request

PASSWORD = "m130 measurement key"
EMPTY_SIGNING_CONFIG = {
    "mediaType": "application/vnd.dev.sigstore.signingconfig.v0.2+json",
    "caUrls": [],
    "oidcUrls": [],
    "rekorTlogUrls": [],
    "rekorTlogConfig": {"selector": "ANY"},
    "tsaUrls": [],
    "tsaConfig": {"selector": "ANY"},
}


def run(cmd, env=None, check=True):
    t0 = time.perf_counter()
    p = subprocess.run(cmd, env=env, capture_output=True, text=True)
    took = time.perf_counter() - t0
    if check and p.returncode != 0:
        raise SystemExit(f"{cmd}: exit {p.returncode}\n{p.stdout}\n{p.stderr}")
    return p, took


def wait_up(port):
    for _ in range(100):
        try:
            urllib.request.urlopen(f"http://127.0.0.1:{port}/v2/", timeout=1)
            return
        except Exception:
            time.sleep(0.1)
    raise SystemExit(f"registry on {port} never answered")


def quantiles(xs):
    xs = sorted(xs)
    q = lambda p: xs[min(len(xs) - 1, int(round(p * (len(xs) - 1))))]
    return {"n": len(xs), "p50_ms": q(0.5) * 1e3, "p90_ms": q(0.9) * 1e3, "p99_ms": q(0.99) * 1e3,
            "max_ms": xs[-1] * 1e3, "mean_ms": statistics.fmean(xs) * 1e3}


def main():
    a = argparse.ArgumentParser()
    a.add_argument("--shards", required=True)
    a.add_argument("--cosign", required=True)
    a.add_argument("--registry", required=True)
    a.add_argument("--zot", required=True)
    a.add_argument("--n", type=int, default=20)
    args = a.parse_args()
    work = tempfile.mkdtemp(prefix="m130-")
    procs = []
    out = {"registries": {}}
    try:
        # The registries, each storing under the work directory.
        os.makedirs(f"{work}/dist")
        with open(f"{work}/dist.yml", "w") as f:
            f.write(f"version: 0.1\nlog:\n  level: info\nstorage:\n  filesystem:\n    rootdirectory: {work}/dist\n"
                    "  delete:\n    enabled: true\nhttp:\n  addr: 127.0.0.1:15600\n")
        with open(f"{work}/zot.json", "w") as f:
            json.dump({"distSpecVersion": "1.1.1", "storage": {"rootDirectory": f"{work}/zot", "gc": False},
                       "http": {"address": "127.0.0.1", "port": "15601"}, "log": {"level": "warn"}}, f)
        dist_log = open(f"{work}/dist.log", "w")
        procs.append(subprocess.Popen([args.registry, "serve", f"{work}/dist.yml"], stdout=dist_log, stderr=dist_log))
        procs.append(subprocess.Popen([args.zot, "serve", f"{work}/zot.json"], stdout=subprocess.DEVNULL,
                                      stderr=subprocess.DEVNULL))
        wait_up(15600)
        wait_up(15601)
        # A key cosign makes, and a signing config naming no service.
        os.makedirs(f"{work}/keys")
        env = dict(os.environ, COSIGN_PASSWORD=PASSWORD, SHARDS_HOME=f"{work}/home")
        subprocess.run([args.cosign, "generate-key-pair"], cwd=f"{work}/keys", env=env, check=True,
                       capture_output=True)
        with open(f"{work}/keys/signing-config.json", "w") as f:
            json.dump(EMPTY_SIGNING_CONFIG, f)
        key, pub = f"{work}/keys/cosign.key", f"{work}/keys/cosign.pub"
        os.makedirs(f"{work}/agent")
        with open(f"{work}/agent/run.sh", "w") as f:
            f.write("#!/bin/sh\necho agent\n")
        with open(f"{work}/agent/agent.json", "w") as f:
            f.write('{"name":"main","version":"1.0.0"}')
        shards = lambda *a: run([args.shards, *a], env)
        cosign_verify = lambda ref: run([args.cosign, "verify", "--key", pub, "--insecure-ignore-tlog=true",
                                         "--allow-http-registry", ref], env, check=False)[0]
        cosign_sign = lambda ref: run([args.cosign, "sign", "--key", key, "--signing-config",
                                       f"{work}/keys/signing-config.json", "--allow-http-registry", "--yes", ref], env)
        for label, port in (("distribution v3.1.2", 15600), ("zot v2.1.22", 15601)):
            r = {}
            name = f"127.0.0.1:{port}/m130/agent:1"
            made, _ = shards("build", "agent", f"{work}/agent", "-t", name)
            digest = made.stdout.strip()
            shards("push", "agent", name)
            mark = os.path.getsize(f"{work}/dist.log")
            signed, _ = shards("sign", "agent", name, "--key", key)
            ours = signed.stdout.strip().rsplit(" ", 1)[-1]
            pushed, _ = shards("push", "agent", name)
            r["shards push"] = [l for l in pushed.stdout.splitlines() if "signature" in l]
            if port == 15600:
                with open(f"{work}/dist.log") as f:
                    f.seek(mark)
                    r["requests of the signature's push"] = [
                        l.split('"')[1] for l in f if '"' in l and "/v2/" in l and "/v2/ " not in l]
            ref = f"{name.rsplit(':', 1)[0]}@{digest}"
            v = cosign_verify(ref)
            r["cosign verify of shards' signature"] = {"exit": v.returncode, "checks": [
                l.strip() for l in v.stderr.splitlines() if l.strip().startswith("-")]}
            cosign_sign(ref)
            shards("rm", "agent", name)
            pulled, _ = shards("pull", "agent", name)
            sigs = [l.rsplit(" ", 1)[-1] for l in pulled.stdout.splitlines() if ": signature " in l]
            r["signatures shards pulled"] = len(sigs)
            theirs = [s for s in sigs if s != ours]
            if len(theirs) != 1 or ours not in sigs:
                raise SystemExit(f"{label}: pulled {sigs}, ours {ours}")
            shards("rm", "agent", name, "--referrer", theirs[0])
            r["cosign verify, cosign's deleted by shards"] = cosign_verify(ref).returncode
            shards("rm", "agent", name, "--referrer", ours)
            last = cosign_verify(ref)
            r["cosign verify, both deleted"] = {"exit": last.returncode, "said": last.stderr.strip().splitlines()[-1:]}
            out["registries"][label] = r
        # `shards sign` timed, and cosign signing the same artifact, alternated.
        name = "127.0.0.1:15601/m130/timed:1"
        made, _ = shards("build", "agent", f"{work}/agent", "-t", name)
        digest = made.stdout.strip()
        shards("push", "agent", name)
        ref = f"127.0.0.1:15601/m130/timed@{digest}"
        ours, theirs = [], []
        for _ in range(args.n):
            ours.append(run([args.shards, "sign", "agent", name, "--key", key], env)[1])
            theirs.append(cosign_sign(ref)[1])
        out["shards sign (scrypt N=2^16 included)"] = quantiles(ours)
        out["cosign sign, signing and pushing"] = quantiles(theirs)
        out["host"] = {"uname": " ".join(os.uname()), "cpus": os.cpu_count()}
        out["versions"] = {"shards": run([args.shards, "version"], env, check=False)[0].stdout.strip()[:200],
                           "cosign": "v3.1.3"}
    finally:
        for p in procs:
            p.terminate()
        for p in procs:
            p.wait(timeout=10)
        shutil.rmtree(work, ignore_errors=True)
    print(json.dumps(out, indent=1))


if __name__ == "__main__":
    main()
