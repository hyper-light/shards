#!/usr/bin/env python3
"""GPU metadata only, no VM/device queues/commands. Does not install tools/drivers."""
import hashlib
import json
import os
import platform
import re
import shutil
import subprocess
from pathlib import Path

root = Path(__file__).resolve().parents[4]
here = Path(__file__).resolve().parent
result = {"platform": platform.platform(), "arch": platform.machine(),
          "source_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
          "scope": "read-only device/API enumeration, no VM, performance or parity claim"}


def run(command, **kwargs):
    p = subprocess.run(command, capture_output=True, text=True, timeout=45, check=False, **kwargs)
    return {"command": command, "exit_code": p.returncode, "stdout": p.stdout, "stderr": p.stderr}


def hashed(path):
    path = Path(path)
    return {"path": str(path), "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}


if platform.system() == "Darwin":
    result["os_build"] = run(["sw_vers", "-buildVersion"])
    source = here / "metal-readonly.m"
    result["compiler_sdk_trace"] = run(["cc", "-###", "-fobjc-arc", "-c", str(source)])
    chosen = re.findall(r'"-isysroot" "([^"]+)"', result["compiler_sdk_trace"]["stderr"])
    if chosen:
        sdk = Path(chosen[0])
        result["sdk_metal_header"] = hashed(sdk / "System/Library/Frameworks/Metal.framework/Headers/MTLDevice.h")
        settings = sdk / "SDKSettings.json"
        if settings.exists():
            info = json.loads(settings.read_text())
            result["sdk_version"] = {k: info.get(k) for k in ["Version", "CanonicalName"]}
    else:
        result["sdk_provenance"] = "compiler did not name an isysroot; not inferred"
    result["probe_source"] = hashed(source)
    binary = root / "target/managed-virtio-gpu-light/metal-readonly"
    result["compiler"] = run(["cc", "--version"])
    result["compile"] = run(["cc", "-fobjc-arc", "-Wall", "-Wextra", "-Werror", str(source),
                             "-framework", "Metal", "-framework", "Foundation", "-o", str(binary)])
    if result["compile"]["exit_code"] == 0:
        result["metal_enumeration"] = run([str(binary)])
        result["probe_binary"] = hashed(binary)
    result["driver_identity"] = "Metal driver ships with the recorded macOS build; no Vulkan/MoltenVK binary was probed"
else:
    if platform.system() == "Linux":
        result["libc"] = platform.libc_ver()
        result["drm_devices"] = []
        for node in sorted(Path("/sys/class/drm").glob("renderD*")):
            device = node / "device"
            item = {"node": str(node), "driver": str((device / "driver").resolve())}
            for name in ["vendor", "device", "revision"]:
                path = device / name
                if path.exists():
                    item[name] = path.read_text().strip()
            result["drm_devices"].append(item)
    found = shutil.which("vulkaninfo") or shutil.which("vulkaninfo.exe")
    if found:
        result["tool"] = hashed(found)
        env = dict(os.environ)
        env["MESA_SHADER_CACHE_DISABLE"] = "true"
        result["vulkan_enumeration"] = run([found, "--summary"], env=env)
    else:
        result["vulkan_enumeration"] = {"outcome": "unavailable: vulkaninfo is not installed; no installation attempted"}
print(json.dumps(result, indent=2))
