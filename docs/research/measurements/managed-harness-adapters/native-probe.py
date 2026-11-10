#!/usr/bin/env python3
"""Finite, provider-free initialization against the two pinned native binaries.

Run under lockf /tmp/g121-mac.lock. Output directory must be new; all raw streams,
inputs and failures are retained there. No user turn/auth request is sent.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
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
        parser.error("deadline must fit the assigned finite600s envelope")
    out = args.out.resolve()
    out.mkdir(exist_ok=False)
    if shutil.disk_usage(out).free < 30 * 1024**3:
        raise RuntimeError("Mac30GiB floor unmet")
    state = out / "disposable-state"
    for directory in ["home", "codex", "claude", "tmp"]:
        (state / directory).mkdir(parents=True, exist_ok=True)
    env = {
        "PATH": "/usr/bin:/bin",
        "HOME": str(state / "home"),
        "CODEX_HOME": str(state / "codex"),
        "CLAUDE_CONFIG_DIR": str(state / "claude"),
        "TMPDIR": str(state / "tmp"),
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
        "DISABLE_AUTOUPDATER": "1",
    }
    codex = Path("/Users/adalundhe/.nvm/versions/node/v24.14.1/lib/node_modules/@openai/codex/node_modules/@openai/codex-darwin-arm64/vendor/aarch64-apple-darwin/bin/codex")
    claude = Path("/Users/adalundhe/.local/share/claude/versions/2.1.321")
    identities = {
        codex: "030316ecfbb41f4e7946e584b0063ef333b7a36cd312ba349f2f6869e39b62cc",
        claude: "abc9feeeca8091d51b623291e044cf0528c2f6b74e0c1d6ef671d52c73f7cd3c",
    }
    for binary, expected in identities.items():
        digest = hashlib.sha256()
        with binary.open("rb") as stream:
            for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                digest.update(chunk)
        actual = digest.hexdigest()
        if actual != expected:
            raise RuntimeError(f"pinned binary identity changed: {binary}")
    profile = '(version 1)(allow default)(deny network*)(deny mach-lookup (global-name "com.apple.SecurityServer"))(deny file-read* (subpath "/Users/adalundhe/Library/Keychains") (subpath "/Library/Keychains") (subpath "/Users/adalundhe/.claude") (literal "/Users/adalundhe/.claude.json") (subpath "/Users/adalundhe/.codex"))'
    (out / "sandbox.sb").write_text(profile + "\n")
    cases = [
        ("codex", [str(codex), "app-server", "--listen", "stdio://", "-c", 'cli_auth_credentials_store="file"', "-c", "analytics.enabled=false", "-c", "feedback.enabled=false"], {"id": 1, "method": "initialize", "params": {"clientInfo": {"name": "shards-adapter-local-probe", "version": "0.1.0"}, "capabilities": {"experimentalApi": True}}}),
        ("claude", [str(claude), "--print", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose", "--permission-prompt-tool", "stdio", "--setting-sources", "", "--settings", "{}", "--strict-mcp-config", "--mcp-config", '{"mcpServers":{}}', "--tools", "", "--no-session-persistence"], {"type": "control_request", "request_id": "shards-local-initialize", "request": {"subtype": "initialize", "hooks": None}}),
    ]
    records = []
    for name, command, message in cases:
        started = time.monotonic()
        deadline = started + args.deadline_seconds
        child = subprocess.Popen(["/usr/bin/sandbox-exec", "-p", profile] + command, cwd=state, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        raw = (json.dumps(message, separators=(",", ":")) + "\n").encode()
        (out / (name + ".stdin")).write_bytes(raw)
        child.stdin.write(raw)
        child.stdin.flush()
        capture = {child.stdout: bytearray(), child.stderr: bytearray()}
        selector = selectors.DefaultSelector()
        selector.register(child.stdout, selectors.EVENT_READ)
        selector.register(child.stderr, selectors.EVENT_READ)
        reply = None
        tail, error = b"", b""
        try:
            while time.monotonic() < deadline and selector.get_map() and reply is None:
                for key, _ in selector.select(max(0, deadline - time.monotonic())):
                    chunk = os.read(key.fd, 65536)
                    if not chunk:
                        selector.unregister(key.fileobj)
                        continue
                    capture[key.fileobj].extend(chunk)
                for line in bytes(capture[child.stdout]).splitlines():
                    try:
                        packet = json.loads(line)
                    except json.JSONDecodeError:
                        continue
                    if name == "codex" and packet.get("id") == 1:
                        reply = packet
                    if name == "claude" and packet.get("type") == "control_response" and packet.get("response", {}).get("request_id") == "shards-local-initialize":
                        reply = packet
            if name == "codex" and reply is not None:
                ack = b'{"method":"initialized"}\n'
                child.stdin.write(ack)
                child.stdin.flush()
                (out / "codex.initialized.stdin").write_bytes(ack)
            child.stdin.close()
            child.stdin = None
            tail, error = child.communicate(timeout=max(0, deadline - time.monotonic()))
        except BaseException:
            child.kill()
            tail, error = child.communicate()
            raise
        finally:
            selector.close()
            capture[child.stdout].extend(tail)
            capture[child.stderr].extend(error)
            (out / (name + ".stdout")).write_bytes(capture[child.stdout])
            (out / (name + ".stderr")).write_bytes(capture[child.stderr])
            record = {"name": name, "argv": command, "exit": child.returncode, "reply_before_eof": reply is not None, "elapsed_s": time.monotonic() - started}
            records.append(record)
            (out / "commands.json").write_text(json.dumps(records, indent=2) + "\n")
        if child.returncode != 0 or reply is None:
            raise RuntimeError(f"{name} local initialization failed")
        if name == "codex":
            if "result" not in reply:
                raise RuntimeError("Codex initialization was refused")
        else:
            response = reply["response"]
            data = response.get("response", {})
            if response.get("subtype") != "success" or data.get("claude_code_version") != "2.1.321" or data.get("account", {}).get("tokenSource") != "none":
                raise RuntimeError("Claude initialization/version/no-token contract failed")
    print(json.dumps(records, indent=2))


if __name__ == "__main__":
    main()
