# Experimental delimited-continuation JavaScript API

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

## Control transfer (one-shot)

`vm/delimited.rs` recognizes the function's internal data, not its variable name.
The normal/spread Call opcodes and the non-intrinsic branches of CallEval use
this dispatch. A prompt records the bottom frame index and host reentry depth.

Reset prepares the thunk frame without entering a recursive Rust interpreter.
Shift validates all frames to the prompt, removes its own call operands, moves
that suffix of the VM stack into a GC-traced continuation, and prepares the
handler under the same prompt. It does not throw a JavaScript exception.

Calling a one-shot continuation consumes its saved segment and installs every saved frame
under a fresh prompt. Its argument becomes the suspended call's result. All
outer pending frames are already in the VM when a resumed computation captures
again, so they move into the new continuation along with the innermost frame.
Returning or unwinding the bottom frame pops that prompt through `Vm::pop_frame`.
The ordinary VM call-result path delivers the result to the handler/caller.

This follows the control rules in the local QuickJS fork's `dc_finish_prompt`
and `dc_resume`, but does not copy its C-stack unwinding or exception sentinel.
The imported JS fixture acknowledges its source in the file header.

## Supported and rejected paths (one-shot)

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

Multi-shot is a separate opt-in mode described below. The independent `cfg(test)`
experiment remains in `vm/continuation.rs`; public multi-shot is validated by JS
calls in `vm/delimited/multishot_tests.rs`. Exhaustive opcode, resource/OOM,
cross-platform, performance and Test262 validation remain future work.

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

## Opt-in multi-shot mode

From Rust, call `context.enable_multishot_delimited_continuations()?` before
parsing Scripts. This registers the same `reset` and `shift` names as one-shot;
registration collisions are errors. The CLI alternatives are mutually exclusive:

```sh
cargo run -p boa_cli --no-default-features --locked --offline -- \
  --multishot-continuations examples/multishot-continuations.js
```

```js
reset(() => 1 + shift(k => k(2) + k(3))); // 7
const k = reset(() => {
  let count = 0;
  shift(k => k);
  return ++count;
});
k(); // 1
k(); // 2
k(); // 3
```

A multi-shot continuation keeps its control snapshot after return or throw.
Every invocation clones frames/registers, installs a fresh prompt, and runs
through the same VM dispatch as one-shot. Existing variable cells and heap
objects are shared; earlier side effects are neither replayed nor rolled back.
New scopes entered and objects allocated after capture are created on each
resume. Recursive invocation of the same continuation is allowed within VM
limits. Mutations made by the nested invocation remain visible to the outer one.
Recapture preserves pending outer frames and does not consume the original
multi-shot snapshot. Existing one-shot continuations still consume their state.

### Compilation coverage

Enabling multi-shot also selects shared binding analysis for subsequent
`Script::parse` calls in this Context. All local bindings (including const and
parameters) use existing GC-managed environment slots. After compilation the
Script and nested CodeBlocks receive a provenance flag. Capture checks every
frame for this flag, not merely the thunk containing reset or shift.

Previously parsed Scripts, already compiled functions, module code, and code
created by eval or the Function constructor do not acquire this flag. They can
execute, but cannot be part of a multi-shot capture; the operation throws before
moving frames. For example, an old caller outside a fresh reset is allowed, but
an old callee inside its captured segment is rejected. This prevents the prior
register-local versus closure-local mismatch from silently returning.

The AST pass is scoped to each selected Script. This is not support for arbitrary
dynamically compiled or module code, nor a retrofit of existing functions.
Failed global registration leaves the shared compilation setting unchanged. Once enabled,
shared Script compilation stays enabled in that Context; there is no disable API.
Native/implicit/async/iterator and Context/realm restrictions still apply.

### Public multi-shot tests

The JS tests cover repeated composition and escaped calls across scripts;
closure-present/absent let/var/parameter/default-parameter/loop variables;
external closure mutation, heap effects, fresh scopes, and GC; handler shift,
recapture across multiple frames, bounded recursive resumption; throw/finally,
catch followed by capture, mapped arguments, const and TDZ checks; rejection of
old/dynamic/module code; actual native callback boundaries; and coexistence with
saved one-shot continuations.

An uncaught error from a rejected capture exposed retained value-stack storage
when a callee unwound into a host-entry frame. `handle_throw` now performs the
same stack/environment cleanup as a direct throw at that boundary. Tests verify
that uncaught resumed exceptions leave the Context usable and snapshots reusable.

Validation for this integration (default features disabled): 38 AST, 1,165
engine and 313 parser tests passed (one parser test ignored). The engine count
includes 13 public multi-shot tests and the existing one-shot API tests.
Both CLI examples produced their expected output; formatting and engine/CLI
Clippy passed. These runs do not establish Test262 or performance conformance.

Known baseline limitation: evaluating a rejected module leaves value-stack
storage even without continuation support. The module rejection test compares
against an ordinary module TypeError, checks that capture rejection adds no
extra storage, and checks prompt/frame cleanup and subsequent Script execution.
That existing module/job cleanup issue is not fixed by this change.


### Extended consistency checks

Seven additional Rust tests execute the public JS API (20 multi-shot tests in
total). They check:

- Shared destructured/default parameter storage and block shadowing under all
  combinations of AST optimization on/off, strict/sloppy mode and closure
  presence/absence.
- Nested prompt isolation and recovery after an inner reset throws.
- Repeated capture inside finally, preserving each snapshot's pending return
  value without replaying the prefix before that capture.
- Stack-size and recursion-limit rejection followed by successful repeated
  use of the same snapshot.
- Getter/setter/coercion/constructor/Proxy/native forwarding and active-iterator
  rejection, followed by multi-shot capture in the still-live outer prompt.
- Spread, method aliases and a continuation passed as an argument named eval.
- Foreign-Context and direct Rust-call rejection without damaging the original
  snapshot.

All these cases assert prompt, frame, value-stack and pending-exception cleanup
and subsequent Script execution. The module baseline exception documented above
remains confined to its separate test. These tests check supported examples and
rejection boundaries, not arbitrary-program equivalence or full ECMAScript
conformance.

After expansion, the no-default-features library suites passed: 38 AST,
1,172 engine and 313 parser tests (one parser test ignored). Engine/CLI Clippy,
workspace formatting and diff whitespace checks also passed.
