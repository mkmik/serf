//! Bytecode interpreter.
//!
//! Activations live in an explicit `Vec<Frame>` rather than on the Rust stack,
//! so Self-level recursion is bounded by memory, not by the host stack. A send
//! in tail position always reuses the caller's frame, which is what makes
//! `whileTrue:` (recursive in Self) run in constant space; the frame carries
//! the activations it displaced, so a block returning non-locally to one of
//! them still finds the continuation it belongs to.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::compile::*;
use crate::prims;
use crate::value::*;

pub struct SelfErr {
    pub msg: String,
    pub trace: Vec<String>,
}

pub enum Unwind {
    Err(SelfErr),
    /// a block returned to a home method that is not on this run's stack
    EscapedNLR,
}

pub struct Frame {
    scope: ObjRef,
    /// the activation's method, held here rather than fetched from it.
    /// `act_method` is a thread-local, a `RefCell` borrow and an `Rc` clone,
    /// and the interpreter asks for the method on almost every bytecode.
    method: Rc<Method>,
    pc: usize,
    /// activations this frame tail-called away from. They have not returned --
    /// their continuation is now this frame's -- so a block returning
    /// non-locally to one of them has to find it here.
    tail_of: Vec<ObjRef>,
}

impl Frame {
    fn new(scope: ObjRef) -> Frame {
        Frame { method: act_method(scope), scope, pc: 0, tail_of: vec![] }
    }

    /// This frame's activation and every one whose continuation it took over
    /// have now genuinely returned. There is nothing to hand back: an
    /// activation is a heap object, so the frame simply stops naming it and the
    /// next scavenge forgets it. The pools this used to feed are gone.
    fn retire(self) {
        act_set_dead(self.scope, true);
        for s in self.tail_of.iter() {
            act_set_dead(*s, true);
        }
    }

    /// The operand stack lives in the activation, so pushing costs a store and
    /// nothing else -- no `Vec`, and none of the 1,538,337 `malloc`s a test run
    /// made when every frame brought its own.
    fn push(&self, v: Value) {
        crate::obj::act_push(self.scope, v)
    }
    fn pop(&self) -> Option<Value> {
        (self.depth() > 0).then(|| crate::obj::act_pop(self.scope))
    }
    fn last(&self) -> Option<Value> {
        crate::obj::act_peek(self.scope)
    }
    fn depth(&self) -> usize {
        crate::obj::act_depth(self.scope)
    }
    /// The operand at depth `i`, for a send collecting its arguments in order.
    fn at(&self, i: usize) -> Value {
        crate::obj::act_stack_at(self.scope, i)
    }
    fn truncate(&self, n: usize) {
        crate::obj::act_truncate(self.scope, n)
    }

    fn is_home(&self, target: ObjRef) -> bool {
        self.scope == target || self.tail_of.iter().any(|s| *s == target)
    }
}

/// Everything a frame stack keeps alive, as slots the collector may rewrite.
///
/// The operand stack is not here any more: it is inside the activation, so
/// tracing the activation traces it.
///
/// `tail_of` is in here now, where it was not before: with a handle table an
/// untraced reference was merely stale, but a direct pointer into a space that
/// has been abandoned is not, and a non-local return still compares against
/// these. The activations themselves stay alive either way -- a block that
/// captured one holds it -- but the pointer has to be moved with them.
pub fn frame_roots(fs: &mut [Frame], f: &mut dyn FnMut(&mut ObjRef)) {
    for fr in fs.iter_mut() {
        f(&mut fr.scope);
    }
}

/// `tail_of` is weak, and has to be. It holds activations this frame took the
/// continuation of, purely so a non-local return can recognise one -- nothing
/// ever reads their slots through it. An entry no block still holds can never
/// be the target of anything, so keeping it alive would be keeping every
/// activation a loop ever made: `whileTrue:` is recursive in Self, its
/// recursive call is in tail position, and the block that carries it captures
/// the activation each time round.
///
/// `Rc::strong_count` used to answer "does anything else hold this?". The
/// collector answers it now, which is the same question asked at the only
/// moment it can be answered honestly.
pub fn frame_weak(fs: &mut [Frame], f: &mut dyn FnMut(ObjRef) -> Option<ObjRef>) {
    for fr in fs.iter_mut() {
        fr.tail_of = fr.tail_of.iter().filter_map(|s| f(*s)).collect();
    }
}

