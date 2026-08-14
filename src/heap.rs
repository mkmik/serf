//! The object arena: variable-size objects in one bump-allocated region,
//! addressed by direct tagged pointers.
//!
//! This is the floor the memory subsystem is being rebuilt on; see MEMORY.md.
//! Nothing in the VM stands on it yet -- `gc.rs` still keeps objects in
//! fixed-size cells behind a handle table -- so everything here is exercised by
//! its own tests until the switch-over.
//!
//! Three things it has to get right, and they are all about the compiler rather
//! than about the collector:
//!
//! * **Tagging must not launder the pointer through an integer.** The C++ VM
//!   carries an empty-asm optimisation barrier in `objects/tag.hh` because
//!   "clang 13+ assumes `this` is aligned and folds the tag bits to zero,
//!   miscompiling every tag accessor" (llvm/llvm-project#59889), and the one
//!   published account of putting a moving collector under a Rust interpreter
//!   needed `std::hint::black_box` for the same reason. Rust has a principled
//!   answer instead: keep the value a pointer and move only its address, with
//!   `map_addr` and `addr`. A `usize` cannot represent a pointer -- casting to
//!   one drops the provenance, and reconstituting it is exactly the ambiguity
//!   that lets the optimiser conclude something false.
//! * **The whole heap is one allocation.** Every object pointer is derived from
//!   its base, so they all share one provenance and an object may be moved from
//!   any part of the heap to any other. This is not tidiness. The first draft
//!   of this module gave each space its own allocation, and Miri rejected
//!   `forwarded()` on the spot: a scavenge rebuilds the reference to the copy
//!   out of a pointer to the corpse, which handed a to-space address the
//!   from-space's provenance. "attempting to access 8 bytes, but got
//!   alloc91680+0x1940 which is at or beyond the end of the allocation".
//! * **No `&` or `&mut` to an object ever outlives the statement that made it.**
//!   Rust assumes a reference's address is fixed; a moving collector makes that
//!   false. Reads and writes go through raw pointers, one word at a time.
//!
//! `cargo miri test heap::` is what says this is true rather than intended.

// Wired up by the switch-over; until then the VM proper does not call in here.
#![allow(dead_code)]

use std::alloc::{alloc_zeroed, Layout};
use std::cell::{Cell, RefCell};

// ------------------------------------------------------------------- the word

/// One word of the Self universe: an object pointer or an immediate integer.
///
/// ```text
/// w & 1 == 0   object pointer, 8-aligned, dereferenced with no masking at all
/// w & 1 == 1   smallint, value = (w as i64) >> 1              (63-bit)
/// ```
///
/// Pointers take the zero tag on purpose. The C++ VM spends two bits and gives
/// `Mem_Tag` the value 1 (`objects/tag.hh:13`), so every deref masks; it had
/// four tags to fit in a 32-bit word and no untagged pointer to spare. On 64
/// bits a deref can be a plain load, which is the instruction a JIT most wants
/// to emit, and integer arithmetic still costs only an add and a decrement.
///
/// Objects are 8-aligned, so bits 1 and 2 are spare for later immediates
/// without disturbing either fast path.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Oop(*mut u64);

const INT_TAG: usize = 1;

/// A smallint is 63-bit. Self's own are 30-bit, and serf has no bignums --
/// `_IntAdd:` is a `checked_add` whose overflow is an error -- so this only
/// moves where that error fires.
pub const INT_MIN: i64 = -(1 << 62);
pub const INT_MAX: i64 = (1 << 62) - 1;

impl Oop {
    /// An immediate integer. It is not a pointer and must never be treated as
    /// one, which `without_provenance_mut` says outright rather than leaving a
    /// cast to imply it.
    pub fn int(v: i64) -> Oop {
        debug_assert!((INT_MIN..=INT_MAX).contains(&v), "smallint out of range: {v}");
        Oop(std::ptr::without_provenance_mut((((v as u64) << 1) | 1) as usize))
    }

    /// The null word: not an object and not an integer. What an untouched arena
    /// word reads as, so a walk over one cannot mistake it for either.
    pub fn null() -> Oop {
        Oop(std::ptr::without_provenance_mut(0))
    }

