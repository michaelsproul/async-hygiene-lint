#!/usr/bin/env python3
"""Verify config invalidation, deny levels, coverage warnings, and runner setup."""
import json
import os
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[1]
FIXTURE = ROOT / "tests/fixtures/program"
RUNNER = ROOT / "scripts/lint.py"
base_env = os.environ.copy()
base_env["CARGO_TARGET_DIR"] = str(ROOT / "target/control-tests")
base_env.pop("RUSTFLAGS", None)
base_env["CARGO_ENCODED_RUSTFLAGS"] = "--cfg=async_hygiene_runner_test"


def run(config=None, flags=""):
    env = dict(base_env, DYLINT_RUSTFLAGS=flags)
    if config is not None:
        env["DYLINT_TOML"] = config
    result = subprocess.run([
        "python3", str(RUNNER), "--lib", "--message-format=json"
    ], cwd=FIXTURE, env=env, text=True, capture_output=True)
    messages = []
    for line in result.stdout.splitlines():
        try:
            item = json.loads(line)
        except json.JSONDecodeError:
            continue
        if item.get("reason") == "compiler-message":
            messages.append(item["message"])
    assert "internal compiler error" not in result.stderr, result.stderr
    return result, messages


result, messages = run(flags="-Ddisallowed_from_async")
assert result.returncode != 0, result.stderr
errors = [m for m in messages if (m.get("code") or {}).get("code") == "disallowed_from_async"]
assert errors and all(m["level"] == "error" for m in errors), messages
# These calls reach non-inline MIR through two dependency crates. The encoded
# flags set above must not prevent the runner from adding the analysis flags.
assert any("blocking::bad" in m["message"] for m in errors), messages

result, messages = run("[async_hygiene]\nprohibited = []", "-Ddisallowed_from_async")
assert result.returncode == 0, result.stderr
assert not any((m.get("code") or {}).get("code") in ("disallowed_from_async", "async_hygiene_incomplete") for m in messages), messages

result, messages = run("[async_hygiene]\nprohibitted = []")
assert result.returncode != 0
assert any("invalid async_hygiene configuration" in m["message"] for m in messages), (result.stderr, messages)

result, messages = run('''[async_hygiene]
prohibited = [{ path = "hygiene_fixture::bad" }]
insulators = [{ path = "hygiene_fixture::offload", callback-args = [9] }]
''')
assert result.returncode != 0
assert any("out-of-range callback argument" in m["message"] for m in messages), (result.stderr, messages)

result, messages = run("[async_hygiene]\nmax-instances = 1")
assert result.returncode == 0, result.stderr
assert any((m.get("code") or {}).get("code") == "async_hygiene_incomplete" and any("work limit" in child["message"] for child in m["children"]) for m in messages), messages

# Without dependency MIR, lack of a transitive warning must be accompanied by
# an explicit coverage diagnostic. Use a separate Cargo artifact directory.
library = next((ROOT / "target/debug").glob("*async_hygiene@*.so"), None)
if library is None:
    library = next((ROOT / "target/debug").glob("*async_hygiene@*.dylib"))
env = dict(base_env, CARGO_TARGET_DIR=str(ROOT / "target/missing-mir-tests"),
           CARGO_ENCODED_RUSTFLAGS="-Zmir-opt-level=0",
           DYLINT_DRIVER_PATH=str(ROOT / "target/dylint-drivers"))
result = subprocess.run(["cargo", "dylint", "--lib-path", str(library)],
                        cwd=FIXTURE, env=env, text=True, capture_output=True)
assert result.returncode == 0, result.stderr
assert "MIR unavailable for `bridge::transitive`" in result.stderr, result.stderr
print("Verified runner, encoded flags, config invalidation/errors, deny levels, work limits, and missing dependency MIR.")
