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
use std::collections::HashMap;
use std::rc::Rc;

pub use crate::gc::ObjRef;

thread_local! {
    /// The programming timestamp of the object graph itself: bumped by
    /// anything that can change what a lookup finds -- adding or removing a
    /// slot, writing a parent slot, `_MirrorDefine:` -- and by a collection,
    /// because dead handle ids are recycled. `Vm::lookup` memoises against it.
    static LOOKUP_GEN: Cell<u64> = const { Cell::new(0) };
}

pub fn lookup_gen_bump() {
    LOOKUP_GEN.with(|g| g.set(g.get() + 1));
}

pub fn lookup_gen() -> u64 {
    LOOKUP_GEN.with(|g| g.get())
}

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

/// An interned slot name. Two things want this. A `Slot` has to be `Copy` for
/// the young generation to abandon a from-space by resetting a length rather
/// than by running a destructor per dead slot, and an `Rc<str>` has one. And
/// comparing names is what lookup spends its time on, which a `u32` compare
/// settles without touching the string at all.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Sym(u32);

/// Pre-interned, because interning hashes the name and this one is on the
/// allocation path: every string and byte vector is born with a parent slot.
pub const SYM_PARENT: Sym = Sym(0);
const PREINTERNED: [&str; 1] = ["parent"];

thread_local! {
    /// Names are leaked, so `sym_str` can hand out a `&'static str` with no
    /// guard to hold and nothing to clone -- the same trade the heap makes.
    /// A world interns its slot names once; there is no unbounded source of
    /// them to make the leak grow.
    static SYMS: RefCell<(Vec<&'static str>, HashMap<&'static str, Sym>)> =
        RefCell::new({
            let mut names = vec![];
            let mut ids = HashMap::new();
            for (i, n) in PREINTERNED.iter().enumerate() {
                names.push(*n);
                ids.insert(*n, Sym(i as u32));
            }
            (names, ids)
        });
}

pub fn sym(name: &str) -> Sym {
    SYMS.with(|t| {
        let mut t = t.borrow_mut();
        if let Some(&s) = t.1.get(name) {
            return s;
        }
        let leaked: &'static str = Box::leak(name.to_string().into_boxed_str());
        let s = Sym(t.0.len() as u32);
        t.0.push(leaked);
        t.1.insert(leaked, s);
        s
    })
}

pub fn sym_str(s: Sym) -> &'static str {
    SYMS.with(|t| t.borrow().0[s.0 as usize])
}

impl From<&str> for Sym {
    fn from(s: &str) -> Sym {
        sym(s)
    }
}

#[cfg(test)]
mod sym_tests {
    use super::*;

    /// `SYM_PARENT` is a constant, so it is only right for as long as the
    /// table really does hand out the pre-interned names first, in order.
    #[test]
    fn the_preinterned_names_keep_their_numbers() {
        for (i, n) in PREINTERNED.iter().enumerate() {
            assert_eq!(sym(n), Sym(i as u32), "{n}");
            assert_eq!(sym_str(Sym(i as u32)), *n);
        }
        assert_eq!(sym("parent"), SYM_PARENT);
        // interning is by content: a name built at runtime is the same symbol
        assert_eq!(sym(&format!("par{}", "ent")), SYM_PARENT);
        assert_ne!(sym("parent "), SYM_PARENT);
    }
}

impl std::fmt::Display for Sym {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(sym_str(*self))
    }
}

#[derive(Clone, Copy)]
pub struct Slot {
    pub name: Sym,
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
    /// Inline caches, one per literal; sized on the method's first send. Not
    /// a GC root: an entry is only read at the generation it was filled at,
    /// and a collection bumps that.
    pub sites: RefCell<Vec<Site>>,
}