    fn obj(p: *mut u64) -> Oop {
        debug_assert!(p.addr() & 7 == 0, "object pointer is not 8-aligned");
        Oop(p)
    }

    pub fn is_int(self) -> bool {
        self.0.addr() & INT_TAG != 0
    }

    pub fn is_null(self) -> bool {
        self.0.addr() == 0
    }

    pub fn is_obj(self) -> bool {
        !self.is_int() && !self.is_null()
    }

    pub fn as_int(self) -> Option<i64> {
        if self.is_int() {
            // arithmetic shift, so a negative smallint comes back negative
            Some((self.0.addr() as i64) >> 1)
        } else {
            None
        }
    }

    pub fn addr(self) -> usize {
        self.0.addr()
    }

    fn ptr(self) -> Option<*mut u64> {
        if self.is_obj() {
            Some(self.0)
        } else {
            None
        }
    }
}

impl std::fmt::Debug for Oop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.as_int() {
            Some(i) => write!(f, "{}", i),
            None if self.is_null() => f.write_str("null"),
            None => write!(f, "@{:#x}", self.addr()),
        }
    }
}

// ----------------------------------------------------------------- the header

/// Every object starts with two words.
///
/// ```text
/// word 0  mark:  forwarded:1 │ age:8 │ kind:8 │ hash:23 │ size in words:24
/// word 1  map:   the shape, once maps are heap objects; null until then
/// ```
///
/// This is the C++ VM's header widened: its mark word is
/// `tag:2 │ hash:22 │ age:7 │ marked:1` (`objects/markOop.hh:15`) and its
/// second word is the map pointer. `size` lives here rather than in the map so
/// that a sweep can walk a space with one load per object instead of two.
///
/// When the object has been evacuated the mark word is replaced wholesale by
/// `FORWARDED | new address` -- the trick is `mark_memOop` in
/// `objects/memOop.hh:50`, and it works because the fields it overwrites have
/// gone with the copy.
pub const HEADER_WORDS: usize = 2;

const FORWARDED: u64 = 1 << 63;
const SIZE_BITS: u32 = 24;
const SIZE_MASK: u64 = (1 << SIZE_BITS) - 1;
const HASH_SHIFT: u32 = SIZE_BITS;
const HASH_BITS: u32 = 23;
const HASH_MASK: u64 = (1 << HASH_BITS) - 1;
const KIND_SHIFT: u32 = HASH_SHIFT + HASH_BITS;
const AGE_SHIFT: u32 = KIND_SHIFT + 8;

/// The largest object the header can describe, in words.
pub const MAX_OBJECT_WORDS: usize = SIZE_MASK as usize;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Kind {
    Slots = 0,
    Bytes,
    ObjVector,
    Method,
    Block,
    Mirror,
    Proxy,
    Float,
    Activation,
    Map,
}

const LAST_KIND: u8 = Kind::Map as u8;

impl Kind {
    fn from(b: u8) -> Kind {
        debug_assert!(b <= LAST_KIND, "not a kind: {b}");
        // every value stored came from a `Kind`, and only this module writes
        // the header; the clamp keeps a corrupt heap from being worse than wrong
        unsafe { std::mem::transmute::<u8, Kind>(b.min(LAST_KIND)) }
    }
}

fn mark_of(size: usize, kind: Kind) -> u64 {
    debug_assert!(size <= MAX_OBJECT_WORDS, "object too big: {size} words");
    (size as u64 & SIZE_MASK) | ((kind as u64) << KIND_SHIFT)
}

// -------------------------------------------------------------------- the heap

/// The one allocation. Spaces are carved out of it and objects are carved out
/// of those, so every pointer in the heap shares this pointer's provenance.
///
/// Leaked and never freed, exactly as `gc.rs`'s spaces are, which is what lets
/// an `Oop` be `Copy` with no lifetime attached to anything.
struct Region {
    base: *mut u64,
    words: usize,
    /// words handed out to spaces so far
    carved: Cell<usize>,
    /// the spaces themselves, as word ranges, so a deref can ask whether an
    /// address still means something
    spaces: RefCell<Vec<(usize, usize)>>,
}

fn env_words(name: &str, dflt: usize) -> usize {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(dflt)
}

