//! Internal one-shot and multi-shot experiments, compiled only for VM tests.
//!
//! `Prompt::capture` moves a synchronous frame/stack segment out of the VM;
//! `Continuation::resume` relocates it and runs it once, restoring the caller.
//! `MultiShot::resume` clones a saved segment through that same execution path;
//! its tests expose the difference between register locals and shared bindings.
//! `pause_with_storage` opts into shared AST bindings to test the compiler remedy.
//! Tests pause real bytecode after an ordinary marker call returns. This is not
//! shift/reset dispatch: no handler, nested prompts, or native-stack capture is
//! implemented here. Prompts and continuations must stay in their original Context.

use super::{CallFrame, CallFrameFlags, Stack, Vm};
use crate::{Context, JsNativeError, JsResult, JsValue};
use boa_gc::{Finalize, Trace};

/// A frame boundary recorded by the test driver before the marker call.
struct Prompt {
    frame_index: usize,
    host_call_depth: usize,
}

#[derive(Clone, Debug, Trace, Finalize)]
struct Segment {
    frames: Vec<CallFrame>,
    stack: Stack,
    return_value: JsValue,
}

/// Taking the segment consumes the continuation even if execution throws.
#[derive(Debug, Trace, Finalize)]
struct Continuation {
    suspended: Option<Segment>,
}

/// An immutable control snapshot, not a snapshot of the JavaScript heap.
/// This deliberately naive prototype is not suitable for a public API: register
/// locals are copied while environment bindings retain shared storage.
#[derive(Debug, Trace, Finalize)]
struct MultiShot {
    snapshot: Segment,
}

impl MultiShot {
    fn resume(&self, value: JsValue, context: &mut Context) -> JsResult<JsValue> {
        Continuation {
            suspended: Some(self.snapshot.clone()),
        }
        .resume(value, context)
    }
}

fn unsupported(message: &'static str) -> crate::JsError {
    JsNativeError::typ().with_message(message).into()
}

impl Prompt {
    /// Capture at a call-return boundary: the top stack slot is its result.
    /// All guards precede mutation. Protected regions are conservatively rejected
    /// for the whole function, not just its currently executing instruction.
    fn capture(&self, vm: &mut Vm) -> JsResult<Continuation> {
        if self.frame_index == 0 || self.frame_index >= vm.frames.len() {
            return Err(unsupported("invalid continuation prompt"));
        }
        if vm.host_call_depth != self.host_call_depth || vm.native_active_function.is_some() {
            return Err(unsupported("continuation crosses a native boundary"));
        }
        if vm.pending_exception.is_some() {
            return Err(unsupported("cannot capture a pending exception"));
        }
        for frame in &vm.frames[self.frame_index..] {
            if frame.exit_early()
                || frame.code_block.is_async()
                || frame.code_block.is_generator()
                || !frame.code_block.handlers.is_empty()
                || !frame.iterators.is_empty()
            {
                return Err(unsupported("unsupported continuation frame"));
            }
        }

        let base = vm.frames[self.frame_index].fp;
        let stack = vm.stack.split_off_frame(&vm.frames[self.frame_index]);
        let mut frames = vm.frames.split_off(self.frame_index);
        for frame in &mut frames {
            frame.fp -= base;
            frame.rp -= base;
            frame.flags |= CallFrameFlags::REGISTERS_ALREADY_PUSHED;
            vm.shadow_stack.pop();
        }
        Ok(Continuation {
            suspended: Some(Segment {
                frames,
                stack,
                return_value: vm.take_return_value(),
            }),
        })
    }
}

