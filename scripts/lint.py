#!/usr/bin/env python3
"""Run from the workspace to lint; trailing arguments are cargo check arguments."""
import os
from pathlib import Path
import subprocess
import sys
import tomllib

ROOT = Path(__file__).resolve().parents[1]
TOOLCHAIN = tomllib.loads((ROOT / "rust-toolchain.toml").read_text())["toolchain"]["channel"]


def main():
    build_env = os.environ.copy()
    # The plugin is a host library, independent of the project's target/flags.
    for key in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_BUILD_TARGET"):
        build_env.pop(key, None)
    build_env["CARGO_TARGET_DIR"] = str(ROOT / "target")
    subprocess.run(["cargo", f"+{TOOLCHAIN}", "build", "--locked"], cwd=ROOT, env=build_env, check=True)
    libraries = list((ROOT / "target/debug").glob(f"*async_hygiene@{TOOLCHAIN}-*"))
    libraries = [path for path in libraries if path.suffix in (".so", ".dylib", ".dll")]
    if len(libraries) != 1:
        raise RuntimeError(f"expected one Dylint library, found {libraries}")
    env = os.environ.copy()
    extra_flags = ["-Zalways-encode-mir", "-Zmir-opt-level=0"]
    if "CARGO_ENCODED_RUSTFLAGS" in env:
        flags = env["CARGO_ENCODED_RUSTFLAGS"]
        env["CARGO_ENCODED_RUSTFLAGS"] = "\x1f".join(filter(None, [flags, *extra_flags]))
    else:
        env["RUSTFLAGS"] = " ".join(filter(None, [env.get("RUSTFLAGS", ""), *extra_flags]))
    env.setdefault("DYLINT_DRIVER_PATH", str(ROOT / "target/dylint-drivers"))
    Path(env["DYLINT_DRIVER_PATH"]).mkdir(parents=True, exist_ok=True)
    arguments = sys.argv[1:]
    if arguments[:1] == ["--"]:
        arguments = arguments[1:]
    return subprocess.run([
        "cargo", f"+{TOOLCHAIN}", "dylint", "--lib-path", str(libraries[0]), "--", *arguments
    ], env=env).returncode


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
        print(f"async-hygiene: {error}", file=sys.stderr)
        sys.exit(1)
