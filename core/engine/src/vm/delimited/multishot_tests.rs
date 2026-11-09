//! Public multi-shot JS tests: shared storage, reentry/recapture, provenance
//! rejection and errors. Optimizer/strict matrices exercise compiler storage;
//! boundary and limit tests check rejection followed by reuse and VM cleanup.
//! Every continuation is obtained through real shift/reset.
use crate::{Context, JsValue, NativeFunction, Script, Source, js_string};

fn context() -> Context {
    let mut context = Context::default();
    context.enable_multishot_delimited_continuations().unwrap();
    context
        .register_global_builtin_callable(
            js_string!("gc"),
            0,
            NativeFunction::from_fn_ptr(|_, _, _| {
                boa_gc::force_collect();
                Ok(JsValue::undefined())
            }),
        )
        .unwrap();
    context
}

fn eval(context: &mut Context, source: &str) -> JsValue {
    context
        .eval(Source::from_bytes(source))
        .unwrap_or_else(|e| panic!("{e}\n{source}"))
}

fn clean(context: &mut Context) {
    assert!(context.vm.prompts.is_empty());
    assert_eq!(context.vm.frames.len(), 1);
    assert!(context.vm.stack.stack.is_empty());
    assert!(context.vm.pending_exception.is_none());
    assert_eq!(eval(context, "40+2"), 42.into());
}

#[test]
fn composition_and_saved_continuations_can_run_many_times() {
    let c = &mut context();
    assert_eq!(
        eval(
            c,
            r#"
        reset(() => 1 + shift(k => k(2) + k(3)));
    "#
        ),
        7.into()
    );
    eval(c, "globalThis.k = reset(() => 10 + shift(k => k));");
    boa_gc::force_collect();
    assert_eq!(eval(c, "k(1) + k(2) + k(3)"), 36.into());
    clean(c);
}

#[test]
fn closure_presence_does_not_change_shared_local_or_parameter_updates() {
    for (parameters, declaration, call) in [
        ("", "let count = 0;", ""),
        ("", "var count = 0;", ""),
        ("count", "", "0"),
        ("count = 0", "", ""),
    ] {
        for reader in ["", "const read = () => count;"] {
            let c = &mut context();
            let source = format!(
                r#"
                function inner({parameters}) {{
                    {declaration} {reader}
                    shift(k => k);
                    return ++count;
                }}
                const k = reset(() => inner({call}));
                const a = k(); gc(); const b = k(); const d = k();
                a === 1 && b === 2 && d === 3;
            "#
            );
            assert_eq!(eval(c, &source), true.into());
            clean(c);
        }
    }
    for reader in ["", "const read = () => count;"] {
        let c = &mut context();
        let source = format!(
            r#"
            const k = reset(() => {{
                for (let count = 0; count < 5; count++) {{
                    {reader} shift(k => k); return ++count;
                }}
            }});
            k() === 1 && k() === 2 && k() === 3;
        "#
        );
        assert_eq!(eval(c, &source), true.into());
        clean(c);
    }
}

#[test]
fn external_closure_writes_heap_effects_and_fresh_scopes() {
    let c = &mut context();
    assert_eq!(
        eval(
            c,
            r#"
        let write, before = 0, after = 0, previous;
        const k = reset(() => {
            let count = 0; const box = { n: 0 };
            write = v => { count = v; };
            before++;
            shift(k => k);
            after++; box.n++;
            { let fresh = 0; const object = {};
              if (object === previous) throw Error('reused fresh allocation');
              previous = object;
              return ++count * 100 + box.n * 10 + ++fresh; }
        });
        const a = k(); write(8); gc(); const b = k(); const d = k();
        a === 111 && b === 921 && d === 1031 && before === 1 && after === 3;
    "#
        ),
        true.into()
    );
    clean(c);
}

#[test]
fn handler_shift_and_recapture_keep_outer_frames_and_original_snapshot() {
    let c = &mut context();
    assert_eq!(
        eval(
            c,
            r#"
        const h = reset(() => 1 + shift(k => 2 + shift(j => j(7) + j(8))));
        const first = reset(() => {
            function inner() { return shift(k => k) + shift(k => k); }
            return 10 + inner();
        });
        const a = first(2), b = first(5); gc();
        h === 19 && a(3) === 15 && b(4) === 19 && a(9) === 21 && first(1)(2) === 13;
    "#
        ),
        true.into()
    );
    clean(c);
}

