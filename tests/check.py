#!/usr/bin/env python3
"""Exercise the actual Dylint driver and cross-crate compiler metadata."""
import collections
import json
import os
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[1]
FIXTURE = ROOT / "tests/fixtures/program"
subprocess.run(["cargo", "build", "--locked"], cwd=ROOT, check=True)
libraries = list((ROOT / "target/debug").glob("*async_hygiene@*"))
assert len(libraries) == 1, libraries
env = dict(os.environ, RUSTFLAGS="-Zalways-encode-mir -Zmir-opt-level=0")
# Ensure a changed lint library is exercised even when fixture sources are fresh.
env["CARGO_TARGET_DIR"] = str(ROOT / "target/fixtures")
env["DYLINT_DRIVER_PATH"] = str(ROOT / "target/dylint-drivers")
Path(env["DYLINT_DRIVER_PATH"]).mkdir(parents=True, exist_ok=True)
subprocess.run(["cargo", "clean", "-p", "hygiene_fixture"], cwd=FIXTURE, env=env, check=True)
result = subprocess.run([
    "cargo", "dylint", "--lib-path", str(libraries[0]), "--", "--message-format=json"
], cwd=FIXTURE, env=env, text=True, capture_output=True)
diagnostics = []
for line in result.stdout.splitlines():
    try:
        message = json.loads(line)
    except json.JSONDecodeError:
        continue
    if message.get("reason") == "compiler-message":
        diagnostics.append(message["message"])
print(result.stderr)
for message in diagnostics:
    if message.get("rendered"):
        print(message["rendered"])
assert result.returncode == 0, result.returncode
expected = collections.Counter()
for number, line in enumerate((FIXTURE / "src/lib.rs").read_text().splitlines(), 1):
    if "//~ prohibited" in line:
        expected[number] = line.count("//~ prohibited")
actual = collections.Counter()
for message in diagnostics:
    code = (message.get("code") or {}).get("code")
    assert code != "async_hygiene_incomplete", message
    if code == "disallowed_from_async":
        primary = [s for s in message["spans"] if s["is_primary"]]
        assert len(primary) == 1, primary
        actual[primary[0]["line_start"]] += 1
assert actual == expected, f"expected {expected}, got {actual}"
print(f"Verified {sum(expected.values())} prohibited call chains and all safe cases.")