/// Lend the frame stack to the `Vm` for the duration of `body`, so that a
/// collection inside it can see the activations as roots. Every path that can
/// re-enter the interpreter or reach a safepoint goes through here: an outer
/// `run_stack`'s frames are otherwise a Rust local, invisible to the collector.
/// Moving a `Vec` is two pointer writes, so this is cheap enough to do around
/// every primitive call.
fn lending<T>(vm: &mut Vm, frames: &mut Vec<Frame>, body: impl FnOnce(&mut Vm) -> T) -> T {
    vm.stacks.push(std::mem::take(frames));
    let r = body(vm);
    *frames = vm.stacks.pop().unwrap();
    r
}

/// Why `run_stack` came back.
pub enum Outcome {
    Done(Value),
    /// the stack executed `_Yield:` and is parked, ready to resume
    Yielded { rcvr: Value, arg: Value },
}

/// An activation is a heap object, so this is one bump and a few stores -- the
/// pools that used to stand between a send and `malloc` are gone with it.
pub fn new_scope(
    m: Rc<Method>,
    recv: Value,
    holder: Value,
    args: &[Value],
    lexical: Option<ObjRef>,
) -> ObjRef {
    crate::value::new_activation(m, recv, holder, args, lexical)
}

/// The error the current process is suspending for, if any. The world's own
/// error objects carry `message` and `receiver`; the VM's know their
/// `errorString`.
fn cause_of_error(vm: &mut Vm) -> Option<String> {
    let p = vm.current_proc.as_ref().or(vm.vm_proc.as_ref())?.clone();
    let nil = vm.nil_v();
    let o = p.as_obj()?;
    let err = {
        let b = o.borrow();
        let i = b.find("causeOfError")?;
        let v = b.slots.get(i).value;
        if v.id_eq(&nil) {
            return None;
        }
        v
    };
    let mut parts: Vec<String> = vec![];
    {
        let eo = err.as_obj()?;
        let b = eo.borrow();
        for s in b.slots.iter() {
            if s.kind == SlotKind::Assign {
                continue;
            }
            if let Some(t) = s.value.as_str() {
                let t = t.trim();
                if !t.is_empty() {
                    parts.push(format!("{}: {:.300}", s.name, t));
                }
            }
        }
        if let Some(i) = b.find("receiver") {
            parts.push(format!("receiver: {:.200}", default_print_string(vm, &b.slots.get(i).value)));
        }
    }
    if parts.is_empty() {
        if let Ok(t) = send(vm, err.clone(), "errorString", vec![]) {
            if let Some(x) = t.as_str() {
                parts.push(format!("errorString: {:.300}", x));
            }
        }
        if parts.is_empty() {
            parts.push(format!("error object {:.300}", default_print_string(vm, &err)));
        }
    }
    Some(parts.join("; "))
}

fn clear_cause_of_error(vm: &Vm) {
    let p = match vm.current_proc.as_ref().or(vm.vm_proc.as_ref()) {
        Some(p) => p.clone(),
        None => return,
    };
    let nil = vm.nil_v();
    if let Some(o) = p.as_obj() {
        let i = o.borrow().find("causeOfError");
        if let Some(i) = i {
            o.borrow_mut().assign(i, nil);
        }
    }
}