thread_local! {
    static REGION: Cell<Option<&'static Region>> = const { Cell::new(None) };
}

/// The heap, made on first use, one per thread, never replaced -- the same
/// arrangement `gc()` has.
///
/// `alloc_zeroed` rather than a `Vec`: no `&mut [u64]` ever exists, so there is
/// no reference for these raw pointers to outlive. A large zeroed request goes
/// to the OS as fresh pages, so the resident cost is what is touched, not what
/// is reserved.
fn region() -> &'static Region {
    REGION.with(|r| match r.get() {
        Some(x) => x,
        None => {
            // ponytail: fixed at startup, no growth. The switch-over sizes it
            // for a real world; nothing stands on it yet.
            let words = env_words("SERF_HEAP_WORDS", 1 << 16);
            assert!(words > 0, "the heap needs room for something");
            let p = unsafe { alloc_zeroed(Layout::from_size_align(words * 8, 8).unwrap()) };
            assert!(!p.is_null(), "out of memory for a {words}-word heap");
            let x: &'static Region = Box::leak(Box::new(Region {
                // a pointer *type* cast, which keeps provenance; `as usize` would not
                base: p.cast::<u64>(),
                words,
                carved: Cell::new(0),
                spaces: RefCell::new(vec![]),
            }));
            r.set(Some(x));
            x
        }
    })
}

/// The heap word at index `w`. In bounds by construction, and derived from the
/// one base pointer, so it carries the whole heap's provenance.
fn at(w: usize) -> *mut u64 {
    let r = region();
    debug_assert!(w < r.words, "word {w} is past the end of the heap");
    unsafe { r.base.add(w) }
}

/// Rebuild a pointer from an address read out of the heap. `with_addr`, not a
/// cast: the address came from a word, which has no provenance of its own, and
/// the heap's is the one that covers it.
fn from_addr(a: usize) -> *mut u64 {
    region().base.with_addr(a)
}

fn heap_holds(a: usize) -> bool {
    let r = region();
    a >= r.base.addr() && a < r.base.addr() + r.words * 8
}

/// Does this address lie in a space that still exists? Stronger than
/// `heap_holds`, and the check that catches a pointer into a from-space some
/// collection has already abandoned.
fn in_a_live_space(a: usize) -> bool {
    let r = region();
    if !heap_holds(a) {
        return false;
    }
    let w = (a - r.base.addr()) / 8;
    r.spaces.borrow().iter().any(|(s, n)| w >= *s && w < s + n)
}

/// How much of the heap has been handed out to spaces.
pub fn heap_carved() -> usize {
    region().carved.get()
}

pub fn heap_words() -> usize {
    region().words
}

// ------------------------------------------------------------------- a space

/// A region objects are bump-allocated into and, eventually, copied out of: a
/// young semispace, the old space. A view on the heap, not an allocation of its
/// own -- see the module note on why that matters.
pub struct Space {
    start: usize,
    words: usize,
    bump: Cell<usize>,
}

impl Space {
    /// Carve `words` off the heap. Panics rather than growing: the heap is one
    /// allocation and growing it would move every object in it, which is a
    /// different design.
    pub fn new(words: usize) -> Space {
        let r = region();
        let start = r.carved.get();
        assert!(start + words <= r.words, "heap exhausted: {words} more words wanted");
        r.carved.set(start + words);
        r.spaces.borrow_mut().push((start, words));
        Space { start, words, bump: Cell::new(0) }
    }

    pub fn capacity(&self) -> usize {
        self.words
    }

    pub fn used(&self) -> usize {
        self.bump.get()
    }

    pub fn is_empty(&self) -> bool {
        self.bump.get() == 0
    }

    /// Abandon everything in one store. This is the free: no destructor runs
    /// and nothing is handed back, which is the point of a semispace.
    pub fn reset(&self) {
        self.bump.set(0);
    }

    /// Room for one object, header included. `None` when the space is full --
    /// which in a collector means "tenure it instead", not "find more memory",
    /// so it is an answer rather than an error.
    pub fn alloc(&self, kind: Kind, payload_words: usize) -> Option<Oop> {
        let size = HEADER_WORDS + payload_words;
        if size > MAX_OBJECT_WORDS {
            return None;
        }
        let a = self.bump.get();
        if a + size > self.words {
            return None;
        }
        self.bump.set(a + size);
        let p = at(self.start + a);
        unsafe { p.write(mark_of(size, kind)) };
        Some(Oop::obj(p))
    }