/// A send site's inline cache: the last receiver seen there, and what lookup
/// found for it. serf's compiler emits a fresh literal per send, so a literal
/// index *is* a site id; the C++ compiler shares literals between sends of the
/// same selector, where this degrades to one entry per (method, selector).
///
/// ponytail: monomorphic -- one entry, no polymorphic slots. Most sites never
/// see a second receiver; widen it to 4 when a miss counter says otherwise.
#[derive(Clone, Copy)]
pub struct Site {
    /// arity of the selector in this literal, a pure function of it -- so
    /// unlike `hit` it never goes stale. `u32::MAX` until first computed.
    nargs: u32,
    /// receiver key, what it found, and the `LOOKUP_GEN` that was true at
    hit: Option<(u64, ObjRef, Hit)>,
}

impl Default for Site {
    fn default() -> Site {
        Site { nargs: u32::MAX, hit: None }
    }
}

impl Method {
    fn sites(&self) -> std::cell::RefMut<'_, Vec<Site>> {
        let mut s = self.sites.borrow_mut();
        if s.len() < self.lits.len() {
            s.resize(self.lits.len(), Site::default());
        }
        s
    }

    /// How many arguments the selector in literal `i` takes.
    pub fn site_nargs(&self, i: usize, sel: &str) -> usize {
        let mut s = self.sites();
        if s[i].nargs == u32::MAX {
            s[i].nargs = crate::compile::arg_count(sel) as u32;
        }
        s[i].nargs as usize
    }

    /// What site `i` last found for receiver key `k`, if that is still good.
    pub fn site_hit(&self, i: usize, k: ObjRef) -> Option<Hit> {
        match self.sites()[i].hit {
            Some((g, r, h)) if r == k && g == lookup_gen() => Some(h),
            _ => None,
        }
    }

    pub fn site_fill(&self, i: usize, k: ObjRef, h: Hit) {
        self.sites()[i].hit = Some((lookup_gen(), k, h));
    }
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

/// An activation's slot buffer goes back to the pool rather than to malloc.
/// Dropping the from-space was more than half spent here: a block in the young
/// generation holds its captured activation, so a scavenge that collects the
/// block frees the activation too, and `free` on this thread ends in `madvise`.
impl Drop for Scope {
    fn drop(&mut self) {
        give_vals(std::mem::take(self.slots.get_mut()));
    }
}

thread_local! {
    /// Recycled `Vec<Value>` buffers. An activation's slots and a send's
    /// arguments have the same shape and the same lifetime -- born at a send,
    /// dead a moment later -- so they come from here and go back, and in the
    /// steady state a send allocates nothing at all.
    /// ponytail: a plain stack, not a size-classed pool; every buffer in it is
    /// a handful of `Value`s wide.
    static VALS: RefCell<Vec<Vec<Value>>> = const { RefCell::new(Vec::new()) };
}

/// Cap on both how many buffers are kept and how big a kept one may be: a
/// method with hundreds of slots is rare enough that recycling its buffer
/// would cost more memory than it saves.
const VALS_KEPT: usize = 256;
const VALS_WIDEST: usize = 64;

pub fn take_vals() -> Vec<Value> {
    VALS.try_with(|p| p.borrow_mut().pop()).ok().flatten().unwrap_or_default()
}

thread_local! {
    /// Activations nobody holds any more. An `Rc` cannot hand its allocation
    /// back as it drops, so a frame that has finished with one offers it here
    /// instead: uniquely owned, it can be refilled in place by the next send,
    /// slot buffer and all. An activation some block captured has a second
    /// reference and is declined, and freed the ordinary way.
    static SCOPES: RefCell<Vec<Rc<Scope>>> = const { RefCell::new(Vec::new()) };
}

const SCOPES_KEPT: usize = 256;