#[test]
fn reentrant_resume_uses_independent_registers_with_shared_cells() {
    let c = &mut context();
    assert_eq!(
        eval(
            c,
            r#"
        let k = reset(() => {
            const x = shift(k => k);
            if (x === 0) return 1;
            return x + k(0);
        });
        k(2) === 3 && k(3) === 4 && k(0) === 1;
    "#
        ),
        true.into()
    );
    clean(c);
}

#[test]
fn throw_finally_and_catch_recapture_do_not_consume_multishot() {
    let c = &mut context();
    assert_eq!(
        eval(
            c,
            r#"
        let cleanup = 0, thrown = false;
        const k = reset(() => {
            try { const x = shift(k => k); if (x < 0) throw 17; return x * 2; }
            finally { cleanup++; }
        });
        try { k(-1); } catch(e) { thrown = e === 17; }
        const value = k(3);
        const caught = reset(() => {
            try { shift(k => k); throw 7; }
            catch(e) { return e + shift(k => k); }
        });
        const a = caught(), b = caught();
        thrown && value === 6 && cleanup === 2 && a(1) === 8 && b(2) === 9 && a(3) === 10;
    "#
        ),
        true.into()
    );
    clean(c);
}

#[test]
fn mapped_arguments_remain_shared_and_const_tdz_are_preserved() {
    let c = &mut context();
    assert_eq!(
        eval(
            c,
            r#"
        function f(a) {
            const args = arguments;
            const v = shift(k => k);
            args[0] += v;
            return a;
        }
        const k = reset(() => f(10));
        let constant = false, tdz = false;
        try { const x = 1; x = 2; } catch(e) { constant = e instanceof TypeError; }
        try { x; let x = 1; } catch(e) { tdz = e instanceof ReferenceError; }
        k(2) === 12 && k(3) === 15 && constant && tdz;
    "#
        ),
        true.into()
    );
    clean(c);
}

#[test]
fn unshared_code_cannot_be_captured_but_rejection_leaves_prompt_usable() {
    let c = &mut Context::default();
    eval(
        c,
        "function old() { let x = 0; shift(k => k); return ++x; }",
    );
    let old_script = Script::parse(
        Source::from_bytes("reset(() => { let x = 0; shift(k => k); return ++x; });"),
        None,
        c,
    )
    .unwrap();
    c.enable_multishot_delimited_continuations().unwrap();
    assert!(old_script.evaluate(c).is_err());
    // The rejected script has unwound; subsequent scripts and captures still work.
    clean(c);
    for expression in [
        "old()",
        "Function('return shift(k => k)')()",
        "eval('(function() { return shift(k => k); })')()",
    ] {
        let source = format!(
            r#"
            reset(() => {{
                let rejected = false;
                try {{ {expression}; }} catch(e) {{ rejected = e instanceof TypeError; }}
                if (!rejected) throw Error('unshared frame captured');
                return shift(k => k(2) + k(3));
            }});
        "#
        );
        assert_eq!(eval(c, &source), 5.into());
    }
    clean(c);
}

#[test]
fn real_native_boundaries_and_inner_prompts_still_apply() {
    let c = &mut context();
    assert_eq!(
        eval(
            c,
            r#"
        let rejected = false;
        try { reset(() => [1].map(() => shift(k => k))); }
        catch(e) { rejected = e instanceof TypeError; }
        const result = [1].map(x => reset(() => x + shift(k => k(2) + k(3))))[0];
        rejected && result === 7;
    "#
        ),
        true.into()
    );
    clean(c);
}

#[test]
fn uncaught_resume_throw_restores_host_entry_and_keeps_snapshot() {
    let c = &mut context();
    eval(
        c,
        "globalThis.k = reset(() => { const x = shift(k => k); if (x < 0) throw 17; return x + 1; });",
    );
    let error = c.eval(Source::from_bytes("k(-1)")).unwrap_err();
    assert_eq!(error.into_opaque(c).unwrap(), 17.into());
    clean(c);
    assert_eq!(eval(c, "k(2) + k(3)"), 7.into());
    clean(c);
}

