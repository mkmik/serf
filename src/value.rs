//! Object model: prototypes, slots, multiple-parent lookup.
//!
//! Follows vm/src/any/objects: everything is an object with named slots;
//! inheritance is delegation through slots marked as parents.
//!
//! An object is a run of words in the arena (`heap.rs`), laid out by `obj.rs`,
//! and a `Value` naming one holds its address. A `Value` is `Copy`, and
//! identity is the address -- which moves, so nothing outside the heap may key
//! on it across a collection.
//!
//! An object's *shape* -- slot names and kinds, and the parent values a search
//! recurses into -- is interned as a `MapRef`, the way the C++ VM interns a
//! map, and that is what the method caches key on: a thousand clones of one
//! prototype share one shape and no two of them share an identity.
//! ponytail: the map is only the key; the descriptors themselves are still per
//! object, which is what a real map would share.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use crate::heap::{self, Kind, Oop};
use crate::obj::{self, record_if_old};
pub use crate::obj::{act, Payload, SlotKind};

/// An object's address. Stable only between collections.
pub type ObjRef = Oop;

thread_local! {
    /// The programming timestamp of the object graph itself: bumped by
    /// anything that can change what a lookup finds -- adding or removing a
    /// slot, writing a parent slot, `_MirrorDefine:` -- and by a collection,
    /// because a shape is keyed on parent addresses and a collection moves
    /// them. `Vm::lookup` memoises against it.
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

// ---------------------------------------------------------------- symbols

/// An interned slot name. Comparing names is what lookup spends its time on,
/// which a `u32` compare settles without touching the string at all -- and a
/// number is not a reference, so a descriptor holding one sits in the part of
/// an object the collector does not walk.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Sym(u32);

impl Sym {
    pub fn id(self) -> u32 {
        self.0
    }
    pub fn from_id(i: u32) -> Sym {
        Sym(i)
    }
}

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

impl std::fmt::Display for Sym {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(sym_str(*self))
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
        assert_eq!(sym(&format!("par{}", "ent")), SYM_PARENT);
        assert_ne!(sym("parent "), SYM_PARENT);
    }
}

// ------------------------------------------------------------------ slots

/// One slot, as a caller sees it. Objects store the name and kind packed into
/// a descriptor word and the value in the traced region, so this is a view
/// assembled on read and taken apart on write -- never a thing in the heap.
#[derive(Clone, Copy)]
pub struct Slot {
    pub name: Sym,
    pub kind: SlotKind,
    pub value: Value,
}

pub fn slot(name: &str, kind: SlotKind, value: Value) -> Slot {
    Slot { name: sym(name), kind, value }
}

/// An object's slots. A `Copy` handle rather than a vector: `len`, `iter` and
/// `for s in &slots` read as they always did, and only indexing became `get`,
/// because there is no `&Slot` to hand back from a heap that moves.
#[derive(Clone, Copy)]
pub struct SlotsRef(ObjRef);

impl SlotsRef {
    pub fn len(&self) -> usize {
        obj::slot_count(self.0)
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn get(&self, i: usize) -> Slot {
        Slot {
            name: obj::slot_name(self.0, i),
            kind: obj::slot_kind(self.0, i),
            value: obj::slot_value(self.0, i),
        }
    }
    pub fn iter(&self) -> SlotsIter {
        SlotsIter { o: self.0, i: 0, n: self.len() }
    }
}

/// Reading slots must not allocate. An earlier draft collected into a `Vec` to
/// satisfy `IntoIterator`, which put a `malloc` behind every `for s in &slots`
/// in the VM -- one of the two reasons the malloc count went *up* across the
/// switch-over before this.
pub struct SlotsIter {
    o: ObjRef,
    i: usize,
    n: usize,
}

impl Iterator for SlotsIter {
    type Item = Slot;
    fn next(&mut self) -> Option<Slot> {
        if self.i >= self.n {
            return None;
        }
        let s = Slot {
            name: obj::slot_name(self.o, self.i),
            kind: obj::slot_kind(self.o, self.i),
            value: obj::slot_value(self.o, self.i),
        };
        self.i += 1;
        Some(s)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.n - self.i;
        (n, Some(n))
    }
}

impl ExactSizeIterator for SlotsIter {}

impl IntoIterator for &SlotsRef {
    type Item = Slot;
    type IntoIter = SlotsIter;
    fn into_iter(self) -> SlotsIter {
        self.iter()
    }
}

// ----------------------------------------------------------------- payload

/// What an object's payload *is*, for a caller that wants to branch on it.
/// `Payload` builds one; this reads one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PayKind {
    None,
    Bytes,
    Vector,
    Method,
    Block,
    Mirror,
    Proxy,
    Float,
    Activation,
}

pub fn pay_kind(o: ObjRef) -> PayKind {
    match heap::kind(o) {
        Kind::Slots | Kind::Map => PayKind::None,
        Kind::Bytes => PayKind::Bytes,
        Kind::ObjVector => PayKind::Vector,
        Kind::Method => PayKind::Method,
        Kind::Block => PayKind::Block,
        Kind::Mirror => PayKind::Mirror,
        Kind::Proxy => PayKind::Proxy,
        Kind::Float => PayKind::Float,
        Kind::Activation => PayKind::Activation,
    }
}

