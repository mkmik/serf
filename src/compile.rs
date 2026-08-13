//! Bytecode generation. Opcode set is the one from
//! vm/src/any/parser/byteCodes.hh: 4-bit op, 4-bit index, with INDEX_CODE
//! prefixes shifting in wider indices.
//!
//! ponytail: the BRANCH_* opcodes are omitted -- Self source never produces
//! them (all control flow is message sends to blocks); they exist in the C++
//! VM only for the `_BranchTo:` reflective primitives.

use std::rc::Rc;

use crate::interp;
use crate::parser::{stmt_line, Expr, ObjLit, SendKind};
use crate::value::*;

pub const INDEX: u8 = 0;
pub const LITERAL: u8 = 1;
pub const SEND: u8 = 2;
pub const IMPLICIT_SEND: u8 = 3;
pub const NO_OPERAND: u8 = 4;
pub const READ_LOCAL: u8 = 5;
pub const WRITE_LOCAL: u8 = 6;
pub const LEXICAL_LEVEL: u8 = 7;
pub const BRANCH: u8 = 8;
pub const BRANCH_TRUE: u8 = 9;
pub const BRANCH_FALSE: u8 = 10;
pub const BRANCH_INDEXED: u8 = 11;
pub const DELEGATEE: u8 = 12;
/// doubles as INSTRUCTION_SET_SELECTION_CODE when it is the first bytecode
pub const ARGUMENT_COUNT: u8 = 13;

pub const SELF_C: usize = 0;
pub const POP_C: usize = 1;
pub const NLR_C: usize = 2;
pub const RESEND_C: usize = 3;

/// Self derives a send's arity from its selector.
pub fn arg_count(sel: &str) -> usize {
    if sel.contains(':') {
        sel.matches(':').count()
    } else if sel.starts_with(|c: char| c.is_alphanumeric() || c == '_') {
        0
    } else {
        1
    }
}

/// Wide indices are shifted in a nibble at a time by INDEX_CODE prefixes.
pub fn emit_into(code: &mut Vec<u8>, op: u8, idx: usize) {
    let mut nib = vec![];
    let mut x = idx;
    while x > 0xF {
        nib.push((x & 0xF) as u8);
        x >>= 4;
    }
    nib.push(x as u8);
    nib.reverse();
    let last = nib.pop().unwrap();
    for n in nib {
        code.push((INDEX << 4) | n);
    }
    code.push((op << 4) | last);
}

pub fn block_selector(n: usize) -> String {
    if n == 0 {
        "value".to_string()
    } else {
        format!("value:{}", "With:".repeat(n - 1))
    }
}

struct MB {
    sel: Rc<str>,
    nargs: usize,
    names: Vec<Rc<str>>,
    mutable: Vec<bool>,
    inits: Vec<Value>,
    code: Vec<u8>,
    lits: Vec<Value>,
    lit_strs: Vec<Option<Rc<str>>>,
    is_block: bool,
    line: u32,
}

struct C<'a> {
    vm: &'a mut Vm,
    mbs: Vec<MB>,
    file: Rc<str>,
}

// -------------------------------------------------------------- entry points

/// Compile one top-level statement into a runnable method.
pub fn compile_statement(vm: &mut Vm, e: &Expr, file: &str) -> Result<Rc<Method>, String> {
    let _g = crate::gc::NoGc::new();
    let o = ObjLit { args: vec![], slots: vec![], body: vec![e.clone()], line: stmt_line(e) };
    let file: Rc<str> = file.into();
    let mut c = C { vm, mbs: vec![], file: file.clone() };
    c.push_method(&o, "<top level>", false)
}

pub fn compile_method(vm: &mut Vm, o: &ObjLit, sel: &str, file: &Rc<str>) -> Result<Rc<Method>, String> {
    let _g = crate::gc::NoGc::new();
    let mut c = C { vm, mbs: vec![], file: file.clone() };
    c.push_method(o, sel, false)
}

/// An object literal with no code: a constant object, built now.
pub fn build_object(vm: &mut Vm, o: &ObjLit, file: &Rc<str>) -> Result<Value, String> {
    // nesting a NoGc is free, and these two are `pub`: a caller that reached
    // them directly would be building slots in a Rust local across `eval_const`
    let _g = crate::gc::NoGc::new();
    if !o.args.is_empty() {
        return Err("argument slots are only allowed in methods".into());
    }
    let mut slots = vec![];
    for d in &o.slots {
        let value = match &d.init {
            Some(Expr::ObjLit(inner)) if is_method_literal(inner) => {
                let m = compile_method(vm, inner, &d.name, file)?;
                Value::obj([], Payload::Method(m))
            }
            Some(e) => eval_const(vm, e, file)?,
            None => vm.nil.clone(),
        };
        let kind = if d.parent { SlotKind::Parent } else { SlotKind::Data };
        slots.push(Slot { name: d.name.as_str().into(), kind, value });
        if d.mutable {
            slots.push(Slot {
                name: format!("{}:", d.name).as_str().into(),
                kind: SlotKind::Assign,
                value: vm.nil.clone(),
            });
        }
    }
    Ok(Value::obj(slots, Payload::None))
}

