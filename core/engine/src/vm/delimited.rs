//! Opt-in one-shot and multi-shot shift/reset using bytecode call dispatch.
//! Multi-shot clones control state but shares binding cells; Script compilation
//! records shared-storage provenance checked before multi-shot capture.
//!
//! `call` identifies control functions by object data. `capture` moves the suffix
//! of live frames into a traced segment and dispatches the handler under the same
//! prompt. `resume` installs the entire segment under a fresh prompt, so a later
//! capture includes all still-pending outer frames. No Rust interpreter recursion
//! or exception sentinel implements these transfers. `Vm::pop_frame` removes a
//! prompt when its bottom frame returns or unwinds.

use super::{CallFrame, CallFrameFlags, CodeBlock, CodeBlockFlags, Constant, Stack};
use crate::{
    Context, JsNativeError, JsObject, JsResult, JsValue,
    builtins::function::OrdinaryFunction,
    js_string,
    object::{
        JsData,
        internal_methods::{
            CallValue, InternalMethodCallContext, InternalObjectMethods, ORDINARY_INTERNAL_METHODS,
        },
    },
    property::Attribute,
    realm::Realm,
};
use boa_gc::{Finalize, Gc, GcRefCell, Trace};

#[derive(Debug)]
pub(crate) struct Prompt {
    pub(crate) frame_index: usize,
    host_depth: usize,
    multi_shot: bool,
}

#[derive(Clone, Debug, Trace, Finalize)]
struct Segment {
    frames: Vec<CallFrame>,
    stack: Stack,
    return_value: JsValue,
}

#[derive(Clone, Debug, Trace, Finalize)]
enum Kind {
    Reset,
    ResetMultiShot,
    Shift,
    Resume {
        snapshot: Gc<GcRefCell<Option<Segment>>>,
        multi_shot: bool,
    },
}

#[derive(Debug, Trace, Finalize)]
struct ControlFunction {
    kind: Kind,
    owner: Gc<()>,
    realm: Realm,
}

impl JsData for ControlFunction {
    fn internal_methods(&self) -> &'static InternalObjectMethods {
        static METHODS: InternalObjectMethods = InternalObjectMethods {
            __call__: unsupported_call,
            ..ORDINARY_INTERNAL_METHODS
        };
        &METHODS
    }
}

fn error(message: &'static str) -> crate::JsError {
    JsNativeError::typ().with_message(message).into()
}

fn unsupported_call(
    _object: &JsObject,
    argument_count: usize,
    context: &mut InternalMethodCallContext<'_>,
) -> JsResult<CallValue> {
    pop_call(argument_count, context);
    Err(error(
        "continuation API requires an explicit JavaScript call boundary",
    ))
}

fn pop_call(argument_count: usize, context: &mut Context) -> Vec<JsValue> {
    let args = context
        .vm
        .stack
        .calling_convention_pop_arguments(argument_count);
    context.vm.stack.pop(); // function
    context.vm.stack.pop(); // this
    args
}

fn function(kind: Kind, context: &mut Context) -> JsResult<JsObject> {
    let name = match &kind {
        Kind::Reset | Kind::ResetMultiShot => "reset",
        Kind::Shift => "shift",
        Kind::Resume { .. } => "continuation",
    };
    let object = JsObject::from_proto_and_data_with_shared_shape(
        context.root_shape(),
        context.intrinsics().constructors().function().prototype(),
        ControlFunction {
            kind,
            owner: context.vm.continuation_owner.clone(),
            realm: context.realm().clone(),
        },
    )
    .upcast();
    object.define_property_or_throw(
        js_string!("name"),
        crate::property::PropertyDescriptor::builder()
            .value(crate::JsString::from(name))
            .configurable(true),
        context,
    )?;
    object.define_property_or_throw(
        js_string!("length"),
        crate::property::PropertyDescriptor::builder()
            .value(1)
            .configurable(true),
        context,
    )?;
    Ok(object)
}

impl Context {
    /// Install experimental one-shot `reset(thunk)` and `shift(handler)` globals.
    ///
    /// The functions work from explicit synchronous JS calls (including spread).
    /// Capture cannot cross native, implicit-call, constructor, async/generator,
    /// or active iterator boundaries. Callbacks must be ordinary synchronous JS
    /// functions in this realm. Native `.call`/`.apply`, bound/Proxy wrappers of
    /// these control functions and direct Rust `JsObject::call` are unsupported.
    /// Capturing does not run finally; abandoning a continuation never runs JS.
    ///
    /// # Errors
    /// Returns an error if either global name already exists or registration fails.
    pub fn enable_delimited_continuations(&mut self) -> JsResult<()> {
        self.install_delimited_continuations(false)
    }