/// An object's payload, as a `Copy` handle. The kind decides which accessor
/// means anything.
#[derive(Clone, Copy)]
pub struct PayRef(ObjRef);

impl PayRef {
    pub fn kind(&self) -> PayKind {
        pay_kind(self.0)
    }
    pub fn is_none(&self) -> bool {
        self.kind() == PayKind::None
    }
    pub fn bytes(&self) -> Option<Vec<u8>> {
        obj::bytes(self.0)
    }
    pub fn byte_len(&self) -> usize {
        obj::byte_len(self.0)
    }
    pub fn byte_at(&self, i: usize) -> u8 {
        heap::byte_at(self.0, i)
    }
    pub fn set_byte_at(&self, i: usize, b: u8) {
        heap::set_byte_at(self.0, i, b)
    }
    pub fn vector(&self) -> Option<Vec<Value>> {
        (self.kind() == PayKind::Vector).then(|| obj::elements(self.0))
    }
    pub fn vector_len(&self) -> usize {
        if self.kind() == PayKind::Vector {
            heap::ilen(self.0)
        } else {
            0
        }
    }
    pub fn element(&self, i: usize) -> Value {
        obj::from_oop(heap::element(self.0, i))
    }
    pub fn set_element(&self, i: usize, v: Value) {
        let w = obj::to_oop(v);
        heap::set_element(self.0, i, w);
        if w.is_obj() {
            heap::heap().record(self.0);
        }
    }
    pub fn method(&self) -> Option<Rc<Method>> {
        obj::method_of(self.0)
    }
    pub fn block_scope(&self) -> Option<ObjRef> {
        obj::block_scope(self.0)
    }
    pub fn set_block_scope(&self, s: Option<ObjRef>) {
        obj::set_block_scope(self.0, s)
    }
    pub fn mirror(&self) -> Option<Value> {
        (self.kind() == PayKind::Mirror).then(|| obj::mirror_of(self.0))
    }
    pub fn proxy(&self) -> Option<u64> {
        obj::proxy(self.0)
    }
    pub fn kill_proxy(&self) {
        obj::kill_proxy(self.0)
    }
    pub fn set_proxy(&self, p: Option<u64>) {
        obj::set_proxy(self.0, p)
    }
}

// ------------------------------------------------------------- the object

/// A borrowed object. `Copy`, because there is nothing to guard: the words are
/// in the arena and every read goes to them. Held only within a statement --
/// an object moves, and a view that outlived a safepoint would name a space
/// that has been abandoned.
#[derive(Clone, Copy)]
pub struct Obj {
    pub slots: SlotsRef,
    pub payload: PayRef,
    at: ObjRef,
}

impl Obj {
    /// By interned name. Every hot caller already has one; `find` is for the
    /// cold ones that hold a string and pay a hash to get here.
    pub fn find_sym(&self, name: Sym) -> Option<usize> {
        obj::find(self.at, name)
    }
    pub fn find(&self, name: &str) -> Option<usize> {
        self.find_sym(sym(name))
    }
    /// Write a slot's value. A parent write rewires the object graph and so
    /// invalidates memoised lookups and this object's shape; a data write
    /// cannot change what a lookup finds, which is the point of keeping data
    /// values out of the shape.
    pub fn assign(&self, i: usize, v: Value) {
        obj::assign(self.at, i, v)
    }
    pub fn map(&self) -> MapRef {
        map_of(self.at)
    }
    pub fn oop(&self) -> ObjRef {
        self.at
    }
}

impl ObjRef {
    pub fn borrow(self) -> Obj {
        Obj { slots: SlotsRef(self), payload: PayRef(self), at: self }
    }
    /// The same view, with the write barrier fired: an old object that may now
    /// hold a young reference has to be scanned by the next scavenge.
    /// Conservative -- it fires for reads dressed as writes -- which is the
    /// trade the C++ VM's unconditional card store makes.
    pub fn borrow_mut(self) -> Obj {
        record_if_old(self);
        self.borrow()
    }
    /// Identity, for a table that lives no longer than a collection. An object
    /// moves, so nothing may key on this across one.
    pub fn id(self) -> usize {
        self.addr()
    }
}

// -------------------------------------------------------------------- maps

/// A map: the shape a lookup depends on, interned so that every object of that
/// shape names the same one. The C++ VM keeps slot descriptors in a map and
/// keys its lookups on it -- `MethodLookupKey` "adds the receiver map to that
/// info, and is specific to a given receiver map" (lookup/key.hh:49).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct MapRef(u32);

/// Slot names and kinds in order, plus the address of every parent slot's
/// value. Addresses move, so a shape is only good for one `LOOKUP_GEN` -- and
/// a collection bumps that, which is what makes keying on them sound.
#[derive(PartialEq, Eq, Hash)]
struct Shape(Vec<(u32, u8, usize)>);

