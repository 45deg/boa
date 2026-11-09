# One-shot and multi-shot VM segment experiments

Base: `5a6281f7523decbb3921d58f1482210f2e03eb9e`.
Branch: `dev`.

This is stage 1 of the shift/reset project. The implementation lives in
`core/engine/src/vm/continuation.rs` and is compiled only with `cfg(test)`.
This module does not implement public dispatch. The separately implemented
opt-in one-shot API is described in [delimited-api.md](delimited-api.md).

## What is implemented

`Prompt::capture` moves a suffix of the real VM frame stack and its value stack
into a traced `Segment`. Frame and register pointers become relative to that
segment. Environment pointers retain their own coordinate system.

`Continuation::resume` takes ownership of that segment, relocates its frames to
the current stack base, supplies the saved call's result, and executes the actual
VM interpreter. Its bottom frame exits back to the Rust driver. Return and throw
both consume the continuation and restore the caller's frame boundary, stack,
return value, and shadow stack. Validation failures leave the segment available.

The driver executes bytecode one instruction at a time until an ordinary
JavaScript `marker()` call returns. An ordinary function named `thunk` defines
the delimiter for this experiment. These names are test-driver conventions,
not special JavaScript bindings or a proposed implementation of the builtins.
The driver simulates the handler returning 99; it does not execute a shift handler.

## Tests

```sh
cargo test -p boa_engine --lib vm::continuation --no-default-features --locked --offline
cargo test -p boa_engine --lib --no-default-features --locked --offline
cargo fmt --all --check
cargo clippy -p boa_engine --lib --tests --no-default-features --locked --offline
```

Tests cover:

- Two captured JS frames, a changed stack base, and no replay of earlier effects.
- Resumption after the original caller has finished, followed by rejection of reuse.
- A resumed throw, consumption, and successful subsequent script evaluation.
- A saved closure/object across forced GC while the segment is held in a GC cell.
- Restoration of a still-live caller after resuming its captured callee.
- Native-depth mismatch rejection without changing the running frames or stack.
- Conservative rejection of a function with a finally handler.
- A rejected stack limit followed by successful resumption of the same segment.
- Dropping an unresumed segment without executing its remaining effects.

## Multi-shot feasibility experiment

`MultiShot` retains a `Segment` and clones its frames/value stack on each call,
then uses the same one-shot resume path for the working copy. The saved segment
remains reusable after either return or throw. Heap objects and environment
bindings are shared; they are not rolled back.

Four added tests establish:

| Case | Observed result |
| --- | --- |
| Two-frame pure continuation, inputs 3, 4, 3 | 7, 9, 7; capture prefix runs once |
| Local `let count = 0; marker(); return ++count` | 1, 1 |
| Same local with unused `const read = () => count` before marker | 1, 2 |
| Existing object property increment across resumes and forced GC | 1, 2, 3; objects allocated after marker are fresh |
| Rejected stack limit, then throw, then successful resume | Snapshot remains reusable, caller VM state is restored |

The local-binding test is a **characterization of a defect in the naive
prototype**, not an acceptance test claiming the desired semantics works.
The design requires 1, 2 in both local-binding cases. A reader closure changing
observable results means this implementation cannot be exposed as multi-shot.

`ByteCompiler::get_binding` maps local bindings to registers; cloned registers
restore the old value. Escaping bindings live in shared environments, so their
updates survive the next resume. Sharing an explicit object demonstrates the
shared-storage approach, but is not a general compiler fix.

The follow-up below implements an opt-in compilation pass that makes bindings in
potentially captured frames escape into shared environments. Existing
`Scope::escape_all_bindings` and the binding escape analyzer are relevant entry
points used by that pass.
All reachable synchronous callees matter, including functions with no direct
shift call. Parameters, block/loop bindings, closures, and previously compiled
functions must use consistent storage. Disabling instruction optimizations
alone does not establish that contract.

Conclusion: repeated synchronous control resumption is feasible in this
prototype. The original clone-only path fails shared-store semantics; the opt-in pass
below addresses the tested binding cases. Broader semantics still require validation. Native boundaries, nested prompts, resource cleanup,
reentrant invocation, and performance remain outside this experiment. The
snapshot clone is linear in the saved frame/stack size; no timing or memory
benchmark has been performed.