/// A frame stack that will perform one send when run: how a Self process is
/// started (`_NewProcessSize:Receiver:Selector:Arguments:`).
pub fn stack_for_send(vm: &mut Vm, recv: Value, sel: &str, args: Vec<Value>) -> Vec<Frame> {
    let n = args.len();
    let mut code = vec![];
    for i in 0..=n {
        emit_into(&mut code, READ_LOCAL, i);
    }
    emit_into(&mut code, SEND, 0);
    let m = Rc::new(Method {
        sel: "<process>".into(),
        nargs: n + 1,
        arg_slots: (0..=n).collect(),
        slot_names: (0..=n).map(|i| -> Rc<str> { format!("a{}", i).as_str().into() }).collect(),
        slot_flags: vec![2 << 2; n + 1],
        slot_inits: RefCell::new(vec![vm.nil_v(); n + 1]),
        code,
        lits: RefCell::new(vec![vm.string(sel)]),
        lit_strs: vec![Some(sel.into())],
        is_block: false,
        file: "<process>".into(),
        line: 0,
        source: Cell::new(None),
        sites: Default::default(),
        max_stack: std::cell::Cell::new(u32::MAX),
    });
    let mut a = vec![recv.clone()];
    a.extend(args);
    let scope = new_scope(m, recv, recv, &a, None);
    vec![Frame::new(scope)]
}

/// Send a message from Rust (used by the REPL for `printString`).
pub fn send(vm: &mut Vm, recv: Value, sel: &str, args: Vec<Value>) -> Result<Value, Unwind> {
    let n = args.len();
    let mut code = vec![];
    for i in 0..=n {
        emit_into(&mut code, READ_LOCAL, i);
    }
    emit_into(&mut code, SEND, 0);
    let m = Rc::new(Method {
        sel: "<send>".into(),
        nargs: n + 1,
        arg_slots: (0..=n).collect(),
        slot_names: (0..=n).map(|i| -> Rc<str> { format!("a{}", i).as_str().into() }).collect(),
        slot_flags: vec![2 << 2; n + 1],
        slot_inits: RefCell::new(vec![vm.nil.clone(); n + 1]),
        code,
        lits: RefCell::new(vec![vm.string(sel)]),
        lit_strs: vec![Some(sel.into())],
        is_block: false,
        file: "<vm>".into(),
        line: 0,
        source: Cell::new(None),
        sites: Default::default(),
        max_stack: std::cell::Cell::new(u32::MAX),
    });
    let mut a = vec![recv];
    a.extend(args);
    let lobby = vm.lobby.clone();
    let scope = new_scope(m, lobby, lobby, &a, None);
    run(vm, scope)
}

pub fn describe(vm: &Vm, u: Unwind) -> String {
    let _ = vm;
    match u {
        Unwind::Err(e) => {
            if e.trace.is_empty() {
                e.msg
            } else {
                format!("{}\n{}", e.msg, e.trace.join("\n"))
            }
        }
        Unwind::EscapedNLR => "non-local return from a block whose home method has returned".into(),
    }
}

fn err(frames: &[Frame], msg: String) -> Unwind {
    let trace = frames
        .iter()
        .rev()
        .take(24)
        .map(|f| {
            format!(
                "  at {} ({}:{})",
                f.method.sel, f.method.file, f.method.line
            )
        })
        .collect();
    Unwind::Err(SelfErr { msg, trace })
}

fn nth_lexical(s: ObjRef, n: usize) -> Option<ObjRef> {
    let mut cur = s;
    for _ in 0..n {
        cur = act_lexical(cur)?;
    }
    Some(cur)
}

fn clone_block(proto: &Value, scope: ObjRef) -> Value {
    let o = proto.as_obj().unwrap();
    let m = o.borrow().payload.method().expect("a block prototype without a method");
    let slots: Vec<Slot> = o.borrow().slots.iter().collect();
    Value::obj(slots, Payload::Block(m, Some(scope)))
}