impl Continuation {
    /// Resume from the saved call result, then return to the Rust test driver.
    /// The bottom frame exits to this driver; it does not unwind caller frames.
    fn resume(&mut self, value: JsValue, context: &mut Context) -> JsResult<JsValue> {
        if context.vm.pending_exception.is_some() || context.vm.native_active_function.is_some() {
            return Err(unsupported("cannot resume at this VM boundary"));
        }
        let Some(segment) = self.suspended.as_ref() else {
            return Err(unsupported("continuation has already been consumed"));
        };
        let frame_count = context.vm.frames.len();
        let stack_base = context.vm.stack.stack.len();
        if frame_count - 1 + segment.frames.len() + context.vm.host_call_depth
            >= context.vm.runtime_limits.recursion_limit()
            || stack_base + segment.stack.stack.len()
                >= context.vm.runtime_limits.stack_size_limit()
        {
            return Err(unsupported("continuation exceeds VM limits"));
        }
        let base = u32::try_from(stack_base)
            .map_err(|_| unsupported("continuation stack offset overflow"))?;
        if segment.frames.iter().any(|frame| {
            frame.fp.checked_add(base).is_none() || frame.rp.checked_add(base).is_none()
        }) {
            return Err(unsupported("continuation frame offset overflow"));
        }

        let mut segment = self.suspended.take().expect("validated suspended state");
        let caller_return = context.vm.take_return_value();
        let caller_shadow = context.vm.shadow_stack.clone();
        // The driver stops immediately after the marker returns. Replacing this
        // slot feeds the resumed value into the existing call-result instruction.
        *segment.stack.stack.last_mut().expect("saved call result") = value;
        context.vm.stack.stack.append(&mut segment.stack.stack);
        for (index, mut frame) in segment.frames.drain(..).enumerate() {
            frame.fp += base;
            frame.rp += base;
            frame.set_exit_early(index == 0);
            context.vm.push_frame(frame);
        }
        context.vm.return_value = std::mem::take(&mut segment.return_value);
        let result = context.run().consume();

        // Return and throw have different VM cleanup paths. Restore the exact
        // caller boundary in either case, including its diagnostic shadow stack.
        while context.vm.frames.len() > frame_count {
            context.vm.pop_frame();
        }
        context.vm.stack.stack.truncate(stack_base);
        context.vm.shadow_stack = caller_shadow;
        context.vm.return_value = caller_return;
        context.vm.pending_exception = None;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Script, Source, environments::EnvironmentStack, vm::ActiveRunnable};
    use cow_utils::CowUtils;
    use std::ops::ControlFlow;

    /// Execute actual bytecode until the ordinary `marker()` function returns.
    /// `thunk` supplies the test delimiter; its caller remains live after capture.
    fn pause(context: &mut Context, source: &str) -> Prompt {
        pause_with_storage(context, source, false)
    }

    fn pause_with_storage(context: &mut Context, source: &str, shared: bool) -> Prompt {
        let script = Script::parse(Source::from_bytes(source), None, context).unwrap();
        if shared {
            script.share_bindings_for_continuation_experiment(context);
        }
        let code = script.codeblock(context).unwrap();
        context.vm.push_frame_with_stack(
            CallFrame::new(
                code.clone(),
                Some(ActiveRunnable::Script(script)),
                EnvironmentStack::new(),
                context.realm().clone(),
            )
            .with_env_fp(0)
            .with_flags(CallFrameFlags::EXIT_EARLY),
            JsValue::undefined(),
            JsValue::null(),
        );
        context.realm().resize_global_env();
        context.global_declaration_instantiation(&code).unwrap();
        let mut prompt = None;
        let mut marker_depth = None;
        for _ in 0..10_000 {
            let name = context.vm.frame().code_block.name().to_std_string_escaped();
            if name == "thunk" && prompt.is_none() {
                prompt = Some(Prompt {
                    frame_index: context.vm.frames.len() - 1,
                    host_call_depth: context.vm.host_call_depth,
                });
            }
            if name == "marker" {
                marker_depth = Some(context.vm.frames.len());
            }
            let frame = context.vm.frame();
            let pc = frame.pc as usize;
            let opcode = super::super::Opcode::decode(frame.code_block.bytecode.bytes[pc]);
            let flow = context.execute_one(
                |context, opcode| {
                    super::super::opcode::OPCODE_HANDLERS[opcode as usize](context, pc)
                },
                opcode,
            );
            assert!(matches!(flow, ControlFlow::Continue(())), "{flow:?}");
            if marker_depth.is_some_and(|depth| context.vm.frames.len() < depth) {
                return prompt.expect("thunk must have started");
            }
        }
        panic!("marker was not reached within the instruction budget");
    }

    fn finish_caller(context: &mut Context) {
        // Simulate the handler returning 99 to the original delimiter's caller.
        context.vm.stack.push(99);
        assert_eq!(context.run().consume().unwrap(), JsValue::from(109));
        context.vm.pop_frame();
        assert_eq!(context.vm.frames.len(), 1);
        assert!(context.vm.stack.stack.is_empty());
    }

    fn capture_multi(context: &mut Context, source: &str) -> MultiShot {
        let prompt = pause(context, source);
        let mut continuation = prompt.capture(&mut context.vm).unwrap();
        let snapshot = continuation.suspended.take().unwrap();
        finish_caller(context);
        MultiShot { snapshot }
    }

