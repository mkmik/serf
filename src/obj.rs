//! Self objects as arena objects: the layout `value.rs` will be rewritten onto.
//!
//! `heap.rs` knows about words, slots and collection but nothing about Self.
//! This is the other half of that boundary -- what a Self object *is* in those
//! terms, and the accessors the interpreter and the primitives will use once
//! `gc.rs` and its cells are gone.
//!
//! It is built and tested before the flip so that the flip is a rename rather
//! than a design. Nothing calls it yet.

#![allow(dead_code)]

use std::cell::RefCell;
use std::rc::Rc;

use crate::heap::{self, Kind, Oop, Shape};
use crate::value::{Method, Sym};

/// What `Value` becomes at the flip: the same three cases, with an arena
/// pointer where the handle used to be. Kept separate until then so this
/// module can be built and tested while `gc.rs` still owns `Value`.
#[derive(Clone, Copy, Debug)]
pub enum Val {
    Int(i64),
    Float(f64),
    Obj(Oop),
}

impl Val {
    pub fn id_eq(&self, o: &Val) -> bool {
        match (self, o) {
            (Val::Int(a), Val::Int(b)) => a == b,
            (Val::Float(a), Val::Float(b)) => a == b,
            (Val::Obj(a), Val::Obj(b)) => a == b,
            _ => false,
        }
    }
    pub fn as_obj(&self) -> Option<Oop> {
        match self {
            Val::Obj(o) => Some(*o),
            _ => None,
        }
    }
}

// ------------------------------------------------------------------ the word

/// `Value` in the heap is one word. In Rust it stays an enum, because 180 call
/// sites match on it and an eight-byte word cannot hold an `f64` beside a
/// pointer. The conversion is the boundary, and it is two instructions in the
/// common cases.
///
/// A float boxes on the way in: Self boxes them too (`floatMap`), `core.snap`
/// has 155 of them in its entire reachable graph, and one that dies before the
/// next scavenge costs a bump.
pub fn to_oop(v: Val) -> Oop {
    match v {
        Val::Int(i) => Oop::int(i),
        Val::Obj(o) => o,
        Val::Float(f) => {
            let o = heap::heap().alloc_or_tenure(Shape::new(Kind::Float, 0).with_raw(1));
            heap::set_aux_word(o, 0, f.to_bits());
            o
        }
    }
}

pub fn from_oop(o: Oop) -> Val {
    if let Some(i) = o.as_int() {
        return Val::Int(i);
    }
    if o.is_null() {
        return Val::Int(0);
    }
    if heap::kind(o) == Kind::Float {
        return Val::Float(f64::from_bits(heap::aux_word(o, 0)));
    }
    Val::Obj(o)
}

// --------------------------------------------------------------- what a slot is

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum SlotKind {
    Data,
    Parent,
    /// `x:` -- writes the data slot named by the slot's name minus its colon.
    Assign,
}

impl SlotKind {
    fn to_byte(self) -> u8 {
        self as u8
    }
    fn from_byte(b: u8) -> SlotKind {
        match b {
            1 => SlotKind::Parent,
            2 => SlotKind::Assign,
            _ => SlotKind::Data,
        }
    }
}

// ------------------------------------------------------------- what an object is

/// Every Self object is one of these, and the kind decides what its traced
/// fields past the named slots mean.
///
/// ```text
/// Slots      nothing more
/// Bytes      a string or byte vector; the bytes are untraced
/// ObjVector  its elements
/// Method     [0] method index (an immediate)
/// Block      [0] method index, [1] the activation it closed over, or null
/// Mirror     [0] the reflectee
/// Proxy      one untraced word: a foreign pointer
/// Float      one untraced word: the bits
/// Activation see `act`
/// ```
pub const F_METHOD: usize = 0;
pub const F_BLOCK_SCOPE: usize = 1;
pub const F_MIRROR: usize = 0;

/// How a Self object is built. Kept as an enum so that `Value::obj(slots,
/// Payload::Bytes(v))` reads as it always has -- the 66 construction sites do
/// not care that the bytes now go into arena words.
pub enum Payload {
    None,
    Bytes(Vec<u8>),
    Vector(Vec<Val>),
    Method(Rc<Method>),
    /// block prototype (`None`) or a block closed over an activation
    Block(Rc<Method>, Option<Oop>),
    Mirror(Val),
    /// a foreign pointer held by a proxy. `None` is a *dead* proxy: Self kills
    /// one by stamping its type seal, not by nulling its pointer, so a live
    /// proxy may legitimately point at 0 -- `stdin` is fd 0.
    Proxy(Option<u64>),
}