/// Run to completion, standing in for the Self scheduler when the code
/// transfers out: `_Yield: action` does what scheduler.self's `yielded`
/// handler does -- sends the action `doAction` -- and then resumes. Enough of
/// a scheduler for code that yields for a service; a real one would pick the
/// next process off the ready queue.
pub fn run(vm: &mut Vm, scope: ObjRef) -> Result<Value, Unwind> {
    let mut frames = vec![Frame::new(scope)];
    loop {
        match run_stack(vm, &mut frames)? {
            Outcome::Done(v) => return Ok(v),
            Outcome::Yielded { arg, .. } => {
                // `suspendAndTrace: err` sets the process's causeOfError and
                // then transfers out. A real scheduler leaves such a process
                // suspended and runs someone else; resuming it would fall
                // into the code after the error, so stop instead.
                let n_roots = vm.temp_roots.len();
                vm.temp_roots.push(arg);
                let failed = lending(vm, &mut frames, cause_of_error);
                let sent = lending(vm, &mut frames, |vm| send(vm, arg, "doAction", vec![]));
                vm.temp_roots.truncate(n_roots);
                sent?;
                if let Some(msg) = failed {
                    clear_cause_of_error(vm);
                    return Err(err(&frames, msg));
                }
            }
        }
    }
}