fn shape_of(o: ObjRef) -> Shape {
    Shape(
        (0..obj::slot_count(o))
            .map(|i| {
                let v = if obj::slot_kind(o, i) == SlotKind::Parent {
                    heap::slot_value(o, i).addr()
                } else {
                    0
                };
                (heap::slot_name(o, i), heap::slot_kind(o, i), v)
            })
            .collect(),
    )
}

thread_local! {
    /// Interned shapes, and the generation they were interned at. A collection
    /// moves the parents a shape is keyed on, so the whole table is dropped
    /// when the generation moves -- the same event that drops the lookup cache.
    static MAPS: RefCell<(u64, Vec<Rc<Shape>>, HashMap<Rc<Shape>, MapRef>)> =
        RefCell::new((0, Vec::new(), HashMap::new()));
}

fn intern_shape(o: ObjRef) -> MapRef {
    let gen = lookup_gen();
    let s = shape_of(o);
    MAPS.with(|t| {
        let mut t = t.borrow_mut();
        if t.0 != gen {
            t.1.clear();
            t.2.clear();
            t.0 = gen;
        }
        if let Some(&m) = t.2.get(&s) {
            return m;
        }
        let s = Rc::new(s);
        let m = MapRef(t.1.len() as u32);
        t.1.push(s.clone());
        t.2.insert(s, m);
        crate::metrics::map_minted();
        m
    })
}

/// The shape this object's lookups depend on, memoised in the object against
/// the generation it was computed at.
pub fn map_of(o: ObjRef) -> MapRef {
    let gen = lookup_gen() as u32;
    if let Some(m) = heap::shape_memo(o, gen) {
        if MAP_VERIFY.with(|v| *v) {
            verify_map(o, MapRef(m));
        }
        return MapRef(m);
    }
    let m = intern_shape(o);
    heap::set_shape_memo(o, gen, m.0);
    m
}

/// The shape changed, so the memoised map is wrong.
pub fn forget_map(o: ObjRef) {
    heap::set_shape_memo(o, u32::MAX, u32::MAX);
}

thread_local! {
    /// `SERF_MAP_VERIFY=1`: check every memoised map against a freshly computed
    /// shape, so a mutation that changed a shape without saying so fails where
    /// the stale map is used rather than by dispatching somewhere else.
    static MAP_VERIFY: bool = std::env::var_os("SERF_MAP_VERIFY").is_some();
}

fn verify_map(o: ObjRef, cached: MapRef) {
    let fresh = MAPS.with(|t| t.borrow().2.get(&shape_of(o)).copied());
    if fresh != Some(cached) {
        panic!(
            "stale map: object memoised {:?} but its shape interns to {:?} -- \
             a slot was changed without forget_map()",
            cached, fresh
        );
    }
}

// ------------------------------------------------------------------ method

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
    pub slot_inits: RefCell<Vec<Value>>,
    pub code: Vec<u8>,
    pub lits: RefCell<Vec<Value>>,
    /// selector literals pre-decoded, so sends don't rebuild a String each time
    pub lit_strs: Vec<Option<Rc<str>>>,
    pub is_block: bool,
    pub file: Rc<str>,
    pub line: u32,
    /// methodMap's `_source`, plus the slice of it that is this method
    /// (`_sourceOffset`/`_sourceLen`, non-zero only for a block, whose source
    /// string is its enclosing method's). Only a loaded method has one: serf's
    /// parser does not record source spans.
    pub source: Cell<Option<(Value, i64, i64)>>,
    /// Inline caches, one per literal; sized on the method's first send. Not
    /// a GC root: an entry is only read at the generation it was filled at,
    /// and a collection bumps that.
    pub sites: RefCell<Vec<Site>>,
    /// how much operand stack an activation of this method needs, worked out
    /// on its first send. `u32::MAX` until then.
    pub max_stack: Cell<u32>,
}

/// A send site's inline cache.
#[derive(Clone, Copy)]
pub struct Site {
    /// arity of the selector in this literal, a pure function of it -- so
    /// unlike `hit` it never goes stale. `u32::MAX` until first computed.
    nargs: u32,
    /// The `LOOKUP_GEN` that was true at, the last receiver seen here, its map,
    /// and what lookup found. Two ways in: the same receiver again hits without
    /// touching the object at all, and a *different* receiver of the same shape
    /// hits after one deref to read its map.
    hit: Option<(u64, ObjRef, MapRef, MapHit)>,
}

impl Default for Site {
    fn default() -> Site {
        Site { nargs: u32::MAX, hit: None }
    }
}