impl Payload {
    fn shape(&self, slots: usize) -> Shape {
        match self {
            Payload::None => Shape::new(Kind::Slots, slots),
            Payload::Bytes(b) => Shape::indexable(Kind::Bytes, slots, b.len()),
            Payload::Vector(v) => Shape::indexable(Kind::ObjVector, slots, v.len()),
            Payload::Method(_) => Shape::indexable(Kind::Method, slots, 1),
            Payload::Block(..) => Shape::indexable(Kind::Block, slots, 2),
            Payload::Mirror(_) => Shape::indexable(Kind::Mirror, slots, 1),
            Payload::Proxy(_) => Shape::new(Kind::Proxy, slots).with_raw(2),
        }
    }

    fn fill(self, o: Oop) {
        match self {
            Payload::None => {}
            Payload::Bytes(b) => heap::set_bytes(o, &b),
            Payload::Vector(v) => {
                for (i, x) in v.into_iter().enumerate() {
                    heap::set_element(o, i, to_oop(x));
                }
            }
            Payload::Method(m) => set_field(o, F_METHOD, Oop::int(intern_method(m) as i64)),
            Payload::Block(m, s) => {
                set_field(o, F_METHOD, Oop::int(intern_method(m) as i64));
                set_field(o, F_BLOCK_SCOPE, s.unwrap_or_else(Oop::null));
            }
            Payload::Mirror(v) => set_field(o, F_MIRROR, to_oop(v)),
            // a dead proxy is a *seal*, not a null pointer, so the two need
            // telling apart: word 1 says whether word 0 means anything
            Payload::Proxy(p) => {
                heap::set_aux_word(o, 0, p.unwrap_or(0));
                heap::set_aux_word(o, 1, p.is_some() as u64);
            }
        }
    }
}

/// A field past the named slots, by index within the kind's own fields.
fn field(o: Oop, i: usize) -> Oop {
    heap::field(o, heap::slots(o) + anno_span(o) + i)
}

fn set_field(o: Oop, i: usize, v: Oop) {
    heap::set_field(o, heap::slots(o) + anno_span(o) + i, v)
}

fn anno_span(o: Oop) -> usize {
    if heap::is_annotated(o) {
        1 + heap::slots(o)
    } else {
        0
    }
}

/// Build one. The shape is decided by the payload, the slots are written, and
/// the payload fills what is left.
pub fn make(slots: &[(Sym, SlotKind, Val)], payload: Payload, anno: bool) -> Oop {
    let mut shape = payload.shape(slots.len());
    if anno {
        shape = shape.annotated();
    }
    let o = heap::heap().alloc_or_tenure(shape);
    for (i, (name, kind, v)) in slots.iter().enumerate() {
        heap::set_slot_desc(o, i, name.id(), kind.to_byte());
        heap::set_slot_value(o, i, to_oop(*v));
    }
    payload.fill(o);
    o
}

// ------------------------------------------------------------------ accessors

pub fn slot_count(o: Oop) -> usize {
    heap::slots(o)
}

pub fn slot_name(o: Oop, i: usize) -> Sym {
    Sym::from_id(heap::slot_name(o, i))
}

pub fn slot_kind(o: Oop, i: usize) -> SlotKind {
    SlotKind::from_byte(heap::slot_kind(o, i))
}

pub fn slot_value(o: Oop, i: usize) -> Val {
    from_oop(heap::slot_value(o, i))
}

pub fn find(o: Oop, name: Sym) -> Option<usize> {
    heap::find_slot(o, name.id())
}

/// Write a slot's value, through the barrier. A parent write rewires the object
/// graph, so it invalidates the memoised lookups and this object's shape; a
/// data write cannot change what a lookup finds, which is the point of keeping
/// data values out of the shape.
pub fn assign(o: Oop, i: usize, v: Val) {
    if slot_kind(o, i) == SlotKind::Parent {
        crate::value::lookup_gen_bump();
    }
    let w = to_oop(v);
    heap::set_slot_value(o, i, w);
    if w.is_obj() {
        heap::heap().record(o);
    }
}

pub fn bytes(o: Oop) -> Option<Vec<u8>> {
    (heap::kind(o) == Kind::Bytes).then(|| heap::bytes_of(o))
}

pub fn byte_len(o: Oop) -> usize {
    if heap::kind(o) == Kind::Bytes {
        heap::ilen(o)
    } else {
        0
    }
}

pub fn elements(o: Oop) -> Vec<Val> {
    (0..heap::ilen(o)).map(|i| from_oop(heap::element(o, i))).collect()
}

pub fn method_of(o: Oop) -> Option<Rc<Method>> {
    match heap::kind(o) {
        Kind::Method | Kind::Block => {
            Some(method_at(field(o, F_METHOD).as_int().unwrap_or(0) as u32))
        }
        _ => None,
    }
}