    pub fn contains(&self, o: Oop) -> bool {
        match o.ptr() {
            Some(p) => {
                let base = region().base.addr();
                let w = (p.addr() - base) / 8;
                heap_holds(p.addr()) && w >= self.start && w < self.start + self.words
            }
            None => false,
        }
    }

    /// Every object in the space, in allocation order. Objects are
    /// self-describing, so this needs nothing but the bump pointer -- it is how
    /// a scavenge frees the from-space and how a sweep finds the dead.
    pub fn walk(&self) -> impl Iterator<Item = Oop> + '_ {
        let mut a = 0usize;
        std::iter::from_fn(move || {
            if a >= self.bump.get() {
                return None;
            }
            let o = Oop::obj(at(self.start + a));
            let n = size_words(o);
            debug_assert!(n >= HEADER_WORDS, "object with no header at word {a}");
            a += n;
            Some(o)
        })
    }
}

// ---------------------------------------------------------- object accessors

/// Where an object's words are. Checked, because a pointer into a space that a
/// collection has already abandoned is the failure mode this design has to be
/// afraid of, and it is worth catching at the deref rather than three
/// collections later.
fn words_of(o: Oop) -> *mut u64 {
    let p = o.ptr().expect("not an object");
    debug_assert!(in_a_live_space(p.addr()), "pointer into no live space: {:#x}", p.addr());
    p
}

fn mark(o: Oop) -> u64 {
    unsafe { words_of(o).read() }
}

fn set_mark(o: Oop, m: u64) {
    unsafe { words_of(o).write(m) }
}

pub fn size_words(o: Oop) -> usize {
    (mark(o) & SIZE_MASK) as usize
}

pub fn payload_words(o: Oop) -> usize {
    size_words(o) - HEADER_WORDS
}

pub fn kind(o: Oop) -> Kind {
    Kind::from((mark(o) >> KIND_SHIFT) as u8)
}

pub fn hash(o: Oop) -> u32 {
    ((mark(o) >> HASH_SHIFT) & HASH_MASK) as u32
}

pub fn set_hash(o: Oop, h: u32) {
    let m = mark(o) & !(HASH_MASK << HASH_SHIFT);
    set_mark(o, m | ((h as u64 & HASH_MASK) << HASH_SHIFT));
}

pub fn age(o: Oop) -> u8 {
    (mark(o) >> AGE_SHIFT) as u8
}

pub fn set_age(o: Oop, a: u8) {
    let m = mark(o) & !(0xffu64 << AGE_SHIFT);
    set_mark(o, m | ((a as u64) << AGE_SHIFT));
}

/// The map word. Null until maps become heap objects.
pub fn map(o: Oop) -> Oop {
    Oop(from_addr(unsafe { word_at(o, MAP_WORD).read() } as usize))
}

pub fn set_map(o: Oop, m: Oop) {
    unsafe { word_at(o, MAP_WORD).write(m.addr() as u64) }
}

/// `word_at`'s index for the map word, which sits in the header rather than in
/// the payload.
const MAP_WORD: usize = usize::MAX;

fn word_at(o: Oop, i: usize) -> *mut u64 {
    let p = words_of(o);
    let off = if i == MAP_WORD {
        1
    } else {
        debug_assert!(
            i < payload_words(o),
            "field {i} past the end of a {}-word object",
            size_words(o)
        );
        HEADER_WORDS + i
    };
    unsafe { p.add(off) }
}

pub fn field(o: Oop, i: usize) -> Oop {
    Oop(from_addr(unsafe { word_at(o, i).read() } as usize))
}

pub fn set_field(o: Oop, i: usize, v: Oop) {
    unsafe { word_at(o, i).write(v.addr() as u64) }
}

/// Raw payload word, for the parts of an object that are not references --
/// packed bytes, a bytecode run, a float.
pub fn raw(o: Oop, i: usize) -> u64 {
    unsafe { word_at(o, i).read() }
}