/// Offer a finished activation to the pool. Declines any that is still
/// referenced -- that is what makes refilling it in place sound.
pub fn give_scope(mut s: Rc<Scope>) {
    let Some(m) = Rc::get_mut(&mut s) else { return };
    // park it holding nothing: its lexical chain and the values in its slots
    // must not stay reachable just because the buffer is worth keeping
    m.lexical = None;
    m.home = None;
    m.slots.get_mut().clear();
    // `try_with`, not `with`: at thread exit these two pools are destroyed in
    // an order neither of them picks, and dropping a parked activation runs
    // `Scope::drop`, which reaches for the other one. Losing a buffer on the
    // way out costs nothing.
    let _ = SCOPES.try_with(|p| {
        let mut p = p.borrow_mut();
        if p.len() < SCOPES_KEPT {
            p.push(s);
        }
    });
}

pub fn take_scope() -> Option<Rc<Scope>> {
    SCOPES.try_with(|p| p.borrow_mut().pop()).ok().flatten()
}

pub fn give_vals(mut v: Vec<Value>) {
    if v.capacity() == 0 || v.capacity() > VALS_WIDEST {
        return;
    }
    v.clear(); // `Value` is `Copy`, so this is just a length store
    let _ = VALS.try_with(|p| {
        let mut p = p.borrow_mut();
        if p.len() < VALS_KEPT {
            p.push(v);
        }
    });
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
    Slot { name: sym(name), kind, value }
}

impl Obj {
    /// By interned name. Every hot caller already has one; `find` is for the
    /// cold ones that hold a string and pay a hash to get here.
    pub fn find_sym(&self, name: Sym) -> Option<usize> {
        self.slots.iter().position(|s| s.name == name)
    }
    pub fn find(&self, name: &str) -> Option<usize> {
        self.find_sym(sym(name))
    }
    /// Add or replace by name.
    pub fn put(&mut self, s: Slot) {
        lookup_gen_bump();
        match self.find_sym(s.name) {
            Some(i) => self.slots[i] = s,
            None => self.slots.push(s),
        }
    }
    /// Write a slot's value. A parent write rewires the object graph and so
    /// invalidates memoised lookups; a data write cannot change one.
    pub fn assign(&mut self, i: usize, v: Value) {
        if self.slots[i].kind == SlotKind::Parent {
            lookup_gen_bump();
        }
        self.slots[i].value = v;
    }
}

// ---------------------------------------------------------------- lookup

#[derive(Clone, Copy)]
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
#[derive(Clone, Copy)]
pub struct Hit {
    pub holder: ObjRef,
    pub idx: usize,
}

/// Memoised `lookup` results, selector -> start object -> outcome. Entries are
/// valid only for the `LOOKUP_GEN` they were filled at; the whole cache is
/// dropped when the generation moves, which also covers recycled handle ids.
/// ponytail: whole-cache flush on any shape change; per-object invalidation
/// if a mutation-heavy workload ever thrashes it.
#[derive(Default)]
pub struct LookupCache {
    gen: u64,
    len: usize,
    map: HashMap<Rc<str>, HashMap<ObjRef, Result<Hit, LookupErr>>>,
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
    /// Annotation values that may still be young. The two tables above are
    /// Rust side, so they cannot be in the collector's remembered set; this
    /// is their write barrier, and a scavenge takes it as the root set rather
    /// than walking a loaded world's couple of hundred thousand entries. An
    /// old annotation needs no barrier: old objects neither move nor die
    /// outside a major collection, and a major walks both tables in full.
    pub anno_young: Vec<Value>,
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
    /// see `lookup`; not a GC root -- entries never outlive their generation,
    /// and a collection bumps it
    pub lookup_cache: RefCell<LookupCache>,
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
            anno_young: vec![],
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
            lookup_cache: RefCell::new(LookupCache::default()),
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
    /// `major` when the caller is a mark & sweep, which may free an old
    /// object and so has to see every reference the VM holds. A scavenge only
    /// needs the ones that could name something young.
    pub fn each_root(&self, major: bool, f: &mut dyn FnMut(Root)) {
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
        // A loaded world has one of these per annotated slot -- a couple of
        // hundred thousand for morphic, where walking them was about a third
        // of every scavenge (5.8ms -> 3.9ms once it stopped). Only a major
        // has to see them: an old object neither moves nor dies outside one,
        // so a scavenge takes the write barrier's list instead.
        if major {
            for x in self.anno_obj.values().chain(self.anno_slot.values()) {
                v(x);
            }
        } else {
            for x in self.anno_young.iter() {
                v(x);
            }
        }
        for x in self.temp_roots.iter() {
            v(x);
        }
        for fs in self.stacks.iter().chain(self.procs.values()) {
            crate::interp::frame_roots(fs, f);
        }
    }