/// What a parsed body evaluates to: a method if it has code, otherwise the
/// object its slots describe. Object::Eval in the C++ parser.
pub fn build_body(vm: &mut Vm, o: &ObjLit, file: &str) -> Result<Value, String> {
    // Compilation keeps literal vectors and half-built slot lists in Rust
    // locals, and `eval_const` runs Self code from inside them, so the
    // collector must stay out until the method exists. ponytail: bounded by
    // the size of the source being compiled; give `C` a root list if a world
    // ever compiles enough in one go to matter.
    let _g = crate::gc::NoGc::new();
    let file: Rc<str> = file.into();
    if is_method_literal(o) {
        let m = compile_method(vm, o, "<doIt>", &file)?;
        return Ok(Value::obj([], Payload::Method(m)));
    }
    build_object(vm, o, &file)
}

fn is_method_literal(o: &ObjLit) -> bool {
    !o.body.is_empty() || !o.args.is_empty()
}

/// Slot initializers are evaluated when the literal is created, as in
/// Expr::Eval in the C++ parser.
fn eval_const(vm: &mut Vm, e: &Expr, file: &Rc<str>) -> Result<Value, String> {
    let o = ObjLit { args: vec![], slots: vec![], body: vec![e.clone()], line: 0 };
    let m = compile_method(vm, &o, "<slot initializer>", file)?;
    let lobby = vm.lobby.clone();
    let scope = interp::new_scope(m, lobby.clone(), lobby, vec![], None);
    interp::run(vm, scope).map_err(|u| interp::describe(vm, u))
}

// ------------------------------------------------------------------ compiler

impl<'a> C<'a> {
    fn mb(&mut self) -> &mut MB {
        self.mbs.last_mut().unwrap()
    }

    fn emit(&mut self, op: u8, idx: usize) {
        let mb = self.mb();
        emit_into(&mut mb.code, op, idx);
    }

    fn no_operand(&mut self, kind: usize) {
        self.mb().code.push((NO_OPERAND << 4) | kind as u8);
    }

    fn lit(&mut self, v: Value, s: Option<Rc<str>>) -> usize {
        let mb = self.mb();
        mb.lits.push(v);
        mb.lit_strs.push(s);
        mb.lits.len() - 1
    }

    fn lit_sel(&mut self, sel: &str) -> usize {
        let v = self.vm.string(sel);
        self.lit(v, Some(sel.into()))
    }

    fn resolve(&self, name: &str) -> Option<(usize, usize, bool)> {
        for (lvl, mb) in self.mbs.iter().rev().enumerate() {
            if let Some(i) = mb.names.iter().position(|n| &**n == name) {
                return Some((lvl, i, mb.mutable[i]));
            }
            if !mb.is_block {
                break; // enclosing methods are not lexically visible
            }
        }
        None
    }

    fn push_method(&mut self, o: &ObjLit, sel: &str, is_block: bool) -> Result<Rc<Method>, String> {
        let mut mb = MB {
            sel: sel.into(),
            nargs: o.args.len(),
            names: vec![],
            mutable: vec![],
            inits: vec![],
            code: vec![],
            lits: vec![],
            lit_strs: vec![],
            is_block,
            line: o.line,
        };
        for a in &o.args {
            mb.names.push(a.as_str().into());
            mb.mutable.push(false);
            mb.inits.push(self.vm.nil.clone());
        }
        let file = self.file.clone();
        for d in &o.slots {
            if d.parent {
                return Err(format!("'{}': parent slots are not allowed in a method", d.name));
            }
            let v = match &d.init {
                Some(Expr::ObjLit(inner)) if is_method_literal(inner) => {
                    return Err(format!("'{}': local methods are not supported", d.name))
                }
                Some(e) => eval_const(self.vm, e, &file)?,
                None => self.vm.nil.clone(),
            };
            if mb.names.iter().any(|n| &**n == d.name.as_str()) {
                return Err(format!("duplicate slot '{}'", d.name));
            }
            mb.names.push(d.name.as_str().into());
            mb.mutable.push(d.mutable);
            mb.inits.push(v);
        }
        self.mbs.push(mb);
        let r = self.body(&o.body);
        let mb = self.mbs.pop().unwrap();
        r?;
        Ok(Rc::new(Method {
            sel: mb.sel,
            nargs: mb.nargs,
            arg_slots: (0..mb.nargs).collect(),
            slot_flags: (0..mb.names.len()).map(|i| if i < mb.nargs { 2 << 2 } else { 0 }).collect(),
            slot_names: mb.names,
            slot_inits: mb.inits,
            code: mb.code,
            lits: mb.lits,
            lit_strs: mb.lit_strs,
            is_block: mb.is_block,
            file: self.file.clone(),
            line: mb.line,
            source: None,
            sites: Default::default(),
        }))
    }