pub fn set_raw(o: Oop, i: usize, v: u64) {
    unsafe { word_at(o, i).write(v) }
}

/// Copy an object's payload verbatim. What an evacuation is made of.
pub fn copy_payload(from: Oop, to: Oop) {
    debug_assert_eq!(payload_words(from), payload_words(to), "copying between different sizes");
    for i in 0..payload_words(from) {
        set_raw(to, i, raw(from, i));
    }
}

// ------------------------------------------------------------------ forwarding

/// Has this object been evacuated, and if so where to?
///
/// The forwarding address replaces the mark word outright, with the top bit
/// set. Nothing is lost: the fields it overwrites went with the copy, and the
/// corpse is about to be abandoned.
pub fn forwarded(o: Oop) -> Option<Oop> {
    let m = mark(o);
    if m & FORWARDED == 0 {
        return None;
    }
    Some(Oop(from_addr((m & !FORWARDED) as usize)))
}

pub fn set_forwarded(o: Oop, to: Oop) {
    debug_assert!(to.is_obj(), "forwarded to something that is not an object");
    set_mark(o, FORWARDED | to.addr() as u64);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_smallint_is_not_a_pointer_and_survives_the_round_trip() {
        for v in [0i64, 1, -1, 7, -7, 1 << 40, -(1 << 40), INT_MIN, INT_MAX] {
            let o = Oop::int(v);
            assert!(o.is_int(), "{v} did not come back an integer");
            assert!(!o.is_obj(), "{v} looks like an object");
            assert_eq!(o.as_int(), Some(v), "{v} did not survive");
        }
        assert!(Oop::null().is_null());
        assert!(!Oop::null().is_obj());
        assert_eq!(Oop::null().as_int(), None);
    }

    #[test]
    fn an_object_pointer_is_aligned_and_untagged() {
        let s = Space::new(64);
        let o = s.alloc(Kind::Slots, 3).unwrap();
        assert!(o.is_obj());
        assert!(!o.is_int(), "an object pointer must not read as an integer");
        assert_eq!(o.addr() & 7, 0, "object pointers carry no tag bits");
        assert_eq!(size_words(o), HEADER_WORDS + 3);
        assert_eq!(payload_words(o), 3);
        assert_eq!(kind(o), Kind::Slots);
        assert!(s.contains(o));
    }

    #[test]
    fn header_fields_do_not_tread_on_each_other() {
        let s = Space::new(64);
        let o = s.alloc(Kind::Method, 5).unwrap();
        set_hash(o, 0x7f_ffff);
        set_age(o, 200);
        assert_eq!(hash(o), 0x7f_ffff);
        assert_eq!(age(o), 200);
        assert_eq!(size_words(o), HEADER_WORDS + 5, "size was trampled");
        assert_eq!(kind(o), Kind::Method, "kind was trampled");
        set_hash(o, 1);
        assert_eq!(age(o), 200, "writing the hash moved the age");
        assert_eq!(kind(o), Kind::Method);
    }

    #[test]
    fn fields_hold_both_kinds_of_word() {
        let s = Space::new(64);
        let o = s.alloc(Kind::Slots, 4).unwrap();
        let other = s.alloc(Kind::Bytes, 1).unwrap();
        for i in 0..4 {
            assert!(field(o, i).is_null(), "a fresh heap word is not null");
        }
        set_field(o, 0, Oop::int(-42));
        set_field(o, 1, other);
        set_raw(o, 2, 0xdead_beef_dead_beef);
        assert_eq!(field(o, 0).as_int(), Some(-42));
        assert_eq!(field(o, 1), other);
        assert!(field(o, 1).is_obj(), "a stored pointer came back as something else");
        assert_eq!(raw(o, 2), 0xdead_beef_dead_beef);
        set_map(o, other);
        assert_eq!(map(o), other);
        assert_eq!(field(o, 0).as_int(), Some(-42), "the map word overlapped a field");
    }

    /// A pointer read back out of the heap must still be usable as a pointer.
    /// This is the property an `as usize` round trip loses and `with_addr`
    /// keeps, and it is why `Oop` is a pointer rather than an integer.
    #[test]
    fn a_pointer_stored_and_reloaded_is_still_dereferenceable() {
        let s = Space::new(64);
        let holder = s.alloc(Kind::Slots, 1).unwrap();
        let target = s.alloc(Kind::Slots, 2).unwrap();
        set_field(target, 0, Oop::int(99));
        set_field(holder, 0, target);
        let back = field(holder, 0);
        assert_eq!(field(back, 0).as_int(), Some(99), "the reloaded pointer lost its way");
        set_field(back, 1, Oop::int(7));
        assert_eq!(field(target, 1).as_int(), Some(7), "the two pointers are not the same object");
    }

    #[test]
    fn the_space_walks_itself() {
        let s = Space::new(256);
        let sizes = [0usize, 1, 4, 2, 9];
        let made: Vec<Oop> = sizes.iter().map(|n| s.alloc(Kind::Slots, *n).unwrap()).collect();
        let seen: Vec<Oop> = s.walk().collect();
        assert_eq!(seen, made, "the walk did not find the objects in order");
        assert_eq!(s.used(), sizes.iter().map(|n| n + HEADER_WORDS).sum::<usize>());
    }

    #[test]
    fn a_full_space_answers_none_rather_than_growing() {
        let s = Space::new(8);
        assert!(s.alloc(Kind::Slots, 2).is_some()); // 4 words
        assert!(s.alloc(Kind::Slots, 2).is_some()); // 8 words, exactly full
        assert!(s.alloc(Kind::Slots, 0).is_none(), "an overfull space allocated");
        assert_eq!(s.used(), 8);
    }

    #[test]
    fn reset_frees_the_whole_space_at_once() {
        let s = Space::new(64);
        s.alloc(Kind::Slots, 4).unwrap();
        assert!(!s.is_empty());
        s.reset();
        assert!(s.is_empty());
        assert_eq!(s.walk().count(), 0);
        // and the space really is reusable
        let o = s.alloc(Kind::Bytes, 1).unwrap();
        assert_eq!(kind(o), Kind::Bytes);
    }

    /// Evacuation, in miniature: copy an object to another space, forward the
    /// corpse, and reach the copy through the forward. Rebuilding a pointer to
    /// one space out of a pointer into another is the operation that has to be
    /// provenance-correct -- it is what made the whole heap one allocation --
    /// so this is the test Miri is here for.
    #[test]
    fn an_object_forwards_to_its_copy_in_another_space() {
        let from = Space::new(64);
        let to = Space::new(64);
        let o = from.alloc(Kind::Slots, 3).unwrap();
        set_hash(o, 12345);
        set_field(o, 0, Oop::int(11));
        set_field(o, 2, Oop::int(33));
        assert!(forwarded(o).is_none(), "a fresh object is already forwarded");

        let copy = to.alloc(kind(o), payload_words(o)).unwrap();
        copy_payload(o, copy);
        set_hash(copy, hash(o));
        set_forwarded(o, copy);

        let f = forwarded(o).expect("the corpse does not point at the copy");
        assert_eq!(f, copy);
        assert!(to.contains(f) && !from.contains(f), "the copy is in the wrong space");
        assert_eq!(field(f, 0).as_int(), Some(11));
        assert_eq!(field(f, 2).as_int(), Some(33));
        assert_eq!(hash(f), 12345);
        assert_eq!(kind(f), Kind::Slots);
    }

    /// A reference held in another object survives its target being evacuated,
    /// which is the whole reason the mark word doubles as a forwarding pointer.
    #[test]
    fn a_reference_follows_its_target_through_an_evacuation() {
        let from = Space::new(64);
        let to = Space::new(64);
        let holder = from.alloc(Kind::Slots, 1).unwrap();
        let target = from.alloc(Kind::Slots, 1).unwrap();
        set_field(target, 0, Oop::int(5));
        set_field(holder, 0, target);

        let copy = to.alloc(kind(target), payload_words(target)).unwrap();
        copy_payload(target, copy);
        set_forwarded(target, copy);

        // what a Cheney scan does: read the field, notice the forward, rewrite
        let old = field(holder, 0);
        if let Some(f) = forwarded(old) {
            set_field(holder, 0, f);
        }
        assert_eq!(field(holder, 0), copy, "the reference did not follow");
        assert_eq!(field(field(holder, 0), 0).as_int(), Some(5));
    }
}