impl Method {
    fn sites(&self) -> std::cell::RefMut<'_, Vec<Site>> {
        let mut s = self.sites.borrow_mut();
        let n = self.lits.borrow().len();
        if s.len() < n {
            s.resize(n, Site::default());
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

    /// The same receiver as last time? Then the answer stands, and reading the
    /// object to find its map was not necessary.
    pub fn site_hit_recv(&self, i: usize, r: ObjRef) -> Option<MapHit> {
        match self.sites()[i].hit {
            Some((g, s, _, h)) if s == r && g == lookup_gen() => {
                crate::metrics::site(true, false);
                Some(h)
            }
            _ => None,
        }
    }

    /// A different receiver, but of the same shape. This is the probe receiver
    /// keying could not make.
    pub fn site_hit_map(&self, i: usize, k: MapRef) -> Option<MapHit> {
        match self.sites()[i].hit {
            Some((g, _, m, h)) if m == k && g == lookup_gen() => {
                crate::metrics::site(true, true);
                Some(h)
            }
            _ => {
                crate::metrics::site(false, false);
                None
            }
        }
    }

    pub fn site_fill(&self, i: usize, r: ObjRef, k: MapRef, h: MapHit) {
        self.sites()[i].hit = Some((lookup_gen(), r, k, h));
    }
}

// -------------------------------------------------------------- activations

/// An upper bound on how deep this method's operand stack can get, computed
/// once and remembered.
///
/// Every bytecode pushes at most one thing, so counting the ones that push at
/// all bounds the depth. ponytail: an over-estimate -- a proper stack-effect
/// walk would be tighter, and would matter if a long method were ever deeply
/// recursive. `debug_assert` in `act_push` is what would say so.
pub fn max_stack(m: &Method) -> usize {
    let n = m.max_stack.get();
    if n != u32::MAX {
        return n as usize;
    }
    let d = m
        .code
        .iter()
        .filter(|b| {
            matches!(
                *b >> 4,
                crate::compile::LITERAL
                    | crate::compile::READ_LOCAL
                    | crate::compile::SEND
                    | crate::compile::IMPLICIT_SEND
                    | crate::compile::NO_OPERAND
            )
        })
        .count()
        + 1;
    m.max_stack.set(d as u32);
    d
}

/// An activation is a heap object now, so there is no `Rc` to hand back and no
/// pool to hand it to: a frame that returns simply stops naming it, and the
/// next scavenge forgets it. This is where 1.79M of the 1.86M mallocs a test
/// run used to make have gone.
pub fn new_activation(
    m: Rc<Method>,
    recv: Value,
    holder: Value,
    args: &[Value],
    lexical: Option<ObjRef>,
) -> ObjRef {
    // no clones: a send happens millions of times, and cloning the method's
    // initialiser list and argument map on each one is two `malloc`s that the
    // whole exercise is about not making
    let n = m.slot_inits.borrow().len();
    let home = lexical.map(home_of);
    let a = obj::new_activation(m.clone(), n, max_stack(&m));
    obj::act_set(a, act::RECV, recv);
    obj::act_set(a, act::HOLDER, holder);
    obj::act_set_link(a, act::LEXICAL, lexical);
    obj::act_set_link(a, act::HOME, home);
    obj::act_set_dead(a, false);
    for (i, v) in m.slot_inits.borrow().iter().enumerate() {
        obj::act_set_local(a, i, *v);
    }
    for (i, v) in args.iter().enumerate() {
        obj::act_set_local(a, m.arg_slots[i], *v);
    }
    a
}

pub fn home_of(a: ObjRef) -> ObjRef {
    obj::act_link(a, act::HOME).unwrap_or(a)
}

pub fn act_method(a: ObjRef) -> Rc<Method> {
    obj::act_method(a)
}

pub fn act_recv(a: ObjRef) -> Value {
    obj::act_get(a, act::RECV)
}

pub fn act_holder(a: ObjRef) -> Value {
    obj::act_get(a, act::HOLDER)
}

pub fn act_lexical(a: ObjRef) -> Option<ObjRef> {
    obj::act_link(a, act::LEXICAL)
}

pub fn act_dead(a: ObjRef) -> bool {
    obj::act_dead(a)
}

pub fn act_set_dead(a: ObjRef, d: bool) {
    obj::act_set_dead(a, d)
}

pub fn act_locals(a: ObjRef) -> usize {
    obj::act_locals(a)
}

pub fn act_local(a: ObjRef, i: usize) -> Value {
    obj::act_local(a, i)
}

pub fn act_set_local(a: ObjRef, i: usize, v: Value) {
    obj::act_set_local(a, i, v)
}

// ------------------------------------------------------------------- values

impl Value {
    pub fn obj(slots: impl AsRef<[Slot]>, payload: Payload) -> Value {
        Value::Obj(obj::make(slots.as_ref(), payload, false))
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
        self.as_obj().and_then(obj::method_of)
    }
    /// A block object, whose method is its `value...` slot's rather than its
    /// own: what a send finds in a slot is data, not something to run.
    pub fn is_block(&self) -> bool {
        self.as_obj().is_some_and(|o| pay_kind(o) == PayKind::Block)
    }
    pub fn bytes(&self) -> Option<Vec<u8>> {
        self.as_obj().and_then(obj::bytes)
    }
    pub fn as_str(&self) -> Option<String> {
        self.bytes().map(|b| String::from_utf8_lossy(&b).into_owned())
    }
}

// ------------------------------------------------------------- annotations

/// Self keeps an object's annotations in its map. serf keeps them in the
/// object, which is what lets the collector see them -- a Rust-side table
/// keyed on an address cannot survive the address moving, and that is what
/// `Vm::anno_obj` and `Vm::anno_slot` were.
pub fn obj_anno(o: ObjRef) -> Option<Value> {
    let a = heap::obj_anno(o);
    (!a.is_null()).then(|| obj::from_oop(a))
}

pub fn slot_anno(o: ObjRef, i: usize) -> Option<Value> {
    let a = heap::slot_anno(o, i);
    (!a.is_null()).then(|| obj::from_oop(a))
}

impl Vm {
    /// Give an object an annotation. An object that has never had one has no
    /// room for it, so this reshapes it -- which means every pointer to it
    /// moves, exactly as adding a slot does.
    pub fn set_obj_anno(&mut self, o: Value, a: Value) {
        let Some(at) = o.as_obj() else { return };
        let wide = obj::annotate(at);
        if wide != at {
            self.switch(at, wide);
        }
        heap::set_obj_anno(wide, obj::to_oop(a));
        record_if_old(wide);
    }