/// A frame stack is a Self process's stack: run it until it finishes or
/// yields, leaving it resumable either way.
pub fn run_stack(vm: &mut Vm, frames: &mut Vec<Frame>) -> Result<Outcome, Unwind> {
    // interbytecode state, per the C++ abstract_interpreter
    // one argument buffer for the whole run: a send fills it, `new_scope`
    // copies out of it into the activation, and it comes straight back
    let mut argbuf: Vec<Value> = Vec::new();
    let mut index: usize = 0;
    let mut lex_level: usize = 0;
    let mut delegatee: Option<Rc<str>> = None;
    let mut resend = false;

    loop {
        // The safepoint. Allocation only ever asks for a collection; it
        // happens here, between two bytecodes, where the only live references
        // are the ones `Vm::each_root` walks. The C++ VM arranges the same
        // thing by poisoning the stack limit so the next method prologue traps
        // (universe.hh:148).
        if crate::gc::wanted() {
            lending(vm, frames, crate::gc::collect_if_wanted);
        }

        let (at_end, op, x) = {
            let f = frames.last().unwrap();
            let code = &f.method.code;
            if f.pc >= code.len() {
                (true, 0u8, 0usize)
            } else {
                let c = code[f.pc];
                (false, c >> 4, (c & 0xF) as usize)
            }
        };

        if at_end {
            let f = frames.pop().unwrap();
            let res = f.last().unwrap_or_else(|| act_recv(f.scope));
            f.retire();
            match frames.last_mut() {
                None => return Ok(Outcome::Done(res)),
                Some(c) => c.push(res),
            }
            continue;
        }
        frames.last_mut().unwrap().pc += 1;

        match op {
            INDEX => index = (index << 4) | x,

            LITERAL => {
                let idx = (index << 4) | x;
                index = 0;
                let f = frames.last().unwrap();
                let lit = match f.method.lits.borrow().get(idx) {
                    Some(l) => l.clone(),
                    None => return Err(err(frames, "literal index out of range".into())),
                };
                let is_proto = lit.as_obj().is_some_and(|o| {
                    let p = o.borrow().payload;
                    p.kind() == PayKind::Block && p.block_scope().is_none()
                });
                let v = if is_proto { clone_block(&lit, f.scope) } else { lit };
                frames.last_mut().unwrap().push(v);
            }

            NO_OPERAND => match x {
                SELF_C => {
                    let v = act_recv(frames.last().unwrap().scope);
                    frames.last_mut().unwrap().push(v);
                }
                POP_C => {
                    frames.last_mut().unwrap().pop();
                }
                RESEND_C => resend = true,
                NLR_C => {
                    let f = frames.last_mut().unwrap();
                    let value = match f.pop() {
                        Some(v) => v,
                        None => return Err(err(frames, "nothing to return".into())),
                    };
                    let target = home_of(frames.last().unwrap().scope);
                    if act_dead(target) {
                        return Err(err(frames, "non-local return to an activation that already returned".into()));
                    }
                    loop {
                        let f = frames.pop().unwrap();
                        let hit = f.is_home(target);
                        f.retire();
                        if hit {
                            match frames.last_mut() {
                                None => return Ok(Outcome::Done(value)),
                                Some(c) => {
                                    c.push(value);
                                    break;
                                }
                            }
                        }
                        if frames.is_empty() {
                            return Err(Unwind::EscapedNLR);
                        }
                    }
                }
                _ => return Err(err(frames, format!("illegal no-operand code {}", x))),
            },

            LEXICAL_LEVEL => {
                lex_level = (index << 4) | x;
                index = 0;
            }

            READ_LOCAL | WRITE_LOCAL => {
                let idx = (index << 4) | x;
                index = 0;
                let lvl = lex_level;
                lex_level = 0;
                let sc = match nth_lexical(frames.last().unwrap().scope, lvl) {
                    Some(s) => s,
                    None => return Err(err(frames, "no such lexical scope".into())),
                };
                if idx >= act_locals(sc) {
                    return Err(err(frames, "local slot index out of range".into()));
                }
                if op == READ_LOCAL {
                    let v = act_local(sc, idx);
                    frames.last_mut().unwrap().push(v);
                } else {
                    let f = frames.last_mut().unwrap();
                    let v = match f.pop() {
                        Some(v) => v,
                        None => return Err(err(frames, "nothing to assign".into())),
                    };
                    act_set_local(sc, idx, v);
                    let me = act_recv(frames.last().unwrap().scope);
                    frames.last_mut().unwrap().push(me);
                }
            }

            DELEGATEE => {
                let idx = (index << 4) | x;
                index = 0;
                delegatee = act_method(frames.last().unwrap().scope).lit_strs.get(idx).cloned().flatten();
                if delegatee.is_none() {
                    return Err(err(frames, "delegatee must be a name".into()));
                }
            }

            SEND | IMPLICIT_SEND => {
                let idx = (index << 4) | x;
                index = 0;
                let normal = op == SEND;
                let sel = match act_method(frames.last().unwrap().scope).lit_strs.get(idx).cloned().flatten() {
                    Some(s) => s,
                    None => return Err(err(frames, "selector must be a string".into())),
                };
                let argc = act_method(frames.last().unwrap().scope).site_nargs(idx, &sel);
                let (mut cur_recv, mut cur_args) = {
                    let f = frames.last_mut().unwrap();
                    if f.depth() < argc + normal as usize {
                        return Err(err(frames, format!("stack underflow sending '{}'", sel)));
                    }
                    let n = f.depth();
                    let mut args = std::mem::take(&mut argbuf);
                    args.clear();
                    args.extend((n - argc..n).map(|i| f.at(i)));
                    f.truncate(n - argc);
                    let recv = if normal {
                        f.pop().unwrap()
                    } else {
                        act_recv(f.scope)
                    };
                    (recv, args)
                };
                let mut cur_sel: Rc<str> = sel;
                let mut cur_del = delegatee.take();
                let mut cur_resend = resend;
                resend = false;
                // This send's inline cache, good for one trip round the loop
                // only: a retry (a primitive's fail block, undefinedSelector)
                // carries a different selector, and the cache belongs to the
                // site, not to whatever is being sent from it. Primitives and
                // resends never reach `lookup` by this path, so they leave the
                // site empty rather than filling it with someone else's hit.
                let mut site = (!cur_resend && cur_del.is_none() && !cur_sel.starts_with('_'))
                    .then_some(idx);

                loop {
                    // _Restart re-enters the current activation from the top,
                    // as interpreter::do_send_code does with restart_pc()
                    if &*cur_sel == "_Restart" || &*cur_sel == "_RestartIfFail:" {
                        let f = frames.last_mut().unwrap();
                        f.pc = 0;
                        f.truncate(0);
                        break;
                    }
                    if cur_sel.starts_with('_') {
                        // Self's convention: a trailing IfFail: keyword takes a
                        // block that is sent value: with the error string when
                        // the primitive fails (including "not implemented"),
                        // which is how the world recovers by itself
                        let mut fail = None;
                        let mut base: &str = &cur_sel;
                        if let Some(b) = cur_sel.strip_suffix("IfFail:") {
                            if !cur_args.is_empty() && &*cur_sel != "_RestartIfFail:" {
                                base = b;
                                fail = cur_args.pop();
                            }
                        }
                        // a primitive can re-enter the interpreter (the
                        // scheduler stepping a process, `_Parse…` compiling),
                        // and so can collect: everything live in this Rust
                        // frame has to be reachable from the Vm first
                        let n_roots = vm.temp_roots.len();
                        vm.temp_roots.push(cur_recv);
                        vm.temp_roots.extend_from_slice(&cur_args);
                        vm.temp_roots.extend(fail);
                        let called = lending(vm, frames, |vm| prims::call(vm, base, &cur_recv, &cur_args));
                        vm.temp_roots.truncate(n_roots);
                        match called {
                            Err(m) => match fail {
                                Some(b) => {
                                    if std::env::var_os("SERF_TRACE_PRIMS").is_some() {
                                        eprintln!("prim failed: {} -> {}", cur_sel, m);
                                    }
                                    // a fail block takes the error, and
                                    // sometimes the primitive's name too
                                    let two = b.as_obj().map_or(false, |o| {
                                        o.borrow().find("value:With:").is_some()
                                    });
                                    let err_s = vm.string(&m);
                                    cur_args = if two {
                                        cur_sel = "value:With:".into();
                                        vec![err_s, vm.string(&cur_sel)]
                                    } else {
                                        cur_sel = "value:".into();
                                        vec![err_s]
                                    };
                                    cur_recv = b;
                                    cur_del = None;
                                    cur_resend = false;
                                    continue;
                                }
                                None => return Err(err(frames, m)),
                            },
                            Ok(prims::P::Val(v)) => {
                                frames.last_mut().unwrap().push(v);
                                break;
                            }
                            Ok(prims::P::Yield(r, a)) => {
                                // when the process is resumed, _Yield: answers nil
                                let n = vm.nil_v();
                                frames.last_mut().unwrap().push(n);
                                return Ok(Outcome::Yielded { rcvr: r, arg: a });
                            }
                            Ok(prims::P::Activate(m, r)) => {
                                let ns = new_scope(m, r, r, &[], None);
                                frames.push(Frame::new(ns));
                                break;
                            }
                            Ok(prims::P::Send(r, s, a)) => {
                                cur_recv = r;
                                cur_sel = s.as_str().into();
                                cur_args = a;
                                cur_del = None;
                                cur_resend = false;
                                continue;
                            }
                        }
                    }

                    let holder = act_holder(frames.last().unwrap().scope).clone();
                    let found = if let Some(s) = site.take() {
                        let m = &act_method(frames.last().unwrap().scope);
                        let start = vm.lookup_key(&cur_recv);
                        // the same receiver again answers without reading it;
                        // only a new one pays a deref to find out its shape
                        match m.site_hit_recv(s, start) {
                            Some(h) => Ok(h.at(start)),
                            None => {
                                let key = start.borrow().map();
                                match m.site_hit_map(s, key) {
                                    Some(h) => Ok(h.at(start)),
                                    None => {
                                        let r = vm.lookup_from(&cur_recv, &cur_sel, start, key);
                                        if let Ok(h) = r {
                                            m.site_fill(s, start, key, MapHit::of(start, h));
                                        }
                                        r
                                    }
                                }
                            }
                        }
                    } else if cur_resend {
                        vm.lookup_in_parents(&holder, &cur_sel)
                    } else if let Some(d) = cur_del.take() {
                        match vm.lookup(&holder, &d) {
                            Ok(h) => {
                                let dv = h.holder.borrow().slots.get(h.idx).value;
                                vm.lookup(&dv, &cur_sel)
                            }
                            Err(_) => {
                                return Err(err(frames, format!("no delegatee slot '{}' in the method holder", d)))
                            }
                        }
                    } else {
                        vm.lookup(&cur_recv, &cur_sel)
                    };

                    let hit = match found {
                        Ok(h) => h,
                        Err(LookupErr::NotFound) => {
                            // What the VM itself does about a failed lookup
                            // (simpleLookup::generateLookupErrorMethod): send
                            // undefinedSelector:Receiver:Type:Delegatee:
                            // MethodHolder:Arguments: to the current process.
                            // The world's handler on traits process forwards
                            // it to the receiver's own five-keyword
                            // undefinedSelector:... when it has one, which is
                            // how x11Globals fontFamily conjures a font family
                            // on demand and how morphs forward messages.
                            const ERR_SEL: &str =
                                "undefinedSelector:Receiver:Type:Delegatee:MethodHolder:Arguments:";
                            // `_ThisProcess` only reads or allocates, so it
                            // cannot collect -- but it is reached by a runtime
                            // name like any other primitive, so it gets the
                            // same treatment as the call at the top of the loop
                            let n_roots = vm.temp_roots.len();
                            vm.temp_roots.push(cur_recv);
                            let this = lending(vm, frames, |vm| {
                                prims::call(vm, "_ThisProcess", &cur_recv, &[])
                            });
                            vm.temp_roots.truncate(n_roots);
                            let proc = match this {
                                Ok(prims::P::Val(p)) if &*cur_sel != ERR_SEL => Some(p),
                                _ => None,
                            };
                            if let Some(proc) = proc.filter(|p| vm.lookup(p, ERR_SEL).is_ok()) {
                                let ty = vm.string(if !normal {
                                    "implicitSelf"
                                } else if cur_resend {
                                    "undirectedResend"
                                } else {
                                    "normal"
                                });
                                let nil = vm.nil_v();
                                let sel_s = vm.string(&cur_sel);
                                let mh = if cur_resend { holder.clone() } else { nil.clone() };
                                let av = vm.vector(std::mem::take(&mut cur_args));
                                cur_args = vec![sel_s, cur_recv.clone(), ty, nil, mh, av];
                                cur_sel = ERR_SEL.into();
                                cur_recv = proc;
                                cur_del = None;
                                cur_resend = false;
                                continue;
                            }
                            let r = default_print_string(vm, &cur_recv);
                            return Err(err(frames, format!("{} does not understand '{}'", r, cur_sel)));
                        }
                        Err(LookupErr::Ambiguous) => {
                            return Err(err(frames, format!("'{}' is ambiguous: found in more than one parent", cur_sel)))
                        }
                    };

                    let (kind, val, sname) = {
                        let b = hit.holder.borrow();
                        let s = &b.slots.get(hit.idx);
                        (s.kind, s.value, s.name)
                    };

                    if kind == SlotKind::Assign {
                        let target = sym_str(sname).trim_end_matches(':').to_string();
                        let b = hit.holder.borrow_mut();
                        match b.find(&target) {
                            Some(i) => b.assign(i, cur_args[0].clone()),
                            None => {
                                return Err(err(frames, format!("assignment slot '{}' has no data slot", sname)));
                            }
                        }
                        frames.last_mut().unwrap().push(cur_recv);
                        break;
                    }

                    let m = match val.method() {
                        None => {
                            frames.last_mut().unwrap().push(val);
                            break;
                        }
                        Some(m) => m,
                    };
                    if m.nargs != cur_args.len() {
                        return Err(err(frames, format!("'{}' expects {} arguments", cur_sel, m.nargs)));
                    }

                    let (nrecv, nholder, lexical) = if m.is_block {
                        let lex = cur_recv.as_obj().and_then(|o| o.borrow().payload.block_scope());
                        match lex {
                            Some(l) => (act_recv(l), act_holder(l), Some(l)),
                            None => {
                                return Err(err(frames, "block code activated outside of a block".into()))
                            }
                        }
                    } else {
                        (cur_recv.clone(), Value::Obj(hit.holder.clone()), None)
                    };

                    let ns = new_scope(m, nrecv, nholder, &cur_args, lexical);
                    // the buffer goes back for the next send rather than to `free`
                    argbuf = cur_args;

                    // A send in tail position takes over our frame: there is
                    // nothing left for us to do with the answer but hand it on.
                    // We do not return, though -- a block of ours may still
                    // return non-locally *through* us -- so the callee's frame
                    // carries our activation as one it now stands for. Nothing
                    // can reach an activation with no other reference, so those
                    // are simply retired.
                    let tail = {
                        let f = frames.last().unwrap();
                        f.pc >= f.method.code.len() && (f.depth() == 0)
                    };
                    let mut carried = vec![];
                    if tail {
                        let old = frames.pop().unwrap();
                        carried = old.tail_of;
                        // it has not returned -- its continuation is now the
                        // callee's -- so it is carried, not retired. The list
                        // is weak, so a collection drops the entries no block
                        // still holds, which is what keeps a `whileTrue:` from
                        // accumulating one per iteration.
                        carried.push(old.scope);
                    }
                    if frames.len() >= vm.max_frames {
                        let mut h: std::collections::HashMap<String, usize> = Default::default();
                        for f in frames.iter() {
                            *h.entry(format!("{} ({}:{})", f.method.sel, f.method.file, f.method.line))
                                .or_insert(0) += 1;
                        }
                        let mut v: Vec<_> = h.into_iter().collect();
                        v.sort_by(|a, b| b.1.cmp(&a.1));
                        for (k, c) in v.iter().take(6) {
                            eprintln!("  {:8} frames in {}", c, k);
                        }
                        return Err(err(frames, "activation stack overflow".into()));
                    }
                    frames.push(Frame { tail_of: carried, ..Frame::new(ns) });
                    break;
                }
            }

            ARGUMENT_COUNT => {
                // as the first bytecode this selects the instruction set; the
                // interpreter derives arity from the selector either way
                index = 0;
            }

            BRANCH | BRANCH_TRUE | BRANCH_FALSE | BRANCH_INDEXED => {
                let idx = (index << 4) | x;
                index = 0;
                let lit = match act_method(frames.last().unwrap().scope).lits.borrow().get(idx) {
                    Some(l) => l.clone(),
                    None => return Err(err(frames, "branch literal out of range".into())),
                };
                let target = |l: &Value| match l {
                    Value::Int(i) if *i >= 0 => Some(*i as usize),
                    _ => None,
                };
                let dest = match op {
                    BRANCH_INDEXED => {
                        let f = frames.last_mut().unwrap();
                        let i = match f.pop() {
                            Some(Value::Int(i)) if i >= 0 => i as usize,
                            _ => return Err(err(frames, "indexed branch needs an index".into())),
                        };
                        match lit.as_obj().and_then(|o| {
                            let p = o.borrow().payload;
                            (i < p.vector_len()).then(|| p.element(i)).and_then(|t| target(&t))
                        }) {
                            Some(d) => Some(d),
                            None => return Err(err(frames, "bad indexed branch table".into())),
                        }
                    }
                    BRANCH => target(&lit),
                    _ => {
                        let f = frames.last_mut().unwrap();
                        let c = match f.pop() {
                            Some(c) => c,
                            None => return Err(err(frames, "nothing to branch on".into())),
                        };
                        let is_false = vm.is_false(&c);
                        if (op == BRANCH_FALSE) == is_false { target(&lit) } else { None }
                    }
                };
                match dest {
                    Some(d) => {
                        let f = frames.last_mut().unwrap();
                        if d > f.method.code.len() {
                            return Err(err(frames, "branch target out of range".into()));
                        }
                        f.pc = d;
                    }
                    None if op == BRANCH || op == BRANCH_INDEXED => {
                        return Err(err(frames, "branch target must be a bytecode index".into()))
                    }
                    None => {}
                }
            }

            _ => return Err(err(frames, format!("illegal bytecode {}", op))),
        }
    }
}