#[test]
fn existing_one_shot_continuations_keep_their_consumption_contract() {
    let c = &mut Context::default();
    c.enable_delimited_continuations().unwrap();
    eval(
        c,
        r#"
        globalThis.once = reset(() => 1 + shift(k => k));
        delete globalThis.shift; delete globalThis.reset;
    "#,
    );
    c.enable_multishot_delimited_continuations().unwrap();
    assert_eq!(
        eval(
            c,
            r#"
        const many = reset(() => 10 + shift(k => k));
        const first = once(2);
        let consumed = false;
        try { once(3); } catch(e) { consumed = e instanceof TypeError; }
        first === 3 && consumed && many(1) === 11 && many(2) === 12;
    "#
        ),
        true.into()
    );
    clean(c);
}

#[test]
fn module_code_without_shared_provenance_is_rejected() {
    let c = &mut context();
    // Module rejection already retains job-related stack values without this
    // feature. Compare with that baseline rather than claiming an empty stack.
    let baseline_context = &mut Context::default();
    let baseline = crate::Module::parse(
        Source::from_bytes("throw new TypeError('baseline');"),
        None,
        baseline_context,
    )
    .unwrap();
    let baseline_promise = baseline.load_link_evaluate(baseline_context);
    baseline_context.run_jobs().unwrap();
    assert!(matches!(
        baseline_promise.state(),
        crate::builtins::promise::PromiseState::Rejected(_)
    ));
    let baseline_stack_len = baseline_context.vm.stack.stack.len();
    let module = crate::Module::parse(
        Source::from_bytes(
            "export const result = reset(() => { let x = 0; shift(k => k); return ++x; });",
        ),
        None,
        c,
    )
    .unwrap();
    let promise = module.load_link_evaluate(c);
    c.run_jobs().unwrap();
    let crate::builtins::promise::PromiseState::Rejected(reason) = promise.state() else {
        panic!("unshared module capture must reject");
    };
    c.register_global_property(
        js_string!("failure"),
        reason,
        crate::property::Attribute::all(),
    )
    .unwrap();
    assert_eq!(eval(c, "failure instanceof TypeError"), true.into());
    assert_eq!(c.vm.stack.stack.len(), baseline_stack_len);
    assert!(c.vm.prompts.is_empty());
    assert_eq!(c.vm.frames.len(), 1);
    assert!(c.vm.pending_exception.is_none());
    assert_eq!(eval(c, "40+2"), 42.into());
}

#[test]
fn failed_registration_does_not_enable_shared_compilation() {
    let c = &mut Context::default();
    eval(c, "globalThis.shift = 1;");
    assert!(c.enable_multishot_delimited_continuations().is_err());
    assert!(!c.vm.shared_continuation_bindings);
    assert_eq!(eval(c, "typeof reset"), js_string!("undefined").into());
    clean(c);
}

#[test]
fn shared_bindings_agree_with_and_without_ast_optimization() {
    use crate::optimizer::OptimizerOptions;
    for options in [OptimizerOptions::empty(), OptimizerOptions::OPTIMIZE_ALL] {
        for strict in ["", "'use strict';"] {
            for reader in ["", "const read = () => count;"] {
                let c = &mut context();
                c.set_optimizer_options(options);
                let source = format!(
                    r#"{strict}
                    function f({{ count = 0 }} = {{}}) {{
                        {reader}
                        {{ let count = 100; if (count !== 100) throw Error('shadow'); }}
                        shift(k => k);
                        return ++count;
                    }}
                    const k = reset(() => f());
                    k() === 1 && k() === 2 && k() === 3;
                    "#
                );
                assert_eq!(eval(c, &source), true.into(), "{source}");
                clean(c);
            }
        }
    }
}

#[test]
fn nested_prompts_delimit_inner_captures_and_survive_inner_throws() {
    let c = &mut context();
    assert_eq!(
        eval(
            c,
            r#"
        const outer = reset(() => {
            const inner = reset(() => 100 + shift(k => k));
            let caught = false;
            try { reset(() => { throw 17; }); } catch (e) { caught = e === 17; }
            if (!caught) throw Error('missing inner error');
            return inner(1) + inner(2) + shift(k => k);
        });
        outer(3) === 206 && outer(4) === 207;
    "#
        ),
        true.into()
    );
    clean(c);
}