    /// Annotating an object that has no room for annotations widens it, and a
    /// wider object is a different one -- so this answers where the object
    /// went. A `Value` in a Rust local is not a root and `switch_pointers` does
    /// not reach it; the caller has to take the answer.
    #[must_use]
    pub fn set_slot_anno(&mut self, o: Value, i: usize, a: Value) -> Value {
        let Some(at) = o.as_obj() else { return o };
        let wide = obj::annotate(at);
        if wide != at {
            self.switch(at, wide);
        }
        heap::set_slot_anno(wide, i, obj::to_oop(a));
        record_if_old(wide);
        Value::Obj(wide)
    }
}

// ---------------------------------------------------------------- lookup

#[derive(Clone, Copy)]
pub enum LookupErr {
    NotFound,
    Ambiguous,
}

/// Where a selector was found: a slot index in some holder object.
#[derive(Clone, Copy)]
pub struct Hit {
    pub holder: ObjRef,
    pub idx: usize,
}

/// The same answer, relative to the object the search started from: a slot in
/// that object itself, or a slot in one particular ancestor. The distinction is
/// what makes a hit cacheable across a whole clone family.
#[derive(Clone, Copy)]
pub enum MapHit {
    OnSelf(usize),
    In(ObjRef, usize),
}

impl MapHit {
    pub fn of(start: ObjRef, h: Hit) -> MapHit {
        if h.holder == start {
            MapHit::OnSelf(h.idx)
        } else {
            MapHit::In(h.holder, h.idx)
        }
    }
    pub fn at(self, start: ObjRef) -> Hit {
        match self {
            MapHit::OnSelf(idx) => Hit { holder: start, idx },
            MapHit::In(holder, idx) => Hit { holder, idx },
        }
    }
}

/// Memoised `lookup` results, selector -> receiver shape -> outcome. Entries
/// are valid only for the `LOOKUP_GEN` they were filled at; the whole cache is
/// dropped when the generation moves, which also covers a collection having
/// moved every address a shape is keyed on.
#[derive(Default)]
pub struct LookupCache {
    gen: u64,
    len: usize,
    map: HashMap<Rc<str>, HashMap<MapRef, Result<MapHit, LookupErr>>>,
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
    /// dynamically loaded C libraries, for the image's glue primitives
    pub ffi: crate::ffi::Ffi,
    /// parked Self process stacks. A list rather than a map: the key is an
    /// object, and an object's address is not a key across a collection.
    pub procs: Vec<(Value, Vec<crate::interp::Frame>)>,
    /// frame stacks lent to the Vm while the interpreter is somewhere it could
    /// collect. A nested run leaves the outer stacks as Rust locals, where the
    /// collector cannot see them, so `run_stack` lends its own across anything
    /// that might re-enter.
    pub stacks: Vec<Vec<crate::interp::Frame>>,
    /// values held in Rust locals across a call that can re-enter the
    /// interpreter, and so across a collection
    pub temp_roots: Vec<Value>,
    /// the process the scheduler is currently running, for _ThisProcess
    pub current_proc: Option<Value>,
    /// stand-in process for code run outside the scheduler
    pub vm_proc: Option<Value>,
    pub signals_blocked: bool,
    /// true while the world's own scheduler loop is on the stack
    pub scheduler_running: bool,
    /// the world's programming timestamp, bumped as it changes itself
    pub timestamp: i64,
    /// C structs handed to foreign code; kept alive for the VM's lifetime
    pub c_heap: Vec<Vec<u8>>,
    pub flags: HashMap<String, Value>,
    /// Self canonicalises strings: `traits canonicalString =` tests identity
    /// first, so a string serf hands the world must be the world's own object
    /// or it compares unequal to every literal.
    pub canonical: HashMap<Vec<u8>, Value>,
    /// see `lookup`; not a GC root -- entries never outlive their generation,
    /// and a collection bumps it
    pub lookup_cache: RefCell<LookupCache>,
}

impl Vm {
    pub fn new() -> Vm {
        let mk = || Value::obj([], Payload::None);
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
            [
                slot("object", SlotKind::Data, t_object),
                slot("smallInt", SlotKind::Data, t_smallint),
                slot("float", SlotKind::Data, t_float),
                slot("string", SlotKind::Data, t_string),
                slot("byteVector", SlotKind::Data, t_bytevector),
                slot("vector", SlotKind::Data, t_vector),
                slot("block", SlotKind::Data, t_block),
            ],
            Payload::None,
        );
        let globals = Value::obj(
            [
                slot("traits", SlotKind::Data, traits),
                slot("nil", SlotKind::Data, nil),
                slot("true", SlotKind::Data, tru),
                slot("false", SlotKind::Data, fals),
            ],
            Payload::None,
        );
        let lobby = Value::obj([slot("globals", SlotKind::Parent, globals)], Payload::None);
        // prototypes: Self code cannot conjure an indexable out of nothing
        let string_proto =
            Value::obj([slot("parent", SlotKind::Parent, t_string)], Payload::Bytes(vec![]));
        let bv_proto =
            Value::obj([slot("parent", SlotKind::Parent, t_bytevector)], Payload::Bytes(vec![]));
        let vec_proto =
            Value::obj([slot("parent", SlotKind::Parent, t_vector)], Payload::Vector(vec![]));
        let mut vm = Vm {
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
            ffi: crate::ffi::Ffi::default(),
            procs: vec![],
            stacks: vec![],
            temp_roots: vec![],
            current_proc: None,
            vm_proc: None,
            signals_blocked: false,
            scheduler_running: false,
            timestamp: 0,
            c_heap: vec![],
            flags: HashMap::new(),
            canonical: HashMap::new(),
            lookup_cache: RefCell::new(LookupCache::default()),
        };
        for (n, v) in [
            ("lobby", lobby),
            ("globals", globals),
            ("string", string_proto),
            ("byteVector", bv_proto),
            ("vector", vec_proto),
        ] {
            let g = vm.globals;
            vm.put_slot(g, slot(n, SlotKind::Data, v));
            // globals just widened, and `vm.globals` is a root, so the switch
            // has already moved it; nothing to re-fetch
        }
        vm
    }