pub fn block_scope(o: Oop) -> Option<Oop> {
    if heap::kind(o) != Kind::Block {
        return None;
    }
    let s = field(o, F_BLOCK_SCOPE);
    s.is_obj().then_some(s)
}

pub fn proxy(o: Oop) -> Option<u64> {
    (heap::aux_word(o, 1) != 0).then(|| heap::aux_word(o, 0))
}

// ---------------------------------------------------------------- the methods

thread_local! {
    /// Methods, reached from an object by index rather than lived in.
    ///
    /// A method is compiled once and never churns -- 15,774 in `core.snap`,
    /// and a handful more per REPL line -- so a Rust-side table is a fair
    /// bridge until `Method` itself moves into the heap. An activation is not,
    /// which is why activations move outright: `test.self` makes 226,222 of
    /// them and a table that never releases one is a leak rather than a bridge.
    ///
    /// The collector's `dying` hook releases an entry when its method object is
    /// forgotten, which is what keeps `interp::send`'s one-shot methods from
    /// piling up.
    static METHODS: RefCell<(Vec<Option<Rc<Method>>>, Vec<u32>)> =
        const { RefCell::new((Vec::new(), Vec::new())) };
}

pub fn intern_method(m: Rc<Method>) -> u32 {
    METHODS.with(|t| {
        let mut t = t.borrow_mut();
        if let Some(i) = t.1.pop() {
            t.0[i as usize] = Some(m);
            return i;
        }
        t.0.push(Some(m));
        t.0.len() as u32 - 1
    })
}

pub fn method_at(i: u32) -> Rc<Method> {
    METHODS.with(|t| t.borrow().0[i as usize].clone().expect("method already released"))
}

pub fn release_method(i: u32) {
    METHODS.with(|t| {
        let mut t = t.borrow_mut();
        if t.0[i as usize].take().is_some() {
            t.1.push(i);
        }
    });
}

/// What the collector should do with an object it is about to forget.
pub fn on_dying(o: Oop) {
    if matches!(heap::kind(o), Kind::Method | Kind::Block) {
        if let Some(i) = field(o, F_METHOD).as_int() {
            // a block shares its prototype's method, so only the method object
            // itself releases; a block's index is a copy
            if heap::kind(o) == Kind::Method {
                release_method(i as u32);
            }
        }
    }
}

pub fn live_methods() -> usize {
    METHODS.with(|t| t.borrow().0.iter().filter(|m| m.is_some()).count())
}

// ------------------------------------------------------------- an activation

/// An activation's fields. All traced, because an immediate is a legal `Oop`
/// and the collector steps over it -- so the program counter and the slot count
/// sit here as integers rather than needing a region of their own.
pub mod act {
    pub const METHOD: usize = 0;
    pub const RECV: usize = 1;
    pub const HOLDER: usize = 2;
    pub const LEXICAL: usize = 3;
    pub const HOME: usize = 4;
    pub const DEAD: usize = 5;
    pub const LOCALS: usize = 6;
}

pub fn new_activation(m: Rc<Method>, nlocals: usize) -> Oop {
    let a = heap::heap()
        .alloc_or_tenure(Shape::indexable(Kind::Activation, 0, act::LOCALS + nlocals));
    heap::set_field(a, act::METHOD, Oop::int(intern_method(m) as i64));
    a
}

pub fn act_method(a: Oop) -> Rc<Method> {
    method_at(heap::field(a, act::METHOD).as_int().expect("activation without a method") as u32)
}

pub fn act_get(a: Oop, i: usize) -> Val {
    from_oop(heap::field(a, i))
}

pub fn act_set(a: Oop, i: usize, v: Val) {
    let w = to_oop(v);
    heap::set_field(a, i, w);
    if w.is_obj() {
        heap::heap().record(a);
    }
}

pub fn act_link(a: Oop, i: usize) -> Option<Oop> {
    let v = heap::field(a, i);
    v.is_obj().then_some(v)
}

pub fn act_set_link(a: Oop, i: usize, v: Option<Oop>) {
    heap::set_field(a, i, v.unwrap_or_else(Oop::null));
    if v.is_some() {
        heap::heap().record(a);
    }
}

pub fn act_locals(a: Oop) -> usize {
    heap::oop_words(a) - act::LOCALS
}

pub fn act_local(a: Oop, i: usize) -> Val {
    from_oop(heap::field(a, act::LOCALS + i))
}

pub fn act_set_local(a: Oop, i: usize, v: Val) {
    act_set(a, act::LOCALS + i, v)
}

pub fn act_dead(a: Oop) -> bool {
    heap::field(a, act::DEAD).as_int() == Some(1)
}