    #[test]
    fn shared_bindings_keep_external_closure_writes_and_fresh_post_capture_scopes() {
        let context = &mut Context::default();
        let prompt = pause_with_storage(
            context,
            r#"
            let write;
            function marker() { return 0; }
            function thunk() {
                let count = 0;
                write = value => { count = value; };
                marker();
                {
                    let fresh = 0;
                    fresh++;
                    return ++count * 10 + fresh;
                }
            }
            10 + thunk();
        "#,
            true,
        );
        let mut one = prompt.capture(&mut context.vm).unwrap();
        let saved = MultiShot {
            snapshot: one.suspended.take().unwrap(),
        };
        finish_caller(context);
        assert_eq!(
            saved.resume(JsValue::undefined(), context).unwrap(),
            11.into()
        );
        context.eval(Source::from_bytes("write(8)")).unwrap();
        boa_gc::force_collect();
        assert_eq!(
            saved.resume(JsValue::undefined(), context).unwrap(),
            91.into()
        );
        assert_eq!(
            saved.resume(JsValue::undefined(), context).unwrap(),
            101.into()
        );
    }

    #[test]
    fn shared_compilation_preserves_const_and_tdz_checks() {
        let context = &mut Context::default();
        let script = Script::parse(
            Source::from_bytes(
                r#"
            function check() {
                let score = 0;
                try { const value = 1; value = 2; } catch (e) {
                    if (e instanceof TypeError) score++;
                }
                try { value; let value = 1; } catch (e) {
                    if (e instanceof ReferenceError) score += 10;
                }
                return score;
            }
            check();
        "#,
            ),
            None,
            context,
        )
        .unwrap();
        script.share_bindings_for_continuation_experiment(context);
        assert_eq!(script.evaluate(context).unwrap(), 11.into());
    }

    #[test]
    fn shared_bindings_make_closure_presence_irrelevant() {
        // Test both mutable declaration forms, a parameter, a block local and
        // a for-loop binding. The default-storage characterization stays intact.
        for (parameters, declaration, call) in [
            ("", "let count = 0;", ""),
            ("", "var count = 0;", ""),
            ("count", "", "0"),
            ("count = 0", "", ""),
        ] {
            for reader in ["", "const read = () => count;"] {
                let context = &mut Context::default();
                let source = format!(
                    r#"
                    function marker() {{ return 0; }}
                    function inner({parameters}) {{
                        {declaration}
                        {reader}
                        marker();
                        return ++count;
                    }}
                    function thunk() {{ return inner({call}); }}
                    10 + thunk();
                "#
                );
                let prompt = pause_with_storage(context, &source, true);
                let mut one = prompt.capture(&mut context.vm).unwrap();
                let saved = MultiShot {
                    snapshot: one.suspended.take().unwrap(),
                };
                finish_caller(context);
                for expected in 1..=3 {
                    boa_gc::force_collect();
                    assert_eq!(
                        saved.resume(JsValue::undefined(), context).unwrap(),
                        expected.into(),
                        "parameters={parameters}, declaration={declaration}, reader={reader}"
                    );
                }
            }
        }
        for body in [
            "{ let count = 0; marker(); return ++count; }",
            "for (let count = 0; count < 10; count++) { marker(); return ++count; }",
        ] {
            for reader in ["", "const read = () => count;"] {
                let context = &mut Context::default();
                let body = body.cow_replace("marker();", &format!("{reader} marker();"));
                let source = format!(
                    "function marker() {{ return 0; }} function thunk() {{ {body} }} 10 + thunk();"
                );
                let prompt = pause_with_storage(context, &source, true);
                let mut one = prompt.capture(&mut context.vm).unwrap();
                let saved = MultiShot {
                    snapshot: one.suspended.take().unwrap(),
                };
                finish_caller(context);
                for expected in 1..=3 {
                    assert_eq!(
                        saved.resume(JsValue::undefined(), context).unwrap(),
                        expected.into()
                    );
                }
            }
        }
    }

    #[test]
    fn multi_shot_replays_control_without_replaying_capture_prefix() {
        let context = &mut Context::default();
        let saved = capture_multi(
            context,
            r#"
            let visits = 0;
            function marker() { return 0; }
            function inner() { visits++; return 2 * marker(); }
            function thunk() { return 1 + inner(); }
            10 + thunk();
        "#,
        );
        assert_eq!(saved.snapshot.frames.len(), 2);
        for (input, expected) in [(3, 7), (4, 9), (3, 7)] {
            context.vm.stack.push(123);
            assert_eq!(
                saved.resume(input.into(), context).unwrap(),
                expected.into()
            );
            assert_eq!(context.vm.stack.stack, vec![JsValue::from(123)]);
            context.vm.stack.pop();
        }
        assert_eq!(
            context.eval(Source::from_bytes("visits")).unwrap(),
            1.into()
        );
    }