    /// Add or replace a slot. An object is a fixed run of words, so adding one
    /// builds a wider object and switches every pointer to it --
    /// `universe::switch_pointers` (memory/universe.cpp:315), which is the bill
    /// direct pointers come with and is affordable because this happens while a
    /// world is being programmed, not while it runs.
    /// Answers where the object ended up. It has to: widening builds a new
    /// object and switches every pointer in the heap and every root to it, but
    /// a `Value` in a Rust local is neither, so the caller's is stale the
    /// moment this returns.
    pub fn put_slot(&mut self, o: Value, s: Slot) -> Value {
        lookup_gen_bump();
        let Some(at) = o.as_obj() else { return o };
        if let Some(i) = obj::find(at, s.name) {
            forget_map(at);
            heap::set_slot_desc(at, i, s.name.id(), s.kind as u8);
            obj::assign(at, i, s.value);
            return o;
        }
        let wide = obj::grow(at, &[s]);
        self.switch(at, wide);
        Value::Obj(wide)
    }

    /// Drop the slots a name owns -- the data slot and its assignment slot.
    /// Answers where the object went, or `None` if there was no such slot.
    /// It has to answer, for the same reason `put_slot` does: the caller's
    /// `Value` is a Rust local, and the switch does not reach those.
    pub fn remove_slot(&mut self, o: Value, name: &str) -> Option<Value> {
        lookup_gen_bump();
        let at = o.as_obj()?;
        let colon = format!("{}:", name);
        let keep: Vec<Slot> = at
            .borrow()
            .slots
            .iter()
            .filter(|s| sym_str(s.name) != name && sym_str(s.name) != colon)
            .collect();
        if keep.len() == obj::slot_count(at) {
            return None;
        }
        let narrow = obj::reshape(at, &keep);
        self.switch(at, narrow);
        Some(Value::Obj(narrow))
    }

    pub(crate) fn switch(&mut self, from: ObjRef, to: ObjRef) {
        let mut r = VmRoots { vm: self };
        heap::heap().switch_pointers(&mut r, from, to);
    }

    pub fn is_false(&self, v: &Value) -> bool {
        v.id_eq(&self.fals) || self.image_false.as_ref().is_some_and(|f| v.id_eq(f))
    }

    /// nil, the image's if one is loaded.
    pub fn nil_v(&self) -> Value {
        self.image_roots.as_ref().map_or(self.nil, |r| r[1])
    }

    /// Primitives must answer with the *image's* booleans when one is loaded,
    /// or the world's `ifTrue:` finds no methods on them.
    pub fn boolean(&self, b: bool) -> Value {
        if b {
            self.image_true.unwrap_or(self.tru)
        } else {
            self.image_false.unwrap_or(self.fals)
        }
    }