    fn body(&mut self, body: &[Expr]) -> Result<(), String> {
        if body.is_empty() {
            self.no_operand(SELF_C);
            return Ok(());
        }
        for (i, e) in body.iter().enumerate() {
            self.expr(e)?;
            if i + 1 < body.len() {
                self.no_operand(POP_C);
            }
        }
        Ok(())
    }

    fn block(&mut self, o: &ObjLit) -> Result<Value, String> {
        let sel = block_selector(o.args.len());
        let m = self.push_method(o, &sel, true)?;
        let mval = Value::obj([], Payload::Method(m.clone()));
        Ok(Value::obj(
            vec![
                slot("parent", SlotKind::Parent, self.vm.t_block.clone()),
                slot(&sel, SlotKind::Data, mval),
            ],
            Payload::Block(m, None),
        ))
    }

    fn expr(&mut self, e: &Expr) -> Result<(), String> {
        match e {
            Expr::Int(v) => {
                let i = self.lit(Value::Int(*v), None);
                self.emit(LITERAL, i);
            }
            Expr::Float(v) => {
                let i = self.lit(Value::Float(*v), None);
                self.emit(LITERAL, i);
            }
            Expr::Str(b) => {
                // Self canonicalises the string literals in compiled code, so
                // two 'black's anywhere in the world are one object. Its
                // `hash` is `canonicalize identityHash` and `traits
                // canonicalString =` is identity, so a literal that is a fresh
                // object matches nothing and hashes to the wrong bucket.
                let v = match self.vm.canonical.get(b) {
                    Some(v) => v.clone(),
                    None => {
                        let parent = self.vm.t_string.clone();
                        let v = self.vm.bytes_with(parent, b.clone());
                        self.vm.canonical.insert(b.clone(), v.clone());
                        v
                    }
                };
                let i = self.lit(v, None);
                self.emit(LITERAL, i);
            }
            Expr::SelfE => self.no_operand(SELF_C),
            Expr::Return(x) => {
                self.expr(x)?;
                self.no_operand(NLR_C);
            }
            Expr::Block(o) => {
                let v = self.block(o)?;
                let i = self.lit(v, None);
                self.emit(LITERAL, i);
            }
            Expr::ObjLit(o) => {
                if is_method_literal(o) {
                    // `(expr)` -- grouping; the body is inlined here
                    if !o.slots.is_empty() {
                        return Err("slots are not allowed in a parenthesized expression".into());
                    }
                    if !o.args.is_empty() {
                        return Err("argument slots are not allowed here".into());
                    }
                    self.body(&o.body)?;
                } else {
                    let file = self.file.clone();
                    let v = build_object(self.vm, o, &file)?;
                    let i = self.lit(v, None);
                    self.emit(LITERAL, i);
                }
            }
            Expr::Send { recv, sel, args, kind, .. } => {
                if arg_count(sel) != args.len() {
                    return Err(format!("selector '{}' takes {} arguments", sel, arg_count(sel)));
                }
                match recv {
                    Some(r) => {
                        self.expr(r)?;
                        for a in args {
                            self.expr(a)?;
                        }
                        self.send(sel, &SendKind::Normal);
                    }
                    None => {
                        if matches!(kind, SendKind::ImplicitSelf) && self.try_local(sel, args)? {
                            return Ok(());
                        }
                        for a in args {
                            self.expr(a)?;
                        }
                        self.send(sel, kind);
                    }
                }
            }
        }
        Ok(())
    }

    /// Names visible in the lexical scope become direct slot accesses instead
    /// of sends -- this is what makes method locals and block arguments work.
    fn try_local(&mut self, sel: &str, args: &[Expr]) -> Result<bool, String> {
        if args.is_empty() {
            if let Some((lvl, idx, _)) = self.resolve(sel) {
                if lvl > 0 {
                    self.emit(LEXICAL_LEVEL, lvl);
                }
                self.emit(READ_LOCAL, idx);
                return Ok(true);
            }
            return Ok(false);
        }
        if args.len() == 1 && sel.ends_with(':') && sel.matches(':').count() == 1 {
            let base = &sel[..sel.len() - 1];
            if let Some((lvl, idx, mutable)) = self.resolve(base) {
                if !mutable {
                    return Err(format!("'{}' is a constant slot and cannot be assigned", base));
                }
                self.expr(&args[0])?;
                if lvl > 0 {
                    self.emit(LEXICAL_LEVEL, lvl);
                }
                self.emit(WRITE_LOCAL, idx);
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn send(&mut self, sel: &str, kind: &SendKind) {
        match kind {
            SendKind::Normal => {
                let i = self.lit_sel(sel);
                self.emit(SEND, i);
            }
            SendKind::ImplicitSelf => {
                let i = self.lit_sel(sel);
                self.emit(IMPLICIT_SEND, i);
            }
            SendKind::UndirectedResend => {
                self.no_operand(RESEND_C);
                let i = self.lit_sel(sel);
                self.emit(IMPLICIT_SEND, i);
            }
            SendKind::Directed(d) => {
                let di = self.lit_sel(d);
                self.emit(DELEGATEE, di);
                let i = self.lit_sel(sel);
                self.emit(IMPLICIT_SEND, i);
            }
        }
    }
}
