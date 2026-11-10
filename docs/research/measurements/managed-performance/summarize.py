#!/usr/bin/env python3
"""Summarize retained native TSV samples without turning observations into policy."""
import collections
import json
import math
import pathlib
import re
import sys


def distribution(values):
    values = sorted(values)
    if not values:
        return {"n": 0}
    return {"n": len(values), **{
        name: values[math.ceil(len(values) * percentile / 100) - 1]
        for name, percentile in [("p50", 50), ("p90", 90), ("p99", 99), ("max", 100)]
    }}


def fields(line):
    return {key: int(value) if re.fullmatch(r"-?\d+", value) else value
            for key, value in (item.split("=", 1) for item in line.split("\t") if "=" in item)}


def load(path):
    lines = path.read_text().splitlines()
    if "result\tpass" not in lines:
        raise ValueError(f"missing passing result: {path}")
    phases = {line.split("\t")[1]: fields(line) for line in lines if line.startswith("phase\t")}
    samples = [fields(line) for line in lines if line.startswith("sample\t")]
    config = fields(next(line.split("config\t", 1)[1] for line in lines if "config\t" in line))
    expected = config["records"] * (config["channels"] - (config["mode"] == "stall"))
    if len(samples) != expected:
        raise ValueError(f"sample count {len(samples)} != {expected}: {path}")
    resources = collections.defaultdict(dict)
    for line in lines:
        if not line.startswith("resource\t"):
            continue
        _, phase, source, value = line.split("\t", 3)
        if source in ["fds", "tasks"]:
            resources[phase][source] = int(value)
        elif source == "/proc/self/smaps_rollup":
            label, _, data = value.partition(":")
            if label in ["Rss", "Pss", "Private_Clean", "Private_Dirty"]:
                resources[phase][label + "_KiB"] = int(data.split()[0])
    cancel = [fields(line) for line in lines if line.startswith("cancel\t")]
    return {"file": path.name, "config": config, "phases": phases, "samples": samples,
            "resources": dict(resources), "cancel": cancel}


def main():
    directory = pathlib.Path(sys.argv[1])
    groups = collections.defaultdict(list)
    for path in sorted(directory.glob("r*-t*-*.tsv")):
        run = load(path)
        config = run["config"]
        groups[(config["tenants"], config["mode"], config["variant"])].append(run)
    if not groups:
        raise ValueError("no retained sample files")
    result = {"method": "nearest-rank empirical percentiles; pooled records retain process/run identity in raw files; n=3 process tails are not stable estimates", "groups": []}
    for (tenants, mode, variant), runs in sorted(groups.items()):
        group = {"tenants": tenants, "mode": mode, "variant": variant,
                 "processes": len(runs), "files": [run["file"] for run in runs]}
        for key in ["receive_ns", "app_ack_ns"]:
            group[key] = distribution([sample[key] for run in runs for sample in run["samples"]])
        group["cancel_waiter_ns"] = distribution([entry["waiter_ns"] for run in runs for entry in run["cancel"]])
        group["phases"] = {}
        for phase in ["servers_bound", "tls_subscribed", "workload", "closed"]:
            group["phases"][phase] = {
                key: distribution([run["phases"][phase][key] for run in runs])
                for key in ["wall_ns", "alloc", "realloc", "free", "allocated_bytes", "live_bytes", "live_delta", "peak_bytes", "minor_faults", "major_faults", "user_us", "system_us"]
            }
        group["resources"] = {
            phase: {key: distribution([run["resources"][phase][key] for run in runs])
                    for key in ["fds", "tasks", "Rss_KiB", "Pss_KiB", "Private_Clean_KiB", "Private_Dirty_KiB"]}
            for phase in ["initial", "servers_bound", "tls_subscribed", "workload", "closed"]
        }
        result["groups"].append(group)
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