    pub fn string(&self, s: &str) -> Value {
        if let Some(v) = self.canonical.get(s.as_bytes()) {
            return *v;
        }
        self.bytes_with(self.t_string, s.as_bytes().to_vec())
    }

    pub fn bytes_with(&self, parent: Value, b: Vec<u8>) -> Value {
        Value::obj(
            [Slot { name: SYM_PARENT, kind: SlotKind::Parent, value: parent }],
            Payload::Bytes(b),
        )
    }

    pub fn vector(&self, v: Vec<Value>) -> Value {
        Value::obj(
            [Slot { name: SYM_PARENT, kind: SlotKind::Parent, value: self.t_vector }],
            Payload::Vector(v),
        )
    }

    /// A parked process stack, by the process object. A list, not a map: the
    /// key is an object and an object's address is not a key across a
    /// collection, so this compares identity instead of hashing it. There are
    /// a handful of processes, so the scan is shorter than the hash would be.
    pub fn take_proc(&mut self, p: &Value) -> Option<Vec<crate::interp::Frame>> {
        let i = self.procs.iter().position(|(k, _)| k.id_eq(p))?;
        Some(self.procs.remove(i).1)
    }

    pub fn put_proc(&mut self, p: Value, fs: Vec<crate::interp::Frame>) {
        match self.procs.iter().position(|(k, _)| k.id_eq(&p)) {
            Some(i) => self.procs[i].1 = fs,
            None => self.procs.push((p, fs)),
        }
    }

    /// The receiver object whose slots are searched first. Immediates have
    /// none of their own, so lookup starts in their traits.
    fn implicit_parent(&self, v: &Value) -> Option<Value> {
        match v {
            Value::Int(_) => Some(self.t_smallint),
            Value::Float(_) => Some(self.t_float),
            Value::Obj(_) => None,
        }
    }

    /// The object a search actually starts from.
    pub fn lookup_key(&self, recv: &Value) -> ObjRef {
        match recv.as_obj() {
            Some(o) => o,
            None => self.implicit_parent(recv).unwrap().as_obj().unwrap(),
        }
    }

    /// The object a search starts from, and the map a memoised result is keyed
    /// on. Every object of that shape shares the entry.
    pub fn map_key(&self, recv: &Value) -> (ObjRef, MapRef) {
        let start = self.lookup_key(recv);
        (start, map_of(start))
    }

    pub fn lookup(&self, recv: &Value, sel: &str) -> Result<Hit, LookupErr> {
        let (start, key) = self.map_key(recv);
        self.lookup_from(recv, sel, start, key)
    }

