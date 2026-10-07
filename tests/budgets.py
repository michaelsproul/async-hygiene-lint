#!/usr/bin/env python3
"""Exercise each budget through the real driver, with an external hang guard."""
import json
import os
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
KEYS = ("max-instances", "max-iterations", "max-dataflow-iterations",
        "max-aggregate-depth", "max-recursive-instances", "max-incomplete-notes")
BASE = '[async_hygiene]\nprohibited = [{ path = "budget_fixture::bad" }]\ninsulators = []\n'
PRELUDE = '#![allow(dead_code, unused_assignments, unconditional_recursion)]\nfn bad() {}\nfn good() {}\n'
subprocess.run(["cargo", "build", "--locked"], cwd=ROOT, check=True)
library, = (ROOT / "target/debug").glob("*async_hygiene@*")
env = dict(os.environ, RUSTFLAGS="-Zalways-encode-mir -Zmir-opt-level=0",
           CARGO_TARGET_DIR=str(ROOT / "target/budget-tests"),
           DYLINT_DRIVER_PATH=str(ROOT / "target/dylint-drivers"))
for key in ("DYLINT_TOML", "DYLINT_RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"):
    env.pop(key, None)


def write_changed(path, content):
    if not path.exists() or path.read_text() != content:
        path.write_text(content)


def run(source, settings="", flags="", success=True):
    write_changed(project / "src/lib.rs", PRELUDE + source)
    write_changed(project / "dylint.toml", BASE + settings)
    result = subprocess.run([
        "cargo", "dylint", "--lib-path", str(library), "--", "--message-format=json"
    ], cwd=project, env=dict(env, DYLINT_RUSTFLAGS=flags),
        text=True, capture_output=True, timeout=120)
    messages, artifacts = [], []
    for line in result.stdout.splitlines():
        try:
            item = json.loads(line)
        except json.JSONDecodeError:
            continue
        if item.get("reason") == "compiler-message":
            messages.append(item["message"])
        elif item.get("reason") == "compiler-artifact":
            artifacts.append(item)
    rendered = result.stderr + "\n".join(m.get("rendered", "") for m in messages)
    assert "internal compiler error" not in rendered, rendered
    assert (result.returncode == 0) == success, rendered
    return messages, artifacts


def findings(result, code):
    return [m for m in result[0] if (m.get("code") or {}).get("code") == code]


def notes(result):
    return [c["message"] for m in findings(result, "async_hygiene_incomplete") for c in m["children"]]


def exhausted(result, key):
    return any(note.startswith(key + "=") for note in notes(result))


def prohibited(result):
    return findings(result, "disallowed_from_async")


def complete(result):
    assert not findings(result, "async_hygiene_incomplete"), notes(result)


def statistics(result):
    messages = [m["message"] for m in result[0] if m["message"].startswith("async hygiene statistics for")]
    assert len(messages) == 1, result
    return dict(field.split("=", 1) for field in messages[0].split(": ", 1)[1].split("; "))