    #[test]
    fn multi_shot_characterizes_local_binding_storage_mismatch() {
        // These programs differ only by an unused reader closure. The proposed
        // shared-store semantics requires [1, 2] for BOTH. Record the naive
        // prototype's counterexample explicitly instead of claiming conformance.
        for (reader, expected) in [("", [1, 1]), ("const read = () => count;", [1, 2])] {
            let context = &mut Context::default();
            let source = format!(
                r#"
                function marker() {{ return 0; }}
                function thunk() {{
                    let count = 0;
                    {reader}
                    marker();
                    count++;
                    return count;
                }}
                10 + thunk();
            "#
            );
            let saved = capture_multi(context, &source);
            for value in expected {
                assert_eq!(
                    saved.resume(JsValue::undefined(), context).unwrap(),
                    value.into()
                );
            }
        }
    }

    #[test]
    fn multi_shot_shares_existing_heap_but_allocates_fresh_objects_after_resume() {
        let context = &mut Context::default();
        let saved = boa_gc::Gc::new(capture_multi(
            context,
            r#"
            let previous;
            function marker() { return 0; }
            function thunk() {
                const box = { count: 0 };
                marker();
                box.count++;
                const fresh = {};
                if (fresh === previous) throw 99;
                previous = fresh;
                return box.count;
            }
            10 + thunk();
        "#,
        ));
        for expected in 1..=3 {
            boa_gc::force_collect();
            assert_eq!(
                saved.resume(JsValue::undefined(), context).unwrap(),
                expected.into()
            );
            assert_eq!(context.vm.frames.len(), 1);
            assert!(context.vm.stack.stack.is_empty());
        }
    }

    #[test]
    fn multi_shot_can_resume_again_after_a_throw_or_rejected_limit() {
        let context = &mut Context::default();
        let saved = capture_multi(
            context,
            r#"
            function marker() { return 0; }
            function inner() { const x = marker(); if (x < 0) throw 17; return x * 2; }
            function thunk() { return 1 + inner(); }
            10 + thunk();
        "#,
        );
        let limits = context.vm.runtime_limits;
        context.vm.runtime_limits.set_stack_size_limit(0);
        assert!(saved.resume(1.into(), context).is_err());
        context.vm.runtime_limits = limits;
        let error = saved.resume((-1).into(), context).unwrap_err();
        assert_eq!(error.into_opaque(context).unwrap(), 17.into());
        assert_eq!(saved.resume(5.into(), context).unwrap(), 11.into());
        assert_eq!(context.vm.frames.len(), 1);
        assert!(context.vm.pending_exception.is_none());
        assert!(context.vm.stack.stack.is_empty());
        assert_eq!(
            context.eval(Source::from_bytes("6 * 7")).unwrap(),
            42.into()
        );
    }

    #[test]
    fn moves_multiple_frames_and_resumes_once_after_caller_finishes() {
        let context = &mut Context::default();
        let prompt = pause(
            context,
            r#"
            let visits = 0;
            function marker() { return 0; }
            function inner() { visits++; return 2 * marker(); }
            function thunk() { return 1 + inner(); }
            10 + thunk();
        "#,
        );
        let mut continuation = prompt.capture(&mut context.vm).unwrap();
        assert_eq!(continuation.suspended.as_ref().unwrap().frames.len(), 2);
        finish_caller(context);
        boa_gc::force_collect();

        // A different stack base proves fp/rp relocation, rather than restoration
        // to accidentally identical absolute offsets. Keep caller VM state intact.
        context.vm.stack.push(123);
        context.vm.return_value = 456.into();
        assert_eq!(continuation.resume(3.into(), context).unwrap(), 7.into());
        assert_eq!(context.vm.stack.stack, vec![JsValue::from(123)]);
        assert_eq!(context.vm.return_value, JsValue::from(456));
        assert!(
            continuation
                .resume(4.into(), context)
                .unwrap_err()
                .as_native()
                .unwrap()
                .is_type()
        );
        context.vm.stack.pop();
        assert_eq!(
            context.eval(Source::from_bytes("visits")).unwrap(),
            1.into()
        );
        assert_eq!(
            context.eval(Source::from_bytes("40 + 2")).unwrap(),
            42.into()
        );
    }

