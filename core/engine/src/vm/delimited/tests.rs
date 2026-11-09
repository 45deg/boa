//! End-to-end JS call tests: imported `QuickJS` semantics plus Boa dispatch,
//! native reentry, registration, GC, and error-recovery cases. No marker driver.
use crate::{Context, JsValue, NativeFunction, Source, js_string};

fn context() -> Context {
    let mut context = Context::default();
    context.enable_delimited_continuations().unwrap();
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

fn assert_clean(context: &mut Context) {
    assert!(context.vm.prompts.is_empty());
    assert_eq!(context.vm.frames.len(), 1);
    assert!(context.vm.stack.stack.is_empty());
    assert!(context.vm.pending_exception.is_none());
    assert_eq!(eval(context, "40 + 2"), 42.into());
}

#[test]
fn quickjs_one_shot_semantics_through_js_calls() {
    let context = &mut context();
    assert_eq!(
        eval(context, include_str!("quickjs_semantics.js")),
        42.into()
    );
    assert_clean(context);
}

#[test]
fn identities_spread_and_shadowing() {
    let context = &mut context();
    assert_eq!(
        eval(
            context,
            r#"
        const r = reset, s = shift;
        const ops = { r, s };
        const a = ops.r(...[() => 1 + ops.s(...[k => k(2)])]);
        const b = (shift => shift(2))(x => x + 10);
        let effect = 0;
        const c = reset(() => shift(k => k(4), effect++));
        a + b + c + effect;
    "#
        ),
        20.into()
    );
    assert_clean(context);
}

#[test]
fn native_boundaries_reject_before_capture_and_recover_inside_reset() {
    let context = &mut context();
    for expression in [
        "[1].map(() => shift(k => k))",
        "({ get x() { return shift(k => k); } }).x",
        "({ set x(v) { shift(k => k); } }).x = 1",
        "+({ valueOf() { return shift(k => k); } })",
        "new (function C() { shift(k => k); })()",
        "new Proxy(() => shift(k => k), { apply(f,t,a) { return f(); } })()",
        "eval('shift(k => k)')",
        "shift.call(null, k => k)",
        "shift.apply(null, [k => k])",
        "Reflect.apply(shift, null, [k => k])",
        "shift.bind(null)(k => k)",
    ] {
        let source = format!(
            r#"
            reset(() => {{
                let rejected = false;
                try {{ {expression}; }} catch(e) {{
                    if (!(e instanceof TypeError)) throw e;
                    rejected = true;
                }}
                if (!rejected) throw Error('boundary was not rejected');
                return 1 + shift(k => k(4));
            }});
        "#
        );
        assert_eq!(eval(context, &source), 5.into(), "{expression}");
        assert_clean(context);
    }
}

#[test]
fn fresh_prompts_inside_native_and_implicit_callbacks() {
    let context = &mut context();
    assert_eq!(
        eval(
            context,
            r#"
        const a = reset(() => [1].map(x => reset(() => x + shift(k => k(2))))[0]);
        const b = reset(() => ({ get x() { return reset(() => 2 + shift(k => k(3))); } }).x);
        a + b;
    "#
        ),
        8.into()
    );
    assert_clean(context);
}

#[test]
fn resumed_exception_and_handler_exception_have_distinct_handlers() {
    let context = &mut context();
    assert_eq!(
        eval(
            context,
            r#"
        let innerCaught = false, outerCaught = false;
        try {
            reset(() => { try { shift(k => { throw 17; }); }
                          catch(e) { innerCaught = true; } });
        } catch(e) { outerCaught = e === 17; }
        let k = reset(() => { shift(k => k); throw 23; });
        let thrown = false, consumed = false;
        try { k(); } catch(e) { thrown = e === 23; }
        try { k(); } catch(e) { consumed = e instanceof TypeError; }
        !innerCaught && outerCaught && thrown && consumed;
    "#
        ),
        true.into()
    );
    assert_clean(context);
}

#[test]
fn capture_in_finally_preserves_the_pending_return() {
    let context = &mut context();
    assert_eq!(
        eval(
            context,
            r#"
        let log = '';
        const k = reset(() => {
            try { shift(k => k); return 4; }
            finally { log += 'a'; shift(k => k); log += 'b'; }
        });
        const next = k();
        if (log !== 'a') throw Error(log);
        const result = next();
        result === 4 && log === 'ab';
    "#
        ),
        true.into()
    );
    assert_clean(context);
}

#[test]
fn opt_in_registration_and_validation() {
    let mut disabled = Context::default();
    assert_eq!(
        eval(&mut disabled, "typeof reset"),
        js_string!("undefined").into()
    );
    eval(&mut disabled, "globalThis.shift = 9");
    assert!(disabled.enable_delimited_continuations().is_err());
    assert_eq!(
        eval(&mut disabled, "typeof reset"),
        js_string!("undefined").into()
    );
    let context = &mut context();
    for expression in [
        "reset()",
        "shift()",
        "reset(1)",
        "reset(async () => 1)",
        "reset(function* () {})",
        "reset(Math.abs)",
        "reset((() => 1).bind(null))",
        "reset(new Proxy(() => 1, {}))",
        "new reset(() => 1)",
        "new shift(k => k)",
    ] {
        let source = format!(
            "(() => {{ try {{ {expression}; }} catch(e) {{ return e instanceof TypeError; }} return false; }})()"
        );
        assert_eq!(eval(context, &source), true.into());
        assert_clean(context);
    }
    assert_eq!(
        eval(
            context,
            "reset.length === 1 && shift.length === 1 && !Object.keys(globalThis).includes('reset')"
        ),
        true.into()
    );
}

#[test]
fn custom_host_callback_boundary_and_a_fresh_inner_reset() {
    let context = &mut context();
    context
        .register_global_builtin_callable(
            js_string!("bridge"),
            1,
            NativeFunction::from_fn_ptr(|_, args, context| {
                args[0]
                    .as_object()
                    .unwrap()
                    .call(&JsValue::undefined(), &[], context)
            }),
        )
        .unwrap();
    assert_eq!(
        eval(
            context,
            r#"
        reset(() => {
            let rejected = false;
            try { bridge(() => shift(k => k)); }
            catch(e) { rejected = e instanceof TypeError; }
            if (!rejected) throw Error('native capture escaped');
            return bridge(() => reset(() => 2 + shift(k => k(3))));
        });
    "#
        ),
        5.into()
    );
    assert_clean(context);
}

#[test]
fn escaped_continuation_survives_later_scripts_and_rejects_other_contexts() {
    let context = &mut context();
    eval(
        context,
        "globalThis.saved = reset(() => 10 + shift(k => k));",
    );
    boa_gc::force_collect();
    let saved = context
        .global_object()
        .get(js_string!("saved"), context)
        .unwrap();
    let mut other = Context::default();
    other
        .register_global_property(
            js_string!("saved"),
            saved,
            crate::property::Attribute::all(),
        )
        .unwrap();
    assert_eq!(
        eval(
            &mut other,
            "try { saved(2); false; } catch(e) { e instanceof TypeError; }"
        ),
        true.into()
    );
    assert_clean(&mut other);
    assert_eq!(eval(context, "saved(2)"), 12.into());
    assert_eq!(
        eval(
            context,
            "try { saved(3); false; } catch(e) { e instanceof TypeError; }"
        ),
        true.into()
    );
    assert_clean(context);
}

#[test]
fn control_functions_are_not_identified_by_the_identifier_eval() {
    let context = &mut context();
    assert_eq!(
        eval(
            context,
            r#"
        (function(eval) { return reset(() => 1 + eval(k => k(2))); })(shift);
    "#
        ),
        3.into()
    );
    assert_eq!(
        eval(
            context,
            r#"
        (function(eval) { return reset(() => 1 + eval(...[k => k(2)])); })(shift);
    "#
        ),
        3.into()
    );
    assert_clean(context);
}

#[test]
fn failed_resume_validation_does_not_consume_and_active_iterators_are_rejected() {
    let context = &mut context();
    context
        .register_global_builtin_callable(
            js_string!("stackLimit"),
            1,
            NativeFunction::from_fn_ptr(|_, args, context| {
                let limit = args[0].to_u32(context)?;
                let limit = if limit == 0 {
                    context.vm.stack.stack.len() + 16
                } else {
                    limit as usize
                };
                context.runtime_limits_mut().set_stack_size_limit(limit);
                Ok(JsValue::undefined())
            }),
        )
        .unwrap();
    assert_eq!(
        eval(
            context,
            r#"
        const k = reset(() => {
            function deep(n) { if (n === 0) return shift(k => k); return 1 + deep(n - 1); }
            return deep(20);
        });
        let rejected = false;
        stackLimit(0);
        try { k(2); } catch(e) { rejected = e instanceof TypeError; }
        stackLimit(100000);
        rejected && k(2) === 22;
    "#
        ),
        true.into()
    );
    assert_clean(context);
    assert_eq!(
        eval(
            context,
            r#"
        reset(() => {
            let rejected = false;
            try { for (const x of [1]) { shift(k => k); } }
            catch(e) { rejected = e instanceof TypeError; }
            return rejected && shift(k => k(7)) === 7;
        });
    "#
        ),
        true.into()
    );
    assert_clean(context);
}