pub fn act_set_dead(a: Oop, d: bool) {
    heap::set_field(a, act::DEAD, Oop::int(d as i64));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::sym;

    fn s(name: &str, kind: SlotKind, v: Val) -> (Sym, SlotKind, Val) {
        (sym(name), kind, v)
    }

    #[test]
    fn a_word_carries_every_kind_of_value() {
        for v in [Val::Int(0), Val::Int(-7), Val::Int(1 << 40)] {
            let back = from_oop(to_oop(v));
            assert!(v.id_eq(&back), "an integer did not survive the word");
        }
        for f in [0.0f64, -1.5, 1e300, f64::MIN_POSITIVE] {
            let back = from_oop(to_oop(Val::Float(f)));
            match back {
                Val::Float(g) => assert_eq!(g.to_bits(), f.to_bits(), "{f} did not survive"),
                _ => panic!("a float came back as something else"),
            }
        }
        let o = make(&[], Payload::None, false);
        assert!(Val::Obj(o).id_eq(&from_oop(to_oop(Val::Obj(o)))));
    }

    #[test]
    fn an_object_remembers_its_slots() {
        let p = make(&[], Payload::None, false);
        let o = make(
            &[
                s("parent", SlotKind::Parent, Val::Obj(p)),
                s("x", SlotKind::Data, Val::Int(3)),
                s("x:", SlotKind::Assign, Val::Int(0)),
            ],
            Payload::None,
            false,
        );
        assert_eq!(slot_count(o), 3);
        assert_eq!(slot_name(o, 1), sym("x"));
        assert_eq!(slot_kind(o, 0), SlotKind::Parent);
        assert_eq!(slot_kind(o, 2), SlotKind::Assign);
        assert!(slot_value(o, 1).id_eq(&Val::Int(3)));
        assert_eq!(find(o, sym("x")), Some(1));
        assert_eq!(find(o, sym("nope")), None);
        assign(o, 1, Val::Int(9));
        assert!(slot_value(o, 1).id_eq(&Val::Int(9)));
    }

    #[test]
    fn every_payload_round_trips() {
        let parent = make(&[], Payload::None, false);
        let ps = [s("parent", SlotKind::Parent, Val::Obj(parent))];

        let b = make(&ps, Payload::Bytes(b"hello world".to_vec()), false);
        assert_eq!(bytes(b), Some(b"hello world".to_vec()));
        assert_eq!(byte_len(b), 11);

        let v = make(&ps, Payload::Vector(vec![Val::Int(1), Val::Obj(parent)]), false);
        let got = elements(v);
        assert_eq!(got.len(), 2);
        assert!(got[0].id_eq(&Val::Int(1)) && got[1].id_eq(&Val::Obj(parent)));

        let m = make(&ps, Payload::Mirror(Val::Obj(parent)), false);
        assert!(from_oop(field(m, F_MIRROR)).id_eq(&Val::Obj(parent)));

        // a live proxy may legitimately point at 0, and a dead one is sealed
        let live = make(&ps, Payload::Proxy(Some(0)), false);
        assert_eq!(proxy(live), Some(0), "fd 0 read as a dead proxy");
        let dead = make(&ps, Payload::Proxy(None), false);
        assert_eq!(proxy(dead), None);
        assert_eq!(slot_count(live), 1, "the payload trampled the slots");
    }

    #[test]
    fn an_activation_holds_a_frame() {
        let m = crate::value::test_method();
        let a = new_activation(m.clone(), 3);
        assert!(Rc::ptr_eq(&act_method(a), &m));
        act_set(a, act::RECV, Val::Int(5));
        act_set_link(a, act::LEXICAL, None);
        act_set_dead(a, false);
        act_set_local(a, 2, Val::Int(77));
        assert!(act_get(a, act::RECV).id_eq(&Val::Int(5)));
        assert_eq!(act_link(a, act::LEXICAL), None);
        assert!(!act_dead(a));
        assert_eq!(act_locals(a), 3);
        assert!(act_local(a, 2).id_eq(&Val::Int(77)));
        act_set_dead(a, true);
        assert!(act_dead(a));
        assert!(act_local(a, 2).id_eq(&Val::Int(77)), "the dead flag hit a local");
    }

    /// A method object's index goes back on the free list when the object is
    /// collected, so `interp::send`'s one-shot methods do not pile up.
    #[test]
    fn a_collected_method_releases_its_index() {
        let before = live_methods();
        let i = intern_method(crate::value::test_method());
        assert_eq!(live_methods(), before + 1);
        release_method(i);
        assert_eq!(live_methods(), before, "the index was not released");
        // and it is handed straight back out
        let j = intern_method(crate::value::test_method());
        assert_eq!(i, j, "a released index was not reused");
        release_method(j);
    }
}