def check():
    # Deep tuple construction, projection, and subfield assignment all use the
    # enclosing root's depth. This finite program requires more than eight levels.
    nested_type, nested_value = "fn()", "bad as fn()"
    for _ in range(12):
        nested_type, nested_value = f"({nested_type},)", f"({nested_value},)"
    deep = f"pub async fn deep() {{ let mut f: {nested_type} = {nested_value}; f{'.0' * 12} = bad; f{'.0' * 12}(); }}\npub async fn unrelated() {{}}\n"
    low = run(deep, 'max-aggregate-depth = 2\n')
    assert exhausted(low, "max-aggregate-depth"), notes(low)
    assert not prohibited(low)
    assert len(findings(low, "async_hygiene_incomplete")) == 1, low
    for depth in ("32", '"unlimited"'):
        high = run(deep, f"max-aggregate-depth = {depth}\nstatistics = true\n")
        assert int(statistics(high)["peak-aggregate-depth"]) >= 12
        complete(high)
        assert prohibited(high)

    # Existing recursive summaries are reused before the ancestry guard.
    recursive = "fn recurse(n: u32) { if n == 0 { bad(); } else { recurse(n - 1); } }\npub async fn entry() { recurse(2); }\n"
    result = run(recursive, "max-recursive-instances = 1\n")
    complete(result)
    assert prohibited(result)

    # A finite, growing generic accumulator makes each walk instance distinct;
    # the step type eventually terminates after more than eight expansions.
    finite = 'trait Step { fn step<U>(); }\nfn walk<T: Step, U>() { T::step::<(U,)>(); }\n'
    for i in range(13):
        body = "bad();" if i == 12 else f"walk::<S{i + 1}, U>();"
        finite += f"struct S{i}; impl Step for S{i} {{ fn step<U>() {{ {body} }} }}\n"
    finite += 'pub async fn entry() { walk::<S0, ()>(); }\npub async fn unrelated() {}\n'
    low = run(finite, 'max-recursive-instances = 2\n')
    assert exhausted(low, "max-recursive-instances"), notes(low)
    assert not prohibited(low)
    assert len(findings(low, "async_hygiene_incomplete")) == 1
    for limit in ("32", '"unlimited"'):
        high = run(finite, f"max-recursive-instances = {limit}\nstatistics = true\n")
        assert int(statistics(high)["peak-recursive-instances"]) > 8
        complete(high)
        assert prohibited(high)

    # Structural recursion's types shrink, so it must pass even at a limit of 1.
    shrinking = 'trait Walk { fn walk(); }\nimpl Walk for () { fn walk() { bad(); } }\nimpl<T: Walk> Walk for (T,) { fn walk() { T::walk(); } }\n'
    ty = "()"
    for _ in range(12):
        ty = f"({ty},)"
    shrinking += f'pub async fn entry() {{ <{ty} as Walk>::walk(); }}\n'
    result = run(shrinking, "max-recursive-instances = 1\n")
    complete(result)
    assert prohibited(result)

    expanding = 'fn expand<T>() { expand::<(T,)>(); }\npub async fn entry() { expand::<()>(); }\n'
    low = run(expanding, 'max-recursive-instances = 2\n')
    assert exhausted(low, "max-recursive-instances"), notes(low)
    # Unlimited recursion still respects the independently finite instance cap.
    low = run(expanding, 'max-recursive-instances = "unlimited"\nmax-instances = 50\n')
    assert exhausted(low, "max-instances"), notes(low)
    assert not exhausted(low, "max-recursive-instances")

    # Return summaries propagate backwards by one global round at a time.
    chain = "".join(f"fn f{i}() -> fn() {{ f{i + 1}() }}\n" for i in range(110))
    chain += 'fn f110() -> fn() { bad }\npub async fn entry() { f0()(); }\n'
    low = run(chain, 'max-iterations = 100\nmax-dataflow-iterations = "unlimited"\n')
    assert exhausted(low, "max-iterations"), notes(low)
    assert not exhausted(low, "max-dataflow-iterations")
    for limit in ("150", '"unlimited"'):
        high = run(chain, f'max-iterations = {limit}\nmax-dataflow-iterations = "unlimited"\nstatistics = true\n')
        assert int(statistics(high)["solver-iterations"]) > 100
        complete(high)
        assert prohibited(high)

    # A reverse-ordered pointer chain requires over 100 intra-body rounds.
    dataflow = "pub async fn entry() {\n"
    dataflow += "".join(f"let mut f{i}: fn() = good;\n" for i in range(111))
    dataflow += "".join(f"f{i} = f{i + 1};\n" for i in range(110))
    dataflow += "f110 = bad; f0(); }\n"
    low = run(dataflow, 'max-iterations = "unlimited"\nmax-dataflow-iterations = 100\n')
    assert exhausted(low, "max-dataflow-iterations"), notes(low)
    assert not exhausted(low, "max-iterations")
    assert not prohibited(low)
    for limit in ("150", '"unlimited"'):
        high = run(dataflow, f'max-iterations = "unlimited"\nmax-dataflow-iterations = {limit}\nstatistics = true\n')
        assert int(statistics(high)["peak-dataflow-iterations"]) > 100
        complete(high)
        assert prohibited(high)

    # Inherited local iteration budget remains backwards compatible.
    low = run(dataflow, "max-iterations = 100\n")
    assert exhausted(low, "max-dataflow-iterations"), notes(low)
    assert any("inherited from max-iterations" in note for note in notes(low))
    high = run(dataflow, 'max-iterations = 100\nmax-dataflow-iterations = "unlimited"\n')
    complete(high)
    assert prohibited(high)

    direct = "pub async fn entry() { bad(); }\n"
    low = run(direct, "max-instances = 1\n")
    assert exhausted(low, "max-instances"), notes(low)
    high = run(direct, "max-instances = 100\n")
    complete(high)
    assert prohibited(high)

    # Four open-world seeds fit, but constructing the concrete coroutine does
    # not. The synchronous factory is not reachable from the async call graph;
    # its exhausted allocation budget must still reach the async diagnostic.
    factory = "pub fn factory() { let f: fn() = good; let _future = async move { f(); }; }"
    low = run(factory, "max-instances = 4\n")
    assert exhausted(low, "max-instances"), notes(low)
    assert any("observed/attempted count 5" in note for note in notes(low))
    complete(run(factory, "max-instances = 100\n"))

    # Unsupported coverage stays visible with all budgets unlimited.
    opaque = 'unsafe extern "Rust" {\n' + "".join(f"fn opaque{i}();\n" for i in range(7)) + "}\n"
    calls = "".join(f"unsafe {{ opaque{i}(); }}\n" for i in range(7))
    opaque += f"pub async fn entry() {{ {calls} bad(); }}\n"
    unlimited = "".join(f'{key} = "unlimited"\n' for key in KEYS)
    high = run(opaque, unlimited)
    assert len([n for n in notes(high) if n.startswith("MIR unavailable")]) == 7
    assert prohibited(high)
    for cap in (1, 5):
        low = run(opaque, f"max-incomplete-notes = {cap}\n")
        assert len([n for n in notes(low) if n.startswith("MIR unavailable")]) == cap
        assert any(f"{7 - cap} additional incomplete reasons omitted" in n for n in notes(low))
    # The note cap cannot hide an exhausted budget.
    both = opaque.replace(f"{calls} bad();", f"{calls} let f: {nested_type} = {nested_value}; f{'.0' * 12}();")
    low = run(both, "max-aggregate-depth = 2\nmax-incomplete-notes = 1\n")
    assert exhausted(low, "max-aggregate-depth"), notes(low)
    assert not any(n.startswith(("MIR unavailable", "unresolved function pointer")) for n in notes(low)), notes(low)
    assert any("8 additional incomplete reasons omitted" in n for n in notes(low)), notes(low)
    # Repeated failures of one budget are capped, while separate exhausted
    # budgets each retain a visible note even when the cap is only one.
    low = run(direct, "max-instances = 1\nmax-incomplete-notes = 1\n")
    assert len([n for n in notes(low) if n.startswith("max-instances=")]) == 1
    high = run(direct, 'max-instances = 1\nmax-incomplete-notes = "unlimited"\n')
    occurrences = len([n for n in notes(high) if n.startswith("max-instances=")])
    assert occurrences > 1
    assert any(f"{occurrences - 1} additional incomplete reasons omitted" in n for n in notes(low)), notes(low)
    low = run(deep, "max-aggregate-depth = 2\nmax-dataflow-iterations = 1\nmax-incomplete-notes = 1\n")
    assert exhausted(low, "max-aggregate-depth") and exhausted(low, "max-dataflow-iterations"), notes(low)

    for source, settings in [(direct, "max-instances = 1\n"), (opaque, unlimited)]:
        result = run(source, settings, "-Dasync_hygiene_incomplete", success=False)
        assert all(m["level"] == "error" for m in findings(result, "async_hygiene_incomplete"))
    result = run(deep, unlimited, "-Ddisallowed_from_async", success=False)
    assert prohibited(result)

    # Statistics are opt-in, report attempted work, and distinguish convergence
    # from coverage. A synchronous-only expanding crate must do no analysis.
    assert not any(m["message"].startswith("async hygiene statistics") for m in run(direct)[0])
    for key in ("max-instances", "max-iterations", "max-dataflow-iterations"):
        result = run(deep, f"{key} = 1\nstatistics = true\n")
        stats = statistics(result)
        assert key in stats["exhausted-budgets"], stats
        if key == "max-instances":
            assert stats["instances"] == "1" and stats["peak-instance-attempt"] == "2", stats
        elif key == "max-iterations":
            assert stats["solver-converged"] == "false", stats
        else:
            assert stats["dataflow-converged"] == "false", stats
    result = run(expanding.split("pub async")[0], unlimited + "statistics = true\n")
    complete(result)
    stats = statistics(result)
    assert stats["instances"] == stats["solver-iterations"] == "0", stats
    assert stats["skipped"] == "no-local-async-entry-points", stats
    assert stats["exhausted-budgets"] == "[]", stats

    # Every supported async entry-point kind independently prevents the skip.
    # The capture factory also verifies why synchronous seeding remains needed.
    entries = [
        direct,
        "pub fn factory() { let f: fn() = bad; let _future = async move { f(); }; }",
        "pub fn factory() { let _closure = async move || { bad(); }; }",
        "pub struct Custom; impl std::future::Future for Custom { type Output = (); fn poll(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> std::task::Poll<()> { bad(); std::task::Poll::Ready(()) } }",
    ]
    for source in entries:
        result = run(source, "statistics = true\n")
        complete(result)
        assert prohibited(result), result
        stats = statistics(result)
        assert stats["skipped"] == "no" and int(stats["instances"]) > 0, stats
        assert stats["solver-converged"] == stats["dataflow-converged"] == "true", stats
        assert stats["exhausted-budgets"] == "[]", stats
        assert float(stats["elapsed-ms"]) >= 0, stats

    # Actual dylint.toml edits (not an environment override) must invalidate
    # Cargo's result for every setting, even when the source is unchanged.
    for key in KEYS:
        run(direct, f"{key} = 200\n")
        changed = run(direct, f"{key} = 201\n")
        assert changed[1] and all(not a["fresh"] for a in changed[1]), (key, changed)


with tempfile.TemporaryDirectory(prefix="budget-fixture-", dir=ROOT / "target") as directory:
    project = Path(directory)
    (project / "src").mkdir()
    (project / "Cargo.toml").write_text('[package]\nname = "budget_fixture"\nversion = "0.0.0"\nedition = "2024"\n[workspace]\n')
    check()
print("Verified all budgets, deep provenance, recursion, >100-round convergence, note elision, deny levels, config invalidation, statistics, and entry-point skipping.")