    /// `lookup` for a caller that already has the key -- the send bytecode,
    /// which needs it for its inline cache anyway.
    pub fn lookup_from(
        &self,
        recv: &Value,
        sel: &str,
        start: ObjRef,
        key: MapRef,
    ) -> Result<Hit, LookupErr> {
        let gen = lookup_gen();
        {
            let mut c = self.lookup_cache.borrow_mut();
            if c.gen != gen {
                c.map.clear();
                c.len = 0;
                c.gen = gen;
            } else if let Some(r) = c.map.get(sel).and_then(|m| m.get(&key)) {
                return r.map(|h| h.at(start));
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
        let stored = r.map(|h| MapHit::of(start, h));
        let r = stored.map(|h| h.at(start));
        let mut c = self.lookup_cache.borrow_mut();
        // ponytail: crude size cap; an LRU if a world ever legitimately holds
        // this many live (shape, selector) pairs
        if c.len >= 1 << 20 {
            c.map.clear();
            c.len = 0;
        }
        match c.map.get_mut(sel) {
            Some(m) => {
                m.insert(key, stored);
            }
            None => {
                c.map.entry(sel.into()).or_default().insert(key, stored);
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
        if let Some(idx) = obj::find(o, sym(sel)) {
            if !hits.iter().any(|h| h.holder == o && h.idx == idx) {
                hits.push(Hit { holder: o, idx });
            }
            return; // a local slot shadows every parent
        }
        self.search_parents(o, sel, hits, seen);
    }

    fn search_parents(&self, o: ObjRef, sel: &str, hits: &mut Vec<Hit>, seen: &mut Vec<ObjRef>) {
        for i in 0..obj::slot_count(o) {
            if obj::slot_kind(o, i) != SlotKind::Parent {
                continue;
            }
            let p = obj::slot_value(o, i);
            self.search(&p, sel, hits, seen);
        }
    }
}

// ------------------------------------------------------------------- roots

/// Everything the collector must treat as live, as slots it may rewrite.
///
/// With a handle table nothing had to be rewritten and a stale reference was
/// merely stale. With direct pointers a root the collector does not know about
/// is a pointer into a space that has been abandoned, so this walks every
/// field of `Vm` that can hold a `Value`, however deeply.
pub struct VmRoots<'a> {
    pub vm: &'a mut Vm,
}

/// Hand one root to the collector, if it is one.
///
/// It must not go through `to_oop`: that *boxes a float*, and boxing allocates
/// -- during a collection, into the space being abandoned, moving the bump
/// pointer the scan is walking towards. A `Value::Float` in a Rust local was
/// never a heap object anyway; only storing one into the heap makes it one.
pub fn rewrite(v: &mut Value, f: &mut dyn FnMut(&mut Oop)) {
    if let Value::Obj(o) = v {
        f(o);
    }
}

impl heap::Roots for VmRoots<'_> {
    fn each(&mut self, f: &mut dyn FnMut(&mut Oop)) {
        let vm = &mut *self.vm;
        for x in [
            &mut vm.lobby,
            &mut vm.globals,
            &mut vm.nil,
            &mut vm.tru,
            &mut vm.fals,
            &mut vm.t_smallint,
            &mut vm.t_float,
            &mut vm.t_string,
            &mut vm.t_bytevector,
            &mut vm.t_vector,
            &mut vm.t_block,
        ] {
            rewrite(x, f);
        }
        for x in [&mut vm.image_true, &mut vm.image_false, &mut vm.current_proc, &mut vm.vm_proc] {
            if let Some(x) = x {
                rewrite(x, f);
            }
        }
        for x in vm.image_roots.iter_mut().chain(vm.image_strings.iter_mut()).flatten() {
            rewrite(x, f);
        }
        // canonical strings are strong, as the C++ VM's string table is at a
        // scavenge (stringTable.hh:87)
        for x in vm.canonical.values_mut().chain(vm.flags.values_mut()) {
            rewrite(x, f);
        }
        for x in vm.temp_roots.iter_mut() {
            rewrite(x, f);
        }
        for (p, fs) in vm.procs.iter_mut() {
            rewrite(p, f);
            crate::interp::frame_roots(fs, f);
        }
        for fs in vm.stacks.iter_mut() {
            crate::interp::frame_roots(fs, f);
        }
        // A method's literals -- strings, block prototypes, slot initialisers
        // -- are reachable only through the method table, which is Rust-side.
        // `gc.rs` used to walk them through `Payload::Method`; nothing does now
        // unless this does, and a literal the collector never sees is a
        // pointer into a space it has just abandoned.
    }

    fn weak(&mut self, f: &mut dyn FnMut(Oop) -> Option<Oop>) {
        let vm = &mut *self.vm;
        for (_, fs) in vm.procs.iter_mut() {
            crate::interp::frame_weak(fs, f);
        }
        for fs in vm.stacks.iter_mut() {
            crate::interp::frame_weak(fs, f);
        }
    }

    fn dying(&mut self, o: Oop) {
        obj::on_dying(o);
    }

    /// A method object's literals, initialisers and source are still in a Rust
    /// `Method` behind an index, so the collector cannot see them in the
    /// object's words. Reaching them through the object rather than by walking
    /// the whole method table is the difference between O(live methods reached)
    /// and O(every method ever compiled) on every single scavenge.
    fn extra(&mut self, o: Oop, f: &mut dyn FnMut(&mut Oop)) {
        obj::each_method_value_of(o, f);
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
            match b.payload.kind() {
                PayKind::Bytes => {
                    format!("'{}'", String::from_utf8_lossy(&b.payload.bytes().unwrap()))
                }
                PayKind::Vector => {
                    let parts: Vec<String> = b
                        .payload
                        .vector()
                        .unwrap()
                        .iter()
                        .map(|i| default_print_string(vm, i))
                        .collect();
                    format!("({}. )", parts.join(". "))
                }
                PayKind::Method => format!("<method {}>", b.payload.method().unwrap().sel),
                PayKind::Block => format!("<block {}>", b.payload.method().unwrap().sel),
                PayKind::Mirror => "<mirror>".to_string(),
                PayKind::Float => fmt_float(f64::from_bits(heap::aux_word(*o, 0))),
                PayKind::Activation => "<activation>".to_string(),
                PayKind::Proxy => match b.payload.proxy() {
                    Some(p) => format!("<proxy {:#x}>", p),
                    None => "<dead proxy>".into(),
                },
                PayKind::None => {
                    let names: Vec<String> = b
                        .slots
                        .iter()
                        .map(|s| {
                            format!(
                                "{}{}",
                                s.name,
                                if s.kind == SlotKind::Parent { "*" } else { "" }
                            )
                        })
                        .collect();
                    format!("(| {} |)", names.join(". "))
                }
            }
        }
    }
}

/// A method with nothing in it, for tests that need one to hang an object or
/// an activation off.
#[cfg(test)]
pub fn test_method() -> Rc<Method> {
    Rc::new(Method {
        sel: "t".into(),
        nargs: 0,
        arg_slots: vec![],
        slot_names: vec![],
        slot_flags: vec![],
        slot_inits: RefCell::new(vec![]),
        code: vec![],
        lits: RefCell::new(vec![]),
        lit_strs: vec![],
        is_block: false,
        file: "t".into(),
        line: 0,
        source: Cell::new(None),
        sites: Default::default(),
        max_stack: std::cell::Cell::new(u32::MAX),
    })
}
