//! Object model: prototypes, slots, multiple-parent lookup.
//!
//! Follows vm/src/any/objects: everything is an object with named slots;
//! inheritance is delegation through slots marked as parents. Unlike the C++
//! VM there are no maps -- every object carries its own slot vector.
//! ponytail: no maps.
//!
//! An object lives in the heap in `gc.rs`; a `Value` names one by handle, so a
//! `Value` is `Copy` and identity is the handle.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

pub use crate::gc::ObjRef;

#[derive(Clone, Copy)]
pub enum Value {
    Int(i64),
    Float(f64),
    Obj(ObjRef),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SlotKind {
    Data,
    Parent,
    /// `x:` -- writes the data slot named by `Slot::name` minus its trailing colon.
    Assign,
}

#[derive(Clone)]
pub struct Slot {
    pub name: Rc<str>,
    pub kind: SlotKind,
    pub value: Value,
}

pub struct Obj {
    pub slots: Vec<Slot>,
    pub payload: Payload,
}

pub enum Payload {
    None,
    /// strings and byte vectors; they differ only in their parent
    Bytes(Vec<u8>),
    Vector(Vec<Value>),
    Method(Rc<Method>),
    /// block prototype (`None`) or a block closed over an activation
    Block(Rc<Method>, Option<Rc<Scope>>),
    /// a mirror: the reflectee lives in a fixed word, not in a slot
    Mirror(Value),
    /// a foreign pointer held by a proxy object. `None` is a *dead* proxy:
    /// Self kills one by stamping its type seal, not by nulling its pointer,
    /// so a live proxy may legitimately point at 0 -- `stdin` is fd 0.
    Proxy(Option<u64>),
}

/// Compiled code. Args occupy slot indices `0..nargs`, locals follow.
pub struct Method {
    pub sel: Rc<str>,
    pub nargs: usize,
    /// indices in `slot_inits` that receive the arguments, in order
    pub arg_slots: Vec<usize>,
    pub slot_names: Vec<Rc<str>>,
    /// raw Self slotType bits per slot (class, parent, vm), so a loaded
    /// method's descriptors survive being written back out
    pub slot_flags: Vec<u32>,
    pub slot_inits: Vec<Value>,
    pub code: Vec<u8>,
    pub lits: Vec<Value>,
    /// selector literals pre-decoded, so sends don't rebuild a String each time
    pub lit_strs: Vec<Option<Rc<str>>>,
    pub is_block: bool,
    pub file: Rc<str>,
    pub line: u32,
    /// methodMap's `_source`, plus the slice of it that is this method
    /// (`_sourceOffset`/`_sourceLen`, non-zero only for a block, whose source
    /// string is its enclosing method's). Only a loaded method has one: serf's
    /// parser does not record source spans.
    /// ponytail: None means "no source"; give the parser spans if editing
    /// methods from within a loaded world ever needs to write one back.
    pub source: Option<(Value, i64, i64)>,
}

/// One activation's mutable state. Lives on the heap because blocks capture it.
pub struct Scope {
    pub method: Rc<Method>,
    pub recv: Value,
    /// object that held the activated slot; start point for undirected resends
    pub holder: Value,
    pub slots: RefCell<Vec<Value>>,
    pub lexical: Option<Rc<Scope>>,
    /// `None` means "I am the home" (an ordinary method)
    pub home: Option<Rc<Scope>>,
    pub dead: Cell<bool>,
}

impl Scope {
    pub fn home_of(s: &Rc<Scope>) -> Rc<Scope> {
        s.home.clone().unwrap_or_else(|| s.clone())
    }
}

impl Value {
    pub fn obj(slots: Vec<Slot>, payload: Payload) -> Value {
        Value::Obj(crate::gc::alloc(slots, payload))
    }
    pub fn id_eq(&self, o: &Value) -> bool {
        match (self, o) {
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a == b,
            (Value::Obj(a), Value::Obj(b)) => a == b,
            _ => false,
        }
    }
    pub fn as_obj(&self) -> Option<ObjRef> {
        match self {
            Value::Obj(o) => Some(*o),
            _ => None,
        }
    }
    pub fn method(&self) -> Option<Rc<Method>> {
        match self {
            Value::Obj(o) => match &o.borrow().payload {
                Payload::Method(m) => Some(m.clone()),
                _ => None,
            },
            _ => None,
        }
    }
    pub fn bytes(&self) -> Option<Vec<u8>> {
        match self {
            Value::Obj(o) => match &o.borrow().payload {
                Payload::Bytes(b) => Some(b.clone()),
                _ => None,
            },
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<String> {
        self.bytes().map(|b| String::from_utf8_lossy(&b).into_owned())
    }
}

pub fn slot(name: &str, kind: SlotKind, value: Value) -> Slot {
    Slot { name: name.into(), kind, value }
}

impl Obj {
    pub fn find(&self, name: &str) -> Option<usize> {
        self.slots.iter().position(|s| &*s.name == name)
    }
    /// Add or replace by name.
    pub fn put(&mut self, s: Slot) {
        match self.find(&s.name) {
            Some(i) => self.slots[i] = s,
            None => self.slots.push(s),
        }
    }
}

// ---------------------------------------------------------------- lookup

pub enum LookupErr {
    NotFound,
    Ambiguous,
}

/// One edge into the object graph. A `Scope` is not a heap object -- it is
/// reference counted, and holding one alive keeps the `Value`s inside it alive
/// too -- so the collector has to be told about it separately. A `Method` needs
/// no such case: one is only ever reached through the scope it runs in or the
/// object that holds it.
pub enum Root {
    Val(Value),
    Scope(Rc<Scope>),
}

/// Where a selector was found: a slot index in some holder object.
pub struct Hit {
    pub holder: ObjRef,
    pub idx: usize,
}

pub struct Vm {
    pub lobby: Value,
    pub globals: Value,
    pub nil: Value,
    pub tru: Value,
    pub fals: Value,
    pub t_smallint: Value,
    pub t_float: Value,
    pub t_string: Value,
    pub t_bytevector: Value,
    pub t_vector: Value,
    pub t_block: Value,
    /// bumped by the interpreter, checked to keep runaway recursion from
    /// eating the machine
    pub max_frames: usize,
    /// a loaded image brings its own true/false; branch bytecodes must know them
    pub image_true: Option<Value>,
    pub image_false: Option<Value>,
    /// the 40 VM roots of a loaded image, so it can be written back out
    pub image_roots: Option<Vec<Value>>,
    pub image_strings: Option<Vec<Value>>,
    /// Self keeps annotations and the object's kind in its map; serf has no
    /// maps, so a loaded image parks them here keyed by object identity, and
    /// the image writer puts them back.
    pub anno_obj: std::collections::HashMap<usize, Value>,
    pub anno_slot: std::collections::HashMap<(usize, String), Value>,
    /// Identity hashes carried over from a snapshot. Self keeps an object's
    /// hash in its mark word, so hashed collections survive a save; computing
    /// a fresh one on load would leave every dictionary in the image unable
    /// to find its own keys.
    pub id_hash: std::collections::HashMap<usize, i64>,
    pub obj_kind: std::collections::HashMap<usize, u8>,
    /// dynamically loaded C libraries, for the image's glue primitives
    pub ffi: crate::ffi::Ffi,
    /// parked Self process stacks, keyed by process object identity
    pub procs: std::collections::HashMap<usize, Vec<crate::interp::Frame>>,
    /// frame stacks lent to the Vm while the interpreter is somewhere it could
    /// collect. A nested run (the scheduler stepping a process, a fail block,
    /// `_Parse…`) leaves the outer stacks as Rust locals, where the collector
    /// cannot see them, so `run_stack` lends its own across anything that
    /// might re-enter.
    pub stacks: Vec<Vec<crate::interp::Frame>>,
    /// values held in Rust locals across a call that can re-enter the
    /// interpreter, and so across a collection
    pub temp_roots: Vec<Value>,
    /// the process the scheduler is currently running, for _ThisProcess
    pub current_proc: Option<Value>,
    /// stand-in process for code run outside the scheduler, which must not be
    /// identical to the world's schedulerProcess or its error handler
    /// resets its own recursion guard forever
    pub vm_proc: Option<Value>,
    pub signals_blocked: bool,
    /// true while the world's own scheduler loop is on the stack
    pub scheduler_running: bool,
    /// the world's programming timestamp, bumped as it changes itself
    pub timestamp: i64,
    /// C structs handed to foreign code; kept alive for the VM's lifetime
    pub c_heap: Vec<Vec<u8>>,
    pub flags: std::collections::HashMap<String, Value>,
    /// Self canonicalises strings: `traits canonicalString =` tests identity
    /// first, so a string serf hands the world must be the world's own object
    /// or it compares unequal to every literal.
    pub canonical: std::collections::HashMap<Vec<u8>, Value>,
}

impl Vm {
    pub fn new() -> Vm {
        let mk = || Value::obj(vec![], Payload::None);
        let nil = mk();
        let tru = mk();
        let fals = mk();
        let t_object = mk();
        let t_smallint = mk();
        let t_float = mk();
        let t_string = mk();
        let t_bytevector = mk();
        let t_vector = mk();
        let t_block = mk();
        let traits = Value::obj(
            vec![
                slot("object", SlotKind::Data, t_object.clone()),
                slot("smallInt", SlotKind::Data, t_smallint.clone()),
                slot("float", SlotKind::Data, t_float.clone()),
                slot("string", SlotKind::Data, t_string.clone()),
                slot("byteVector", SlotKind::Data, t_bytevector.clone()),
                slot("vector", SlotKind::Data, t_vector.clone()),
                slot("block", SlotKind::Data, t_block.clone()),
            ],
            Payload::None,
        );
        let globals = Value::obj(
            vec![
                slot("traits", SlotKind::Data, traits),
                slot("nil", SlotKind::Data, nil.clone()),
                slot("true", SlotKind::Data, tru.clone()),
                slot("false", SlotKind::Data, fals.clone()),
            ],
            Payload::None,
        );
        let lobby = Value::obj(
            vec![slot("globals", SlotKind::Parent, globals.clone())],
            Payload::None,
        );
        // prototypes: Self code cannot conjure an indexable out of nothing
        let string_proto = Value::obj(
            vec![slot("parent", SlotKind::Parent, t_string.clone())],
            Payload::Bytes(vec![]),
        );
        let bv_proto = Value::obj(
            vec![slot("parent", SlotKind::Parent, t_bytevector.clone())],
            Payload::Bytes(vec![]),
        );
        let vec_proto = Value::obj(
            vec![slot("parent", SlotKind::Parent, t_vector.clone())],
            Payload::Vector(vec![]),
        );
        {
            let mut g = globals.as_obj().unwrap().borrow_mut();
            g.put(slot("lobby", SlotKind::Data, lobby.clone()));
            g.put(slot("globals", SlotKind::Data, globals.clone()));
            g.put(slot("string", SlotKind::Data, string_proto));
            g.put(slot("byteVector", SlotKind::Data, bv_proto));
            g.put(slot("vector", SlotKind::Data, vec_proto));
        }
        Vm {
            lobby,
            globals,
            nil,
            tru,
            fals,
            t_smallint,
            t_float,
            t_string,
            t_bytevector,
            t_vector,
            t_block,
            max_frames: 500_000,
            image_true: None,
            image_false: None,
            image_roots: None,
            image_strings: None,
            anno_obj: std::collections::HashMap::new(),
            anno_slot: std::collections::HashMap::new(),
            id_hash: std::collections::HashMap::new(),
            obj_kind: std::collections::HashMap::new(),
            ffi: crate::ffi::Ffi::default(),
            procs: std::collections::HashMap::new(),
            stacks: vec![],
            temp_roots: vec![],
            current_proc: None,
            vm_proc: None,
            signals_blocked: false,
            scheduler_running: false,
            timestamp: 0,
            c_heap: vec![],
            flags: std::collections::HashMap::new(),
            canonical: std::collections::HashMap::new(),
        }
    }

    /// Everything the collector must treat as live. Miss one and the object it
    /// names is collected out from under the VM, so this walks every field of
    /// `Vm` that can hold a `Value`, however deeply.
    ///
    /// The address-keyed side tables are handled as weak-key tables: their
    /// contents are traced here, and `sweep_weak` then drops whole entries
    /// whose key object turned out to be dead. An entry therefore outlives its
    /// object by one collection, which costs nothing and avoids a fixpoint.
    pub fn each_root(&self, f: &mut dyn FnMut(Root)) {
        let mut v = |x: &Value| f(Root::Val(*x));
        for x in [
            &self.lobby,
            &self.globals,
            &self.nil,
            &self.tru,
            &self.fals,
            &self.t_smallint,
            &self.t_float,
            &self.t_string,
            &self.t_bytevector,
            &self.t_vector,
            &self.t_block,
        ] {
            v(x);
        }
        for x in [&self.image_true, &self.image_false, &self.current_proc, &self.vm_proc] {
            if let Some(x) = x {
                v(x);
            }
        }
        for x in self.image_roots.iter().chain(self.image_strings.iter()).flatten() {
            v(x);
        }
        // canonical strings are strong, as the C++ VM's string table is at a
        // scavenge (stringTable.hh:87). ponytail: prune them at a major
        // collection if a long session ever accumulates too many.
        for x in self.canonical.values().chain(self.flags.values()) {
            v(x);
        }
        // ponytail: a loaded world has one of these per annotated slot -- a
        // couple of hundred thousand for morphic -- and a scavenge walks them
        // all to find the handful that are young. Worth a young/old split of
        // the tables only if the young generation is ever made small enough
        // for that constant to show.
        for x in self.anno_obj.values().chain(self.anno_slot.values()) {
            v(x);
        }
        for x in self.temp_roots.iter() {
            v(x);
        }
        for fs in self.stacks.iter().chain(self.procs.values()) {
            crate::interp::frame_roots(fs, f);
        }
    }

    /// Drop side-table entries whose object died. Handle ids are recycled, so a
    /// stale entry would otherwise reattach an annotation or an identity hash
    /// to an unrelated object later on.
    pub fn sweep_weak(&mut self, dead: &dyn Fn(usize) -> bool) {
        self.anno_obj.retain(|k, _| !dead(*k));
        self.anno_slot.retain(|(k, _), _| !dead(*k));
        self.id_hash.retain(|k, _| !dead(*k));
        self.obj_kind.retain(|k, _| !dead(*k));
        self.procs.retain(|k, _| !dead(*k));
    }

    pub fn is_false(&self, v: &Value) -> bool {
        v.id_eq(&self.fals) || self.image_false.as_ref().map_or(false, |f| v.id_eq(f))
    }

    /// Primitives must answer with the *image's* booleans when one is loaded,
    /// or the world's `ifTrue:` finds no methods on them.
    /// nil, the image's if one is loaded.
    pub fn nil_v(&self) -> Value {
        self.image_roots.as_ref().map_or_else(|| self.nil.clone(), |r| r[1].clone())
    }

    pub fn boolean(&self, b: bool) -> Value {
        if b {
            self.image_true.clone().unwrap_or_else(|| self.tru.clone())
        } else {
            self.image_false.clone().unwrap_or_else(|| self.fals.clone())
        }
    }

    pub fn string(&self, s: &str) -> Value {
        if let Some(v) = self.canonical.get(s.as_bytes()) {
            return v.clone();
        }
        self.bytes_with(self.t_string.clone(), s.as_bytes().to_vec())
    }

    pub fn bytes_with(&self, parent: Value, b: Vec<u8>) -> Value {
        Value::obj(vec![slot("parent", SlotKind::Parent, parent)], Payload::Bytes(b))
    }

    pub fn vector(&self, v: Vec<Value>) -> Value {
        Value::obj(
            vec![slot("parent", SlotKind::Parent, self.t_vector.clone())],
            Payload::Vector(v),
        )
    }

    /// The receiver object whose slots are searched first. Immediates have
    /// none of their own, so lookup starts in their traits.
    fn implicit_parent(&self, v: &Value) -> Option<Value> {
        match v {
            Value::Int(_) => Some(self.t_smallint.clone()),
            Value::Float(_) => Some(self.t_float.clone()),
            Value::Obj(_) => None,
        }
    }

    pub fn lookup(&self, recv: &Value, sel: &str) -> Result<Hit, LookupErr> {
        let mut hits: Vec<Hit> = vec![];
        let mut seen: Vec<ObjRef> = vec![];
        self.search(recv, sel, &mut hits, &mut seen);
        match hits.len() {
            0 => Err(LookupErr::NotFound),
            1 => Ok(hits.pop().unwrap()),
            _ => Err(LookupErr::Ambiguous),
        }
    }

    /// Undirected resend: skip the holder's own slots, search only its parents.
    pub fn lookup_in_parents(&self, holder: &Value, sel: &str) -> Result<Hit, LookupErr> {
        let mut hits: Vec<Hit> = vec![];
        let mut seen: Vec<ObjRef> = vec![];
        if let Some(o) = holder.as_obj() {
            seen.push(o);
            let parents: Vec<Value> = o
                .borrow()
                .slots
                .iter()
                .filter(|s| s.kind == SlotKind::Parent)
                .map(|s| s.value.clone())
                .collect();
            for p in parents {
                self.search(&p, sel, &mut hits, &mut seen);
            }
        }
        match hits.len() {
            0 => Err(LookupErr::NotFound),
            1 => Ok(hits.pop().unwrap()),
            _ => Err(LookupErr::Ambiguous),
        }
    }

    fn search(&self, recv: &Value, sel: &str, hits: &mut Vec<Hit>, seen: &mut Vec<ObjRef>) {
        let o = match recv {
            Value::Obj(o) => *o,
            _ => {
                let p = self.implicit_parent(recv).unwrap();
                return self.search(&p, sel, hits, seen);
            }
        };
        if seen.contains(&o) {
            return;
        }
        seen.push(o);
        if let Some(idx) = o.borrow().find(sel) {
            if !hits.iter().any(|h| h.holder == o && h.idx == idx) {
                hits.push(Hit { holder: o, idx });
            }
            return; // a local slot shadows every parent
        }
        let parents: Vec<Value> = o
            .borrow()
            .slots
            .iter()
            .filter(|s| s.kind == SlotKind::Parent)
            .map(|s| s.value.clone())
            .collect();
        for p in parents {
            self.search(&p, sel, hits, seen);
        }
    }
}

// ---------------------------------------------------------------- printing

pub fn fmt_float(f: f64) -> String {
    if f.is_finite() && f == f.trunc() && f.abs() < 1e16 {
        format!("{:.1}", f)
    } else {
        format!("{}", f)
    }
}

/// VM-level fallback printer, used before the Self-level `printString` exists.
pub fn default_print_string(vm: &Vm, v: &Value) -> String {
    match v {
        Value::Int(i) => i.to_string(),
        Value::Float(f) => fmt_float(*f),
        Value::Obj(o) => {
            if v.id_eq(&vm.nil) {
                return "nil".into();
            }
            if v.id_eq(&vm.tru) {
                return "true".into();
            }
            if v.id_eq(&vm.fals) {
                return "false".into();
            }
            let b = o.borrow();
            match &b.payload {
                Payload::Bytes(bytes) => format!("'{}'", String::from_utf8_lossy(bytes)),
                Payload::Vector(items) => {
                    let parts: Vec<String> =
                        items.iter().map(|i| default_print_string(vm, i)).collect();
                    format!("({}. )", parts.join(". "))
                }
                Payload::Method(m) => format!("<method {}>", m.sel),
                Payload::Block(m, _) => format!("<block {}>", m.sel),
                Payload::Mirror(_) => "<mirror>".to_string(),
                Payload::Proxy(p) => match p {
                    Some(p) => format!("<proxy {:#x}>", p),
                    None => "<dead proxy>".into(),
                },
                Payload::None => {
                    let names: Vec<String> = b
                        .slots
                        .iter()
                        .map(|s| {
                            format!("{}{}", s.name, if s.kind == SlotKind::Parent { "*" } else { "" })
                        })
                        .collect();
                    format!("(| {} |)", names.join(". "))
                }
            }
        }
    }
}