    /// Install experimental multi-shot `reset`/`shift` and enable shared binding
    /// storage for subsequently parsed Scripts. Each resume copies control state;
    /// existing binding cells, objects and side effects remain shared. Recursive
    /// resumption is allowed, subject to the usual VM limits.
    ///
    /// Enable before parsing the scripts whose functions will be captured.
    /// Capture rejects code compiled without shared-binding provenance, including
    /// older Scripts, modules, eval and Function-constructor code. These compilation
    /// paths are not yet supported for multi-shot capture. Other one-shot boundary
    /// restrictions also apply. This does not change previously created continuations.
    ///
    /// # Errors
    /// Returns an error on registration collisions or failure. Failed registration
    /// does not enable shared compilation.
    pub fn enable_multishot_delimited_continuations(&mut self) -> JsResult<()> {
        self.install_delimited_continuations(true)?;
        self.vm.shared_continuation_bindings = true;
        Ok(())
    }

    fn install_delimited_continuations(&mut self, multi_shot: bool) -> JsResult<()> {
        let global = self.global_object();
        for name in [js_string!("reset"), js_string!("shift")] {
            if global.has_own_property(name, self)? {
                return Err(error("shift/reset global already exists"));
            }
        }
        if !global.is_extensible(self)? {
            return Err(error("global object is not extensible"));
        }
        let reset = function(
            if multi_shot {
                Kind::ResetMultiShot
            } else {
                Kind::Reset
            },
            self,
        )?;
        let shift = function(Kind::Shift, self)?;
        let attributes = Attribute::WRITABLE | Attribute::CONFIGURABLE;
        self.register_global_property(js_string!("reset"), reset, attributes)?;
        self.register_global_property(js_string!("shift"), shift, attributes)
    }
}

/// Ordinary frames are marked only after dispatch has returned. A native callback
/// still executing during dispatch therefore never acquires a safe-entry flag.
pub(crate) fn call(
    object: &JsObject,
    argument_count: usize,
    context: &mut Context,
) -> JsResult<()> {
    if let Some(data) = object.downcast_ref::<ControlFunction>() {
        let kind = data.kind.clone();
        let same_owner = Gc::ptr_eq(&data.owner, &context.vm.continuation_owner);
        let same_realm = data.realm == *context.realm();
        drop(data);
        let args = pop_call(argument_count, context);
        if !same_owner || !same_realm {
            return Err(error(
                "continuation API cannot cross Context or realm boundaries",
            ));
        }
        let value = args.first().cloned().unwrap_or_default();
        return match &kind {
            Kind::Reset => reset(&value, false, context),
            Kind::ResetMultiShot => reset(&value, true, context),
            Kind::Shift => capture(&value, context),
            Kind::Resume {
                snapshot,
                multi_shot,
            } => resume(snapshot, *multi_shot, value, context),
        };
    }
    let start = context.vm.frames.len();
    object.__call__(argument_count).resolve(context)?;
    if object.is::<OrdinaryFunction>() {
        for frame in &mut context.vm.frames[start..] {
            frame.flags |= CallFrameFlags::CONTINUATION_SAFE_ENTRY;
        }
    }
    Ok(())
}

fn callback(value: &JsValue, context: &Context) -> JsResult<JsObject> {
    let object = value
        .as_object()
        .ok_or_else(|| error("expected a synchronous JS callback"))?;
    {
        let data = object
            .downcast_ref::<OrdinaryFunction>()
            .ok_or_else(|| error("expected a synchronous JS callback"))?;
        if data.code.is_async()
            || data.code.is_generator()
            || data.code.is_class_constructor()
            || data.realm != *context.realm()
        {
            return Err(error("expected a synchronous JS callback in this realm"));
        }
    }
    Ok(object)
}

/// Sets up a callback without running it on the Rust stack.
fn enter_callback(object: &JsObject, args: &[JsValue], context: &mut Context) -> JsResult<()> {
    context.vm.stack.push(JsValue::undefined());
    context.vm.stack.push(object.clone());
    context.vm.stack.calling_convention_push_arguments(args);
    call(object, args.len(), context)
}

fn reset(value: &JsValue, multi_shot: bool, context: &mut Context) -> JsResult<()> {
    let object = callback(value, context)?;
    context.check_runtime_limits()?;
    let frame_index = context.vm.frames.len();
    enter_callback(&object, &[], context)?;
    context.vm.prompts.push(Prompt {
        frame_index,
        host_depth: context.vm.host_call_depth,
        multi_shot,
    });
    Ok(())
}