    #[test]
    fn resumed_throw_consumes_segment_and_restores_caller() {
        let context = &mut Context::default();
        let prompt = pause(
            context,
            r#"
            function marker() { return 0; }
            function inner() { marker(); throw 17; }
            function thunk() { return inner(); }
            10 + thunk();
        "#,
        );
        let mut continuation = prompt.capture(&mut context.vm).unwrap();
        finish_caller(context);
        let error = continuation.resume(3.into(), context).unwrap_err();
        assert_eq!(error.into_opaque(context).unwrap(), JsValue::from(17));
        assert!(continuation.resume(3.into(), context).is_err());
        assert_eq!(context.vm.frames.len(), 1);
        assert!(context.vm.stack.stack.is_empty());
        assert!(context.vm.pending_exception.is_none());
        assert_eq!(
            context.eval(Source::from_bytes("6 * 7")).unwrap(),
            42.into()
        );
    }

    #[test]
    fn rejected_boundary_does_not_modify_running_frames() {
        let context = &mut Context::default();
        let prompt = pause(
            context,
            r#"
            function marker() { return 0; }
            function thunk() { return 1 + marker(); }
            10 + thunk();
        "#,
        );
        let frames = context.vm.frames.len();
        let stack = context.vm.stack.stack.clone();
        context.vm.host_call_depth += 1;
        assert!(prompt.capture(&mut context.vm).is_err());
        assert_eq!(context.vm.frames.len(), frames);
        assert_eq!(context.vm.stack.stack, stack);
        context.vm.host_call_depth -= 1;
        assert_eq!(context.run().consume().unwrap(), 11.into());
        context.vm.pop_frame();
        assert_eq!(context.eval(Source::from_bytes("1 + 1")).unwrap(), 2.into());
    }

    #[test]
    fn rejects_finally_without_running_cleanup_early() {
        let context = &mut Context::default();
        let prompt = pause(
            context,
            r#"
            let cleanup = 0;
            function marker() { return 0; }
            function thunk() { try { return marker(); } finally { cleanup++; } }
            10 + thunk();
        "#,
        );
        assert!(prompt.capture(&mut context.vm).is_err());
        assert_eq!(context.run().consume().unwrap(), 10.into());
        context.vm.pop_frame();
        assert_eq!(
            context.eval(Source::from_bytes("cleanup")).unwrap(),
            1.into()
        );
    }

    #[test]
    fn gc_preserves_saved_closure_and_resume_restores_a_live_caller() {
        let context = &mut Context::default();
        let prompt = pause(
            context,
            r#"
            function marker() { return 0; }
            function thunk() {
                const box = { value: 40 };
                const read = () => box.value;
                return marker() + read();
            }
            10 + thunk();
        "#,
        );
        let saved = boa_gc::Gc::new(boa_gc::GcRefCell::new(
            prompt.capture(&mut context.vm).unwrap(),
        ));
        let caller_frames = context.vm.frames.len();
        let caller_stack = context.vm.stack.stack.clone();
        boa_gc::force_collect();
        assert_eq!(
            saved.borrow_mut().resume(2.into(), context).unwrap(),
            42.into()
        );
        assert_eq!(context.vm.frames.len(), caller_frames);
        assert_eq!(context.vm.stack.stack, caller_stack);
        finish_caller(context);
    }

    #[test]
    fn rejected_resume_limit_does_not_consume_the_segment() {
        let context = &mut Context::default();
        let prompt = pause(
            context,
            r#"
            function marker() { return 0; }
            function thunk() { return 1 + marker(); }
            10 + thunk();
        "#,
        );
        let mut continuation = prompt.capture(&mut context.vm).unwrap();
        finish_caller(context);
        let limits = context.vm.runtime_limits;
        context.vm.runtime_limits.set_stack_size_limit(0);
        assert!(continuation.resume(2.into(), context).is_err());
        assert!(continuation.suspended.is_some());
        assert_eq!(context.vm.frames.len(), 1);
        assert!(context.vm.stack.stack.is_empty());
        context.vm.runtime_limits = limits;
        assert_eq!(continuation.resume(2.into(), context).unwrap(), 3.into());
    }

    #[test]
    fn dropping_segment_does_not_run_the_rest_of_the_computation() {
        let context = &mut Context::default();
        let prompt = pause(
            context,
            r#"
            let effects = 0;
            function marker() { return 0; }
            function thunk() { marker(); effects++; return effects; }
            10 + thunk();
        "#,
        );
        let continuation = prompt.capture(&mut context.vm).unwrap();
        finish_caller(context);
        drop(continuation);
        boa_gc::force_collect();
        assert_eq!(
            context.eval(Source::from_bytes("effects")).unwrap(),
            0.into()
        );
    }
}
