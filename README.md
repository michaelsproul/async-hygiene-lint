# Async hygiene lint

A [Dylint](https://github.com/trailofbits/dylint) library that reports calls to
blocking or otherwise prohibited functions from async execution, including calls
hidden behind synchronous helpers in other crates. “Prohibited” is a runtime
policy; it does not mean Rust's `unsafe fn` keyword.

```rust
fn helper(handle: &tokio::runtime::Handle) {
    handle.block_on(async {});
}

async fn task(handle: tokio::runtime::Handle) {
    helper(&handle); // warning, with the call chain to Handle::block_on
    tokio::task::spawn_blocking(move || helper(&handle)); // insulated callback
}
```

The design follows Michael Sproul's original
[`disallowed_from_async` Clippy lint](https://github.com/michaelsproul/rust-clippy/commit/5cf4e802500b1067f9eed6df88a0fa0303d06c96):
build a call graph, propagate prohibited effects across synchronous helpers, and
stop propagation at blocking execution boundaries. This implementation uses
instantiated MIR and Cargo's dependency metadata instead of separate taint files.

## Run

Install [Rustup](https://rustup.rs/), Python 3.11 or newer, and Dylint:

```sh
cargo install --locked cargo-dylint dylint-link --version 6.1.0
cd /path/to/async-hygiene-lint
rustup show # installs the pinned nightly and components
cargo build --locked

cd /path/to/project-to-check
python3 /path/to/async-hygiene-lint/scripts/lint.py --workspace --all-targets
```

The runner builds the lint for its pinned nightly, sets
`-Zalways-encode-mir -Zmir-opt-level=0` for the checked project **and its
dependencies**, then invokes Dylint. Additional arguments are passed to
`cargo check`, including `--features`, `--release`, `--target`, and `-p`.
Existing `RUSTFLAGS` or `CARGO_ENCODED_RUSTFLAGS` are preserved. The Dylint driver
cache lives under this repository's `target/dylint-drivers` by default.

The pinned compiler is required because this library uses unstable compiler APIs.
Linux and macOS host linker configurations are included. Other hosts need an
appropriate `dylint-link` linker configuration.

To integrate through Cargo metadata instead of the runner:

```toml
[workspace.metadata.dylint]
libraries = [{ path = "/path/to/async-hygiene-lint" }]
```

```sh
RUSTFLAGS='-Zalways-encode-mir -Zmir-opt-level=0' cargo dylint --all
```

Use `RUSTFLAGS`, not just `DYLINT_RUSTFLAGS`, for those two analysis flags:
non-workspace dependencies also need their MIR encoded. The runner handles this.
Ordinary Cargo caching handles changed source, features, and target options;
there are no separate persisted taint summaries to become stale.

## Configuration

Put `dylint.toml` in the checked workspace's root. Start with
[`dylint.example.toml`](dylint.example.toml). With no configuration, the lint
prohibits Tokio's two `block_on` methods, `futures_executor::block_on`,
`std::thread::sleep`, and `std::thread::park`. It recognizes Tokio's
`spawn_blocking` and `block_in_place`, plus `std::thread::spawn`, as insulators.

```toml
[async_hygiene]
prohibited = [
    { path = "my_crate::blocking_io", reason = "blocks the executor thread" },
    { path = "tokio::runtime::Handle::block_on" },
]
insulators = [
    { path = "tokio::task::spawn_blocking", callback-args = [0] },
    { path = "my_crate::Worker::offload", callback-args = [1] },
]
max-instances = 10000
max-iterations = 100
```

Each provided array **replaces** its default array; omitted fields retain their
defaults. `prohibited = []` disables the analysis. Unknown fields and invalid
limits are configuration errors.

Paths begin with the original Rust crate name (hyphens become underscores), even
if a dependency is renamed locally. Exact paths resolve re-exports, inherent
methods, and trait methods by definition identity. A `*` wildcard matches any
substring, including `::`, of rustc's displayed definition path. Prefer exact
public paths; displayed paths can change with compiler versions and imports.
Rules for dependencies absent from the current crate are harmless.

Insulators are **trusted API contracts**. `callback-args` contains zero-based
argument indices; methods include `self` at index zero. The configured callback
runs outside the caller's async context. Other callable arguments are treated
conservatively as running in the caller's context. The insulator implementation
is not traversed, so configure only APIs whose implementations uphold this
contract. Prohibition takes precedence over insulation when both match.

Argument evaluation is always checked:

```rust
tokio::task::spawn_blocking({
    blocking_io(); // still called on the async thread: warning
    || blocking_io() // callback execution is insulated
});
```

An `async` block inside a blocking callback starts a new async context and is
checked independently. Constructing an ordinary closure or taking a function's
address does not execute its body.

## Diagnostics and analysis scope

`disallowed_from_async` emits one shortest witness chain per async body and
prohibition rule. Async functions, async blocks, async closures, and handwritten
`Future::poll` implementations are entry points. Rust's normal `allow`, `warn`,
`deny`, and `expect` attributes apply at the async entry point. For example:

```rust
#[cfg_attr(dylint_lib = "async_hygiene", deny(disallowed_from_async))]
async fn task() { /* ... */ }
```

For command-line enforcement, set `DYLINT_RUSTFLAGS='-Ddisallowed_from_async'`.
To enforce coverage as well, also deny `async_hygiene_incomplete`.

The interprocedural analysis follows dependency MIR, resolves concrete generic
and trait calls, tracks function pointers through calls, returns, captures, and
aggregate fields, and includes destructor calls. Distinct callable arguments
get distinct summaries. A fixed point handles recursive call graphs. Coroutine
construction contributes captured values without pretending that constructing
a future immediately polls it.

This is a conservative static lint, not a proof of runtime safety:

* Branch conditions and assignment order are not used to rule out calls. Both
  sides of a branch, earlier pointer assignments, and possible array elements
  may contribute targets, causing false positives.
* Dynamic trait objects, unknown function pointers, unresolved generic calls,
  and unavailable dependency/FFI bodies produce `async_hygiene_incomplete`.
  Precompiled standard-library MIR is incomplete; dropping some Tokio handles
  reaches opaque runtime vtables and can produce this diagnostic even when
  their blocking callbacks are correctly insulated.
* Arbitrary heap aliasing, pointer arithmetic, mutable global state, and
  interprocedural writes through pointers are not modeled. Callable provenance
  through these operations may be lost. This is not a sound points-to analysis.
* Uncalled generic async code is checked with symbolic parameters. Calls that
  require concrete types can remain unresolved. Async bodies in dependencies
  are traversed when reached; use `--workspace --all-targets` to check entry
  points across your workspace. Uncompiled feature/target combinations are not
  analyzed.
* Analysis is bounded by the configured instance and iteration limits, plus an
  aggregate provenance nesting limit of eight and eight distinct instances of
  one function along a recursive expansion unless its types shrink (as in
  structural drop glue). Ordinary recursive calls reuse
  their existing summaries. Exhaustion reports incomplete
  analysis instead of silently truncating a safety conclusion.

## Development

```sh
cargo fmt --check
cargo test --locked
python3 tests/check.py
python3 tests/check.py tokio
python3 tests/check.py limits
python3 tests/controls.py
```

The integration tests load the actual Dylint library, compile a three-crate
fixture and real Tokio/futures dependencies, and verify exact diagnostic
locations and counts, including safe cases and expected coverage warnings.
The compiler sources for the pinned toolchain are installed by `rustc-dev` under
`lib/rustlib/rustc-src/rust/compiler`.