## Boundaries of the evidence

This is a same-Context, synchronous, trusted internal experiment. It does not
provide public `shift`, `reset`, or a JavaScript-callable continuation. It does
not validate arbitrary prompt lifetimes, cross-Context use, repeated capture,
nested prompts, reentrant invocation, async execution, or resource cleanup.

Frames with exception handlers, active iterators, async/generator code, or an
early-exit boundary are rejected. Pending exceptions and native activity are
also rejected. The native-depth test changes the recorded depth directly; it is
not evidence that every real native callback path has been audited. Capturing
protected regions and full GC lifecycle coverage remain later-stage work.

The synchronous `Context::run()` invocation belongs to the Rust test driver.
When implementing the JS API, control transfer must be integrated into call
dispatch; wrapping this driver in an ordinary NativeFunction would introduce
the native-stack boundary the design intends to avoid.

## Integration follow-up

The separate `vm/delimited.rs` now introduces VM-recognized callable kinds for reset, shift, and continuation. The original integration goals were:
and explicit prompt/return routing. Replace the marker-based pause with shift's
call dispatch. Preserve function identity under aliasing and property calls,
and route normal and spread calls through the same control-transfer path.
Then test handlers, nested prompts, invalid arguments, and actual host barriers
before exposing the feature through Context/CLI registration.

## Recorded verification (2026-11-09)

On Rust 1.97.1, using the checked-in Cargo.lock:

- Before editing: 29 existing `vm::tests` passed with default features disabled.
- After the one-shot edit: all 1,134 engine library tests passed with default features
  disabled, including the 7 new continuation tests.
- `cargo fmt --all --check` passed.
- Engine library/test Clippy with default features disabled passed without warnings.

Default-feature/all-feature builds, Test262, CLI behavior, performance, and
real native-callback capture were not tested. No remote fork or pull request was created.

Multi-shot follow-up: the 4 new tests and all 7 existing continuation tests
passed. The full no-default-features engine library suite passed 1,138 tests.
The mismatch test passes by asserting the observed defect; it does not mean
multi-shot satisfies the proposed semantics.

## Shared-binding compiler follow-up

`Script::share_bindings_for_continuation_experiment` in boa_ast runs an opt-in
escape-analysis pass on an already analyzed AST and recomputes scope indices
before bytecode compilation. `BindingEscapeAnalyzer::share_bindings` selects
existing GC-managed environment slots instead of register-local binding storage.
It is separate from `direct_eval`: no eval semantics or synthetic JS closure is
introduced. The engine entry point exists only under `cfg(test)`; the AST helper
is hidden from generated docs, but callable across the crate boundary.

This conservative prototype shares all local bindings, including immutable
ones, rather than trying to infer which functions a future shift could capture.
Normal compilation still uses `share_bindings = false`. The old mismatch test
continues to prove the unmodified allocation path behaves differently.

The shared-mode matrix tests closure absent/present for `let`, `var`, a parameter,
a default parameter, a nested block local, and a `for` initializer binding. Each
case now yields **1, 2, 3** across three resumes. The function/parameter cases also
capture two JS frames and force GC between resumes. The loop test captures the
first iteration; it is not comprehensive loop/iterator coverage.

Additional tests verify that a closure can change the saved binding between
resumes, a block entered after capture allocates a fresh environment on each
resume, and ordinary shared-mode evaluation retains const assignment and TDZ
errors. The latter is a compilation check, not support for capturing try/catch.

The mode applies to a whole selected Script before code generation. Modules,
separately parsed eval/Function code, already compiled functions, Context-wide
configuration, performance, and full JavaScript conformance in shared mode are
not covered. No public multi-shot API is enabled.

Shared-binding verification (2026-11-09): with default features disabled,
38 AST, 313 parser, and 1,141 engine library tests passed (one parser test ignored).
The engine suite includes 14 continuation tests; three added tests cover the
shared-binding matrix and its accompanying checks. This is not a full suite
run with every script forced into shared mode.