fn capture(value: &JsValue, context: &mut Context) -> JsResult<()> {
    let handler = callback(value, context)?;
    let prompt = context
        .vm
        .prompts
        .last()
        .ok_or_else(|| error("shift requires an active reset"))?;
    if prompt.host_depth != context.vm.host_call_depth {
        return Err(error("shift cannot cross a native callback boundary"));
    }
    let frame_index = prompt.frame_index;
    let multi_shot = prompt.multi_shot;
    if context.vm.pending_exception.is_some() {
        return Err(error("shift cannot capture a pending exception"));
    }
    for frame in &context.vm.frames[frame_index..] {
        if multi_shot
            && !frame
                .code_block
                .flags
                .get()
                .contains(CodeBlockFlags::SHARED_CONTINUATION_BINDINGS)
        {
            return Err(error(
                "multi-shot capture requires shared-binding compiled frames",
            ));
        }
        if !frame
            .flags
            .contains(CallFrameFlags::CONTINUATION_SAFE_ENTRY)
            || frame
                .flags
                .intersects(CallFrameFlags::CONSTRUCT | CallFrameFlags::NATIVE_FRAME)
            || frame.exit_early()
            || frame.code_block.is_async()
            || frame.code_block.is_generator()
            || !frame.iterators.is_empty()
            || frame.realm != *context.realm()
        {
            return Err(error("shift cannot cross this VM call boundary"));
        }
    }
    context.check_runtime_limits()?;
    // Allocate the callable before transferring ownership out of the live VM.
    let cell = Gc::new(GcRefCell::new(None));
    let continuation = function(
        Kind::Resume {
            snapshot: cell.clone(),
            multi_shot,
        },
        context,
    )?;
    let base = context.vm.frames[frame_index].fp;
    let stack = context
        .vm
        .stack
        .split_off_frame(&context.vm.frames[frame_index]);
    let mut frames = context.vm.frames.split_off(frame_index);
    for frame in &mut frames {
        frame.fp -= base;
        frame.rp -= base;
        frame.flags |= CallFrameFlags::REGISTERS_ALREADY_PUSHED;
        context.vm.shadow_stack.pop();
    }
    *cell.borrow_mut() = Some(Segment {
        frames,
        stack,
        return_value: context.vm.take_return_value(),
    });
    // Keep this prompt: shift's handler itself runs inside the delimiter.
    if let Err(err) = enter_callback(&handler, &[continuation.into()], context) {
        context.vm.prompts.pop();
        return Err(err);
    }
    Ok(())
}

fn resume(
    cell: &GcRefCell<Option<Segment>>,
    multi_shot: bool,
    value: JsValue,
    context: &mut Context,
) -> JsResult<()> {
    let mut state = cell.borrow_mut();
    let segment = state
        .as_ref()
        .ok_or_else(|| error("one-shot continuation already consumed"))?;
    let base = context.vm.stack.stack.len();
    if context.vm.frames.len() - 1 + segment.frames.len() + context.vm.host_call_depth
        >= context.vm.runtime_limits.recursion_limit()
        || base + segment.stack.stack.len() + 1 >= context.vm.runtime_limits.stack_size_limit()
    {
        return Err(error("continuation exceeds VM limits"));
    }
    let offset = u32::try_from(base).map_err(|_| error("continuation stack offset overflow"))?;
    if segment
        .frames
        .iter()
        .any(|f| f.fp.checked_add(offset).is_none() || f.rp.checked_add(offset).is_none())
    {
        return Err(error("continuation frame offset overflow"));
    }
    // One-shot is consumed before user code. Multi-shot keeps an immutable
    // control snapshot; drop the borrow before running a possibly recursive resume.
    let mut segment = if multi_shot {
        segment.clone()
    } else {
        state.take().expect("validated continuation")
    };
    drop(state);
    let frame_index = context.vm.frames.len();
    context.vm.stack.stack.append(&mut segment.stack.stack);
    for mut frame in segment.frames.drain(..) {
        frame.fp += offset;
        frame.rp += offset;
        context.vm.push_frame(frame);
    }
    context.vm.stack.push(value);
    context.vm.return_value = std::mem::take(&mut segment.return_value);
    context.vm.prompts.push(Prompt {
        frame_index,
        host_depth: context.vm.host_call_depth,
        multi_shot,
    });
    Ok(())
}

/// Tag the compiled Script tree, never unrelated or dynamically compiled code.
/// The escape pass ran before compilation; this flag is evidence, not a rewrite.
pub(crate) fn mark_shared_code(code: &CodeBlock) {
    code.flags
        .set(code.flags.get() | CodeBlockFlags::SHARED_CONTINUATION_BINDINGS);
    for constant in &code.constants {
        if let Constant::Function(child) = constant {
            mark_shared_code(child);
        }
    }
}

#[cfg(test)]
mod multishot_tests;
#[cfg(test)]
mod tests;