#[test]
fn capture_in_finally_copies_pending_return_without_replaying_prefix() {
    let c = &mut context();
    assert_eq!(
        eval(
            c,
            r#"
        let prefix = 0, suffix = 0;
        const first = reset(() => {
            try { return shift(k => k); }
            finally { prefix++; shift(k => k); suffix++; }
        });
        const a = first(4), b = first(9);
        const before = prefix === 2 && suffix === 0;
        before && a() === 4 && b() === 9 && a() === 4 &&
            prefix === 2 && suffix === 3;
    "#
        ),
        true.into()
    );
    clean(c);
}

#[test]
fn stack_and_recursion_limit_rejections_preserve_snapshot() {
    let c = &mut context();
    eval(
        c,
        r#"
        globalThis.saved = reset(() => {
            function deep(n) { return n === 0 ? shift(k => k) : 1 + deep(n - 1); }
            return deep(20);
        });
    "#,
    );
    let original = c.runtime_limits();
    for stack_limit in [true, false] {
        if stack_limit {
            c.runtime_limits_mut().set_stack_size_limit(64);
        } else {
            c.runtime_limits_mut().set_recursion_limit(10);
        }
        let error = c.eval(Source::from_bytes("saved(2)")).unwrap_err();
        *c.runtime_limits_mut() = original;
        assert!(error.as_native().is_some_and(crate::JsNativeError::is_type));
        clean(c);
        assert_eq!(eval(c, "saved(2) + saved(3)"), 45.into());
        clean(c);
    }
}

#[test]
fn unsupported_boundaries_recover_before_a_repeated_capture() {
    for expression in [
        "({ get x() { return shift(k => k); } }).x",
        "({ set x(v) { shift(k => k); } }).x = 1",
        "+({ valueOf() { return shift(k => k); } })",
        "new (function C() { shift(k => k); })()",
        "new Proxy(() => shift(k => k), { apply(f) { return f(); } })()",
        "shift.call(null, k => k)",
        "shift.apply(null, [k => k])",
        "shift.bind(null)(k => k)",
        "for (const x of [1]) { shift(k => k); }",
    ] {
        let c = &mut context();
        let source = format!(
            r#"
            reset(() => {{
                let rejected = false;
                try {{ {expression}; }} catch(e) {{ rejected = e instanceof TypeError; }}
                if (!rejected) throw Error('boundary accepted');
                return 1 + shift(k => k(2) + k(3));
            }});
        "#
        );
        assert_eq!(eval(c, &source), 7.into(), "{expression}");
        clean(c);
    }
}

#[test]
fn spread_aliases_and_eval_named_dispatch_resume_repeatedly() {
    let c = &mut context();
    assert_eq!(
        eval(
            c,
            r#"
        const ops = { r: reset, s: shift };
        const k = ops.r(...[() => 10 + ops.s(...[k => k])]);
        function invoke(eval, value) { return eval(...[value]); }
        k(...[1]) === 11 && invoke(k, 2) === 12 && k(3) === 13;
    "#
        ),
        true.into()
    );
    clean(c);
}

#[test]
fn foreign_context_and_native_resume_fail_without_changing_snapshot() {
    let c = &mut context();
    let saved = eval(c, "reset(() => 10 + shift(k => k))");
    let other = &mut context();
    other
        .register_global_property(
            js_string!("saved"),
            saved.clone(),
            crate::property::Attribute::all(),
        )
        .unwrap();
    assert_eq!(
        eval(
            other,
            "try { saved(1); false; } catch(e) { e instanceof TypeError; }"
        ),
        true.into()
    );
    clean(other);
    assert!(
        saved
            .as_object()
            .unwrap()
            .call(&JsValue::undefined(), &[1.into()], c)
            .is_err()
    );
    clean(c);
    c.register_global_property(
        js_string!("saved"),
        saved,
        crate::property::Attribute::all(),
    )
    .unwrap();
    assert_eq!(eval(c, "saved(2) + saved(3)"), 25.into());
    clean(c);
}
