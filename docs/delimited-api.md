# Experimental one-shot JavaScript API

Enable explicitly from Rust with `context.enable_delimited_continuations()?`,
or from the CLI with `--delimited-continuations`. Disabled contexts do not gain
new globals. Existing own global properties named shift/reset are not overwritten.

```sh
cargo run -p boa_cli --no-default-features --locked --offline -- \
  --delimited-continuations examples/continuations.js
```

```js
reset(() => 1 + shift(k => k(2) * 10)); // 30
const first = reset(() => {
  function inner() { return shift(k => k) + shift(k => k); }
  return 10 + inner();
});
const second = first(2);
second(3); // 15
first(4); // TypeError: consumed
```

## Control transfer

`vm/delimited.rs` recognizes the function's internal data, not its variable name.
The normal/spread Call opcodes and the non-intrinsic branches of CallEval use
this dispatch. A prompt records the bottom frame index and host reentry depth.

Reset prepares the thunk frame without entering a recursive Rust interpreter.
Shift validates all frames to the prompt, removes its own call operands, moves
that suffix of the VM stack into a GC-traced continuation, and prepares the
handler under the same prompt. It does not throw a JavaScript exception.

Calling a continuation consumes its saved segment and installs every saved frame
under a fresh prompt. Its argument becomes the suspended call's result. All
outer pending frames are already in the VM when a resumed computation captures
again, so they move into the new continuation along with the innermost frame.
Returning or unwinding the bottom frame pops that prompt through `Vm::pop_frame`.
The ordinary VM call-result path delivers the result to the handler/caller.

This follows the control rules in the local QuickJS fork's `dc_finish_prompt`
and `dc_resume`, but does not copy its C-stack unwinding or exception sentinel.
The imported JS fixture acknowledges its source in the file header.

## Supported and rejected paths

- Synchronous ordinary JS callbacks; ordinary and method calls, aliases and spread.
- Nested reset, shift inside a handler, recapture during resumption, escaped
  one-shot continuations, and rejection of recursive/repeated invocation.
- Tested return/throw/catch/finally behavior. Capture does not run cleanup, and
  dropping a saved continuation does not execute its finally blocks.
- Saved environments/objects across GC and mapped arguments aliasing.
- Same Context and realm only. Validation failures leave a saved segment unused;
  once execution begins, return and throw both consume it.

Two checks protect capture: the host reentry depth must match, and every captured
frame must have been prepared by a supported explicit bytecode call. Native,
constructor, implicit-call, Proxy/bound forwarding, and async/generator frames
cannot be crossed. Pending VM exceptions and active iterator records are also
rejected. A fresh reset inside a native callback or getter is allowed when its
capture stays inside that new prompt.

Reset/shift callbacks themselves cannot be native, async/generator, bound, Proxy,
or class constructor functions. Native `.call`/`.apply`/`Reflect.apply`, bound or
Proxy wrappers of the control functions, and direct Rust `JsObject::call` on a
control function are unsupported. Use JavaScript to invoke the API explicitly.
A native function is otherwise usable inside a reset when capture does not cross
its active invocation. Ordinary loops are supported; active for-of iterators
are conservatively rejected.

Multi-shot is **not exposed**. The independent `cfg(test)` experiment and shared
binding compiler pass remain in `vm/continuation.rs`; their results do not imply
multi-shot works through this API. Exhaustive opcode, resource/OOM, cross-platform,
performance and Test262 validation remain future work.

## Reproducible checks

```sh
cargo test -p boa_engine --lib vm::delimited --no-default-features --locked --offline
cargo test -p boa_engine --lib --no-default-features --locked --offline
cargo fmt --all --check
cargo clippy -p boa_engine -p boa_cli --lib --bins --tests --no-default-features --locked --offline
```

The QuickJS-derived fixture runs 42 checks via real JS calls. Differences are
explicit: Boa supplies `gc()` from the host, assertions check TypeError rather
than QuickJS message strings, and the spread test expects support rather than
rejection. The original QuickJS files have not been changed.

Additional Rust-hosted JS tests cover registration, alias/shadowing (including a
local binding named eval), native map/getter/setter/coercion/Proxy/eval boundaries,
an actual custom Rust callback, recovery after rejection, fresh nested prompts,
handler versus resumed exceptions, capture during finally, later-script resumption,
Context isolation, non-consuming validation failure, and active iterator rejection.

## Verification on 2026-11-09

- Engine library: 1,152 tests passed with `--no-default-features`, including
  11 end-to-end API tests and the existing 14 internal continuation experiments.
- The 42-check JS fixture also passed through the built CLI using
  `--delimited-continuations --debug-object` and a host gc wrapper.
- The CLI example printed `30`, `9`, `15`, `true`, `true`, `3`.
- CLI built with `--no-default-features`; its mandatory engine features include
  trace/flowgraph/temporal, unlike the minimal engine-only test build.
- Workspace formatting and engine/CLI Clippy (`--lib --bins --tests`, default
  features disabled) passed without warnings.

Full default/all-feature test suites, Test262, sanitizer/OOM injection and
performance measurements were not run.