    /// Record an annotation value as a scavenge root. Call after every insert
    /// into `anno_obj` or `anno_slot`; an old value costs nothing but one
    /// scavenge's worth of list.
    pub fn note_anno(&mut self, v: &Value) {
        if matches!(v, Value::Obj(_)) {
            self.anno_young.push(*v);
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
        Value::obj(vec![Slot { name: SYM_PARENT, kind: SlotKind::Parent, value: parent }], Payload::Bytes(b))
    }

    pub fn vector(&self, v: Vec<Value>) -> Value {
        Value::obj(
            vec![Slot { name: SYM_PARENT, kind: SlotKind::Parent, value: self.t_vector }],
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

    /// The object a search actually starts from, and so what a memoised
    /// lookup is keyed on: an immediate has none of its own slots, so it
    /// starts in its traits and every int shares one entry.
    pub fn lookup_key(&self, recv: &Value) -> ObjRef {
        match recv.as_obj() {
            Some(o) => o,
            None => self.implicit_parent(recv).unwrap().as_obj().unwrap(),
        }
    }

    pub fn lookup(&self, recv: &Value, sel: &str) -> Result<Hit, LookupErr> {
        let key = self.lookup_key(recv);
        let gen = lookup_gen();
        {
            let mut c = self.lookup_cache.borrow_mut();
            if c.gen != gen {
                c.map.clear();
                c.len = 0;
                c.gen = gen;
            } else if let Some(r) = c.map.get(sel).and_then(|m| m.get(&key)) {
                return *r;
            }
        }
        let mut hits: Vec<Hit> = vec![];
        let mut seen: Vec<ObjRef> = vec![];
        self.search(recv, sel, &mut hits, &mut seen);
        let r = match hits.len() {
            0 => Err(LookupErr::NotFound),
            1 => Ok(hits[0]),
            _ => Err(LookupErr::Ambiguous),
        };
        let mut c = self.lookup_cache.borrow_mut();
        // ponytail: crude size cap; an LRU if a world ever legitimately holds
        // this many live (receiver, selector) pairs
        if c.len >= 1 << 20 {
            c.map.clear();
            c.len = 0;
        }
        match c.map.get_mut(sel) {
            Some(m) => {
                m.insert(key, r);
            }
            None => {
                c.map.entry(sel.into()).or_default().insert(key, r);
            }
        }
        c.len += 1;
        r
    }

    /// Undirected resend: skip the holder's own slots, search only its parents.
    pub fn lookup_in_parents(&self, holder: &Value, sel: &str) -> Result<Hit, LookupErr> {
        let mut hits: Vec<Hit> = vec![];
        let mut seen: Vec<ObjRef> = vec![];
        if let Some(o) = holder.as_obj() {
            seen.push(o);
            self.search_parents(o, sel, &mut hits, &mut seen);
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
        self.search_parents(o, sel, hits, seen);
    }

    /// Recurse into each parent, re-borrowing per slot rather than collecting
    /// the parents into a vector: nothing can mutate the object mid-search.
    fn search_parents(&self, o: ObjRef, sel: &str, hits: &mut Vec<Hit>, seen: &mut Vec<ObjRef>) {
        let n = o.borrow().slots.len();
        for i in 0..n {
            let b = o.borrow();
            let s = &b.slots[i];
            if s.kind != SlotKind::Parent {
                continue;
            }
            let p = s.value;
            drop(b);
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
