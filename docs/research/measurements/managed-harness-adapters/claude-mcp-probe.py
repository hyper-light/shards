#!/usr/bin/env python3
"""One finite provider-free native SDK MCP initialize/catalog causal check.

Run under the assigned Mac lock. This is a fixture controller, never a product
launcher. The catalog is read from the actual canonical product TOOLS literal.
No user turn, tool effect, credential or provider request is sent.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import selectors
import shutil
import subprocess
import time


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--deadline-seconds", type=float, required=True)
    args = parser.parse_args()
    if not 0 < args.deadline_seconds <= 600:
        parser.error("deadline outside assigned finite envelope")
    out = args.out.resolve()
    out.mkdir(exist_ok=False)
    if shutil.disk_usage(out).free < 30 * 1024**3:
        raise RuntimeError("Mac30GiB floor unmet")
    state = out / "disposable-state"
    for directory in ["home", "claude", "tmp"]:
        (state / directory).mkdir(parents=True, exist_ok=True)
    binary = Path("/Users/adalundhe/.local/share/claude/versions/2.1.321")
    digest = hashlib.sha256()
    with binary.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    if digest.hexdigest() != "abc9feeeca8091d51b623291e044cf0528c2f6b74e0c1d6ef671d52c73f7cd3c":
        raise RuntimeError("pinned native CLI changed")
    source = Path("crates/server/src/server.rs").read_text()
    literals = re.findall(r'const TOOLS: &str = r#"(.*?)"#;', source, re.S)
    if len(literals) != 1:
        raise RuntimeError("canonical product catalog is not the inspected literal")
    catalog = [tool for tool in json.loads(literals[0])["tools"] if tool["name"] in ["send", "answer", "hand"]]
    if [tool["name"] for tool in catalog] != ["send", "answer", "hand"]:
        raise RuntimeError("canonical peer catalog differs")
    (out / "canonical-peer-catalog.json").write_text(json.dumps({"tools": catalog}, indent=2) + "\n")
    profile = '(version 1)(allow default)(deny network*)(deny mach-lookup (global-name "com.apple.SecurityServer"))(deny file-read* (subpath "/Users/adalundhe/Library/Keychains") (subpath "/Library/Keychains") (subpath "/Users/adalundhe/.claude") (literal "/Users/adalundhe/.claude.json") (subpath "/Users/adalundhe/.codex"))'
    (out / "sandbox.sb").write_text(profile + "\n")
    env = {"PATH": "/usr/bin:/bin", "HOME": str(state / "home"), "CLAUDE_CONFIG_DIR": str(state / "claude"), "TMPDIR": str(state / "tmp"), "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1", "DISABLE_AUTOUPDATER": "1", "CLAUDE_CODE_ENTRYPOINT": "sdk-py", "CLAUDE_AGENT_SDK_VERSION": "0.2.180"}
    argv = [str(binary), "--print", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose", "--permission-prompt-tool", "stdio", "--setting-sources", "", "--settings", "{}", "--strict-mcp-config", "--mcp-config", '{"mcpServers":{"shards":{"type":"sdk","name":"shards"}}}', "--tools", "", "--no-session-persistence"]
    before = len(os.listdir("/dev/fd"))
    started = time.monotonic()
    deadline = started + args.deadline_seconds
    child = subprocess.Popen(["/usr/bin/sandbox-exec", "-p", profile] + argv, cwd=state, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    selector = selectors.DefaultSelector()
    selector.register(child.stdout, selectors.EVENT_READ)
    selector.register(child.stderr, selectors.EVENT_READ)
    stdout, stderr, stdin, pending = bytearray(), bytearray(), bytearray(), bytearray()
    events = []
    initialized = None
    status = None
    status_requested = False
    got_catalog = False
    failure = None
    reaped = None

    def send(packet):
        data = (json.dumps(packet, separators=(",", ":")) + "\n").encode()
        stdin.extend(data)
        child.stdin.write(data)
        child.stdin.flush()

    try:
        send({"type": "control_request", "request_id": "shards-native-init", "request": {"subtype": "initialize", "hooks": None}})
        while time.monotonic() < deadline and selector.get_map() and not (initialized and got_catalog and status):
            for key, _ in selector.select(max(0, deadline - time.monotonic())):
                data = os.read(key.fd, 65536)
                if not data:
                    selector.unregister(key.fileobj)
                    continue
                if key.fileobj == child.stderr:
                    stderr.extend(data)
                    continue
                stdout.extend(data)
                pending.extend(data)
                while b"\n" in pending:
                    line, _, remaining = pending.partition(b"\n")
                    pending = bytearray(remaining)
                    packet = json.loads(line)
                    if packet.get("type") == "control_request":
                        request = packet["request"]
                        if request.get("subtype") != "mcp_message" or request.get("server_name") != "shards":
                            raise RuntimeError("unexpected native control request")
                        rpc = request["message"]
                        method = rpc.get("method")
                        events.append({"control": "mcp_message", "method": method, "id": rpc.get("id")})
                        if method == "initialize":
                            version = rpc.get("params", {}).get("protocolVersion")
                            if version not in ["2025-11-25", "2025-06-18", "2025-03-26"]:
                                raise RuntimeError("unverified native MCP revision")
                            result = {"protocolVersion": version, "capabilities": {"tools": {}}, "serverInfo": {"name": "shards", "version": "0.1.0"}}
                        elif method == "tools/list":
                            result = {"tools": catalog}
                            got_catalog = True
                        elif method == "notifications/initialized":
                            result = {}
                        else:
                            raise RuntimeError("unexpected native MCP method; no effect accepted")
                        reply = {"jsonrpc": "2.0", "result": result}
                        if "id" in rpc:
                            reply["id"] = rpc["id"]
                        send({"type": "control_response", "response": {"subtype": "success", "request_id": packet["request_id"], "response": {"mcp_response": reply}}})
                    elif packet.get("type") == "control_response":
                        response = packet["response"]
                        if response.get("subtype") != "success":
                            raise RuntimeError("native control response failed")
                        if response.get("request_id") == "shards-native-init":
                            initialized = response.get("response", {})
                            if initialized.get("claude_code_version") != "2.1.321" or initialized.get("account", {}).get("tokenSource") != "none":
                                raise RuntimeError("native version/no-account mismatch")
                        elif response.get("request_id") == "shards-native-status":
                            status = response.get("response", {})
                    if initialized and got_catalog and not status_requested:
                        send({"type": "control_request", "request_id": "shards-native-status", "request": {"subtype": "mcp_status"}})
                        status_requested = True
        if not (initialized and got_catalog and status):
            raise RuntimeError("native initialize/catalog/status causal sequence incomplete at original deadline")
    except BaseException as error:
        failure = type(error).__name__ + ": " + str(error)
    finally:
        child.stdin.close()
        child.stdin = None
        try:
            tail, errors = child.communicate(timeout=max(0, deadline - time.monotonic()))
        except subprocess.TimeoutExpired:
            child.kill()
            tail, errors = child.communicate()
            failure = failure or "cleanup required kill after original deadline"
        stdout.extend(tail)
        stderr.extend(errors)
        selector.close()
        try:
            os.waitpid(child.pid, os.WNOHANG)
            reaped = "unexpected waitable child"
        except ChildProcessError:
            reaped = "ECHILD"
        after = len(os.listdir("/dev/fd"))
        (out / "native.stdout").write_bytes(stdout)
        (out / "native.stderr").write_bytes(stderr)
        (out / "controller.stdin").write_bytes(stdin)
        receipt = {"argv": argv, "environment": env, "binary_sha256": digest.hexdigest(), "catalog_source_sha256": hashlib.sha256(literals[0].encode()).hexdigest(), "events": events, "initialized": initialized is not None, "catalogued": got_catalog, "mcp_status": status, "pid": child.pid, "exit": child.returncode, "reap": reaped, "fd_before": before, "fd_after": after, "elapsed_s": time.monotonic() - started, "failure": failure, "stdout_bytes": len(stdout), "stderr_bytes": len(stderr), "stdin_bytes": len(stdin)}
        (out / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
    if failure or child.returncode or before != after or reaped != "ECHILD":
        raise RuntimeError("native causal fixture failed; inspect retained receipt and exact streams")
    print(json.dumps({k: receipt[k] for k in ["initialized", "catalogued", "exit", "reap", "fd_before", "fd_after", "elapsed_s", "stdout_bytes", "stderr_bytes", "stdin_bytes"]}, indent=2))


if __name__ == "__main__":
    main()
