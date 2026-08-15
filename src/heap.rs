//! The object arena and its collector: variable-size objects in one
//! bump-allocated region, addressed by direct tagged pointers.
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
use std::collections::HashMap;

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

    /// The null word: not an object and not an integer. What an untouched heap
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
/// word 0  mark:  forwarded:1 │ marked:1 │ dirty:1 │ age:8 │ kind:8 │ hash:22 │ size:23
/// word 1  form:  slots:23 │ aux:8 │ annotated:1 │ oops:32
/// word 2  shape: the memoised map, `gen:32 │ map:32`
/// ```
///
/// `oops` is how many leading payload words hold `Oop`s; the rest are raw.
/// `slots` is how many of those `Oop`s are named slots, which is where the
/// payload splits:
///
/// ```text
/// payload[0 .. slots)              slot values            Oop
/// payload[slots .. + annos)        annotations            Oop   (if annotated)
/// payload[.. oops)                 indexable elements     Oop   (an objVector)
/// payload[oops .. oops + slots)    slot descriptors       raw: name:32 │ kind:8
/// payload[oops + slots .. )        indexable bytes        raw   (a string)
/// ```
///
/// The annotation region is one `Oop` for the object's own annotation and one
/// per slot, present only when the object has any. serf's own world has none
/// and should not pay for them; a loaded world has 218,474 and needs them
/// somewhere the collector can see, which a Rust-side table keyed on an address
/// that moves is not.
///
/// Descriptors sit in the raw region because they are not references -- an
/// interned name is a number. When maps arrive they take the descriptors with
/// them and every clone of a shape stops carrying its own copy; the values, the
/// elements and the bytes are what is left, and they are what actually differs.
///
/// This is the C++ VM's mark word widened -- `tag:2 │ hash:22 │ age:7 │
/// marked:1` (`objects/markOop.hh:15`) -- plus the two bits a collector wants
/// on hand: `dirty` for the remembered set and `forwarded` for an evacuation.
/// `size` lives here rather than in the map so that a sweep can walk a space
/// with one load per object instead of two.
///
/// Word 1 is the map pointer in the finished design: a map knows its objects'
/// layout, which is the one thing this word is used for. Until maps are heap
/// objects it holds the count directly. "Leading `Oop`s then raw words" is
/// general enough for everything, because an immediate is a legal `Oop` -- an
/// activation's program counter is `Oop::int(pc)` and lives happily in the
/// scanned region.
///
/// When the object has been evacuated the mark word is replaced wholesale by
/// `FORWARDED | new address` -- the trick is `mark_memOop` in
/// `objects/memOop.hh:50`, and it works because the fields it overwrites have
/// gone with the copy.
pub const HEADER_WORDS: usize = 3;

const SIZE_BITS: u32 = 23;
const SIZE_MASK: u64 = (1 << SIZE_BITS) - 1;
const HASH_SHIFT: u32 = SIZE_BITS;
const HASH_BITS: u32 = 22;
const HASH_MASK: u64 = (1 << HASH_BITS) - 1;
const KIND_SHIFT: u32 = HASH_SHIFT + HASH_BITS;
const AGE_SHIFT: u32 = KIND_SHIFT + 8;
const DIRTY: u64 = 1 << 61;
const MARKED: u64 = 1 << 62;
const FORWARDED: u64 = 1 << 63;

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

/// The one allocation. Spaces are carved out of it and objects out of those, so
/// every pointer in the heap shares this pointer's provenance.
///
/// Leaked and never freed, exactly as `gc.rs`'s spaces are, which is what lets
/// an `Oop` be `Copy` with no lifetime attached to anything.
struct Region {
    base: *mut u64,
    words: usize,
    carved: Cell<usize>,
    /// the spaces, as word ranges, so a deref can ask whether an address still
    /// means something
    spaces: RefCell<Vec<(usize, usize)>>,
}

fn env_words(name: &str, dflt: usize) -> usize {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(dflt)
}

fn young_words() -> usize {
    // Stress collects after every allocation, so a big young space would only
    // make every scavenge walk more of a space that is nearly empty. The cell
    // heap sized itself down for the same reason.
    let dflt = if std::env::var_os("SERF_GC_STRESS").is_some() { 1 << 12 } else { 1 << 19 };
    env_words("SERF_YOUNG_WORDS", dflt)
}

fn old_words() -> usize {
    // A real world is the reason this is large. Clean-4.4 with its outliner
    // caches filled needs more than 2M words, and the old space does not grow:
    // the heap is one allocation, and growing it would move every object in it.
    // Reserving costs address space, not memory -- the pages are zero-filled by
    // the OS as they are touched -- so the number to pick is "more than any
    // world will want", not "what this one uses".
    env_words("SERF_OLD_WORDS", 1 << 24)
}

thread_local! {
    static REGION: Cell<Option<&'static Region>> = const { Cell::new(None) };
}

/// The collector, made on first use: one per thread, never replaced -- the
/// arrangement `gc()` has. Sized from the environment so a big world can be
/// given room without a rebuild.
pub fn heap() -> &'static Heap {
    thread_local! {
        static H: Cell<Option<&'static Heap>> = const { Cell::new(None) };
    }
    H.with(|h| match h.get() {
        Some(x) => x,
        None => {
            let x: &'static Heap = Box::leak(Box::new(Heap::new(young_words(), old_words())));
            h.set(Some(x));
            x
        }
    })
}

/// The region, made on first use, one per thread, never replaced -- the same
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
            // big enough for the spaces `heap()` will carve, plus room for
            // the ones tests make. Zeroed pages are faulted in as they are
            // touched, so this reserves address space rather than memory.
            let want = 2 * young_words() + old_words() + (1 << 18);
            let words = env_words("SERF_HEAP_WORDS", want);
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

/// The heap word at index `w`, derived from the one base pointer so that it
/// carries the whole heap's provenance.
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

/// Does this address lie in a space that still exists? The check that catches a
/// pointer into a space some collection has already abandoned.
fn in_a_live_space(a: usize) -> bool {
    let r = region();
    if !heap_holds(a) {
        return false;
    }
    let w = (a - r.base.addr()) / 8;
    r.spaces.borrow().iter().any(|(s, n)| w >= *s && w < s + n)
}

pub fn heap_words() -> usize {
    region().words
}

pub fn heap_carved() -> usize {
    region().carved.get()
}

// ------------------------------------------------------------------- a space

/// A region objects are bump-allocated into and, eventually, copied out of: a
/// young semispace, or the old space. A view on the heap, not an allocation of
/// its own -- see the module note on why that matters.
pub struct Space {
    /// the heap's base, cached: `is_young` and every allocation ask where this
    /// space is, and going through `region()` for it put a thread-local lookup
    /// on the hottest paths in the VM
    base: *mut u64,
    start: usize,
    words: usize,
    bump: Cell<usize>,
}

const ANNOTATED: u64 = 1 << 31;
const AUX_SHIFT: u32 = 23;
const AUX_MASK: u64 = 0xff;
const SLOTS_MASK: u64 = (1 << AUX_SHIFT) - 1;

fn init_object(p: *mut u64, size: usize, kind: Kind, oops: usize, slots: usize, anno: bool) {
    debug_assert!(oops <= size - HEADER_WORDS, "more oop words than payload");
    debug_assert!(slots <= SLOTS_MASK as usize, "more named slots than the form word holds");
    unsafe {
        p.write(mark_of(size, kind));
        let f = ((slots as u64) << 32) | oops as u64 | if anno { ANNOTATED << 32 } else { 0 };
        p.add(1).write(f);
        p.add(2).write(0); // no memoised shape yet
    }
}

/// What an object is made of, before it exists. `len` is the indexable part:
/// elements for an `ObjVector`, bytes for a `Bytes`, ignored otherwise.
#[derive(Clone, Copy, Debug)]
pub struct Shape {
    pub kind: Kind,
    pub slots: usize,
    /// traced fields: a vector's elements, an activation's locals, a byte
    /// object's bytes
    pub len: usize,
    /// untraced words: a proxy's foreign pointer, a float's bits, a method's
    /// bytecodes -- things that are not references and must not be followed
    pub raw: usize,
    pub annotated: bool,
}

impl Shape {
    pub fn new(kind: Kind, slots: usize) -> Shape {
        Shape { kind, slots, len: 0, raw: 0, annotated: false }
    }

    pub fn indexable(kind: Kind, slots: usize, len: usize) -> Shape {
        Shape { kind, slots, len, raw: 0, annotated: false }
    }

    /// Words the collector will not look at.
    pub fn with_raw(mut self, raw: usize) -> Shape {
        self.raw = raw;
        self
    }

    /// Room for an object annotation and one per slot. A loaded world wants
    /// this; serf's own does not, and does not pay for it.
    pub fn annotated(mut self) -> Shape {
        self.annotated = true;
        self
    }

    fn anno_words(&self) -> usize {
        if self.annotated {
            1 + self.slots
        } else {
            0
        }
    }

    /// (`Oop` words, raw words). A byte object keeps its exact length in a word
    /// of its own, because `size` only counts whole words and a 46-byte string
    /// has to come back 46 bytes long.
    fn words(&self) -> (usize, usize) {
        let head = self.slots + self.anno_words();
        match self.kind {
            // a byte object's `len` is bytes, packed after a length word
            Kind::Bytes => (head, self.slots + 1 + self.len.div_ceil(8) + self.raw),
            // everything else counts `len` in `Oop`s the collector traces: a
            // vector's elements, an activation's receiver, chain and locals
            _ => (head + self.len, self.slots + self.raw),
        }
    }
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
        Space { base: r.base, start, words, bump: Cell::new(0) }
    }

    pub fn capacity(&self) -> usize {
        self.words
    }

    pub fn used(&self) -> usize {
        self.bump.get()
    }

    pub fn free(&self) -> usize {
        self.words - self.bump.get()
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
    pub fn alloc(&self, s: Shape) -> Option<Oop> {
        let (oops, raw) = s.words();
        let o = self.alloc_words(s.kind, oops, s.slots, oops + raw, s.annotated)?;
        zero_payload(o);
        if s.kind == Kind::Bytes {
            set_raw(o, len_word(o), s.len as u64);
        }
        Some(o)
    }

    /// The layout spelled out rather than derived, for a copy: an evacuation
    /// has an object in front of it and needs the same shape, not a fresh one.
    fn alloc_words(
        &self,
        kind: Kind,
        oops: usize,
        slots: usize,
        payload: usize,
        anno: bool,
    ) -> Option<Oop> {
        let size = HEADER_WORDS + payload;
        if size > MAX_OBJECT_WORDS {
            return None;
        }
        let a = self.bump.get();
        if a + size > self.words {
            return None;
        }
        self.bump.set(a + size);
        let p = unsafe { self.base.add(self.start + a) };
        init_object(p, size, kind, oops, slots, anno);
        Some(Oop::obj(p))
    }

    pub fn contains(&self, o: Oop) -> bool {
        match o.ptr() {
            Some(p) => {
                let lo = self.base.addr() + self.start * 8;
                p.addr() >= lo && p.addr() < lo + self.words * 8
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
            let o = Oop::obj(unsafe { self.base.add(self.start + a) });
            let n = walk_size(o);
            debug_assert!(n >= HEADER_WORDS, "object with no header at word {a}");
            a += n;
            Some(o)
        })
    }

    /// The object starting at word `a` of this space, for a Cheney scan that
    /// has to walk objects appearing behind it as it goes.
    fn object_at(&self, a: usize) -> Oop {
        Oop::obj(unsafe { self.base.add(self.start + a) })
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
    debug_assert!(mark(o) & FORWARDED == 0, "the mark word is a forwarding address");
    (mark(o) & SIZE_MASK) as usize
}

/// How far to the next object, for a walk over a space that has been evacuated.
/// An evacuated object's mark word *is* the forwarding address, so its size is
/// no longer in it -- but the copy is the same size, and the copy still has one.
fn walk_size(o: Oop) -> usize {
    match forwarded(o) {
        Some(f) => size_words(f),
        None => size_words(o),
    }
}

pub fn payload_words(o: Oop) -> usize {
    size_words(o) - HEADER_WORDS
}

fn form(o: Oop) -> u64 {
    unsafe { words_of(o).add(1).read() }
}

/// How many leading payload words hold `Oop`s. The rest are raw: slot
/// descriptors, bytes, a bytecode run, the bits of a float.
pub fn oop_words(o: Oop) -> usize {
    (form(o) & 0xffff_ffff) as usize
}

/// How many named slots the object has. Its values are the first `slots` words
/// of the payload and its descriptors are the first `slots` raw words.
pub fn slots(o: Oop) -> usize {
    ((form(o) >> 32) & SLOTS_MASK) as usize
}

/// A byte the VM may use for whatever an object needs remembering about that
/// the collector does not care about. The image writer keeps the map type the
/// object arrived with here, because a Rust-side table keyed on an address
/// cannot survive the address moving.
pub fn aux(o: Oop) -> u8 {
    ((form(o) >> (32 + AUX_SHIFT)) & AUX_MASK) as u8
}

pub fn set_aux(o: Oop, v: u8) {
    let f = form(o) & !((AUX_MASK) << (32 + AUX_SHIFT));
    unsafe { words_of(o).add(1).write(f | ((v as u64) << (32 + AUX_SHIFT))) }
}

pub fn is_annotated(o: Oop) -> bool {
    form(o) & (ANNOTATED << 32) != 0
}

fn anno_words(o: Oop) -> usize {
    if is_annotated(o) {
        1 + slots(o)
    } else {
        0
    }
}

/// The object's own annotation, and one per slot. Self keeps these in the map;
/// serf keeps them here, which is what lets the collector see them instead of
/// needing a write barrier of its own for a Rust-side table.
pub fn obj_anno(o: Oop) -> Oop {
    if is_annotated(o) {
        field(o, slots(o))
    } else {
        Oop::null()
    }
}

pub fn set_obj_anno(o: Oop, v: Oop) {
    debug_assert!(is_annotated(o), "the object has no room for an annotation");
    set_field(o, slots(o), v)
}

pub fn slot_anno(o: Oop, i: usize) -> Oop {
    if is_annotated(o) {
        field(o, slots(o) + 1 + i)
    } else {
        Oop::null()
    }
}

pub fn set_slot_anno(o: Oop, i: usize, v: Oop) {
    debug_assert!(is_annotated(o), "the object has no room for annotations");
    debug_assert!(i < slots(o), "slot {i} has no annotation");
    set_field(o, slots(o) + 1 + i, v)
}

/// The memoised shape: the `LOOKUP_GEN` it was computed at, and the map it came
/// to. Zero means "not computed", and a generation that has moved on means the
/// same. This word is the map pointer in the finished design.
pub fn shape_memo(o: Oop, gen: u32) -> Option<u32> {
    let w = unsafe { words_of(o).add(2).read() };
    if w != 0 && (w >> 32) as u32 == gen {
        Some(w as u32)
    } else {
        None
    }
}

pub fn set_shape_memo(o: Oop, gen: u32, map: u32) {
    unsafe { words_of(o).add(2).write(((gen as u64) << 32) | map as u64) }
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

/// Old enough to need the write barrier. Nothing young ever reaches
/// `PROMOTE_AGE`: it is tenured at that point, and anything born old is
/// stamped with it.
pub fn is_old(o: Oop) -> bool {
    age(o) >= PROMOTE_AGE
}

pub fn age(o: Oop) -> u8 {
    (mark(o) >> AGE_SHIFT) as u8
}

pub fn set_age(o: Oop, a: u8) {
    let m = mark(o) & !(0xffu64 << AGE_SHIFT);
    set_mark(o, m | ((a as u64) << AGE_SHIFT));
}

fn marked(o: Oop) -> bool {
    mark(o) & MARKED != 0
}

/// Set the mark bit; true the first time, i.e. when the contents still have to
/// be walked.
fn set_marked(o: Oop) -> bool {
    let m = mark(o);
    if m & MARKED != 0 {
        return false;
    }
    set_mark(o, m | MARKED);
    true
}

fn clear_marked(o: Oop) {
    let m = mark(o);
    set_mark(o, m & !MARKED);
}

fn dirty(o: Oop) -> bool {
    mark(o) & DIRTY != 0
}

fn set_dirty(o: Oop, d: bool) {
    let m = mark(o);
    set_mark(o, if d { m | DIRTY } else { m & !DIRTY });
}

fn word_at(o: Oop, i: usize) -> *mut u64 {
    let p = words_of(o);
    debug_assert!(
        i < payload_words(o),
        "field {i} past the end of a {}-word object",
        size_words(o)
    );
    unsafe { p.add(HEADER_WORDS + i) }
}

/// Reading a reference must not touch a thread-local. The heap is one
/// allocation, so the word's own pointer carries the provenance that covers
/// whatever it points at -- `from_addr` would go through `region()`, and a
/// `LocalKey::with` on every field read cost more than the collector did.
pub fn field(o: Oop, i: usize) -> Oop {
    let p = word_at(o, i);
    Oop(p.with_addr(unsafe { p.read() } as usize))
}

pub fn set_field(o: Oop, i: usize, v: Oop) {
    unsafe { word_at(o, i).write(v.addr() as u64) }
}

/// Raw payload word, for the parts of an object that are not references.
pub fn raw(o: Oop, i: usize) -> u64 {
    unsafe { word_at(o, i).read() }
}

pub fn set_raw(o: Oop, i: usize, v: u64) {
    unsafe { word_at(o, i).write(v) }
}

/// An untraced word of an object's own, past its descriptors: a proxy's
/// foreign pointer, a float's bits.
fn aux_at(o: Oop) -> usize {
    oop_words(o) + slots(o) + if kind(o) == Kind::Bytes { 1 + ilen(o).div_ceil(8) } else { 0 }
}

pub fn aux_word(o: Oop, i: usize) -> u64 {
    raw(o, aux_at(o) + i)
}

pub fn set_aux_word(o: Oop, i: usize, v: u64) {
    set_raw(o, aux_at(o) + i, v)
}

// -------------------------------------------------------------- named slots

/// A slot's name and kind, packed into the descriptor word. The name is an
/// interned symbol -- `value.rs`'s `Sym` -- which is a number, so it belongs in
/// the raw region where the collector will not mistake it for a reference.
pub fn slot_name(o: Oop, i: usize) -> u32 {
    debug_assert!(i < slots(o), "slot {i} of an object with {} of them", slots(o));
    raw(o, oop_words(o) + i) as u32
}

pub fn slot_kind(o: Oop, i: usize) -> u8 {
    debug_assert!(i < slots(o), "slot {i} of an object with {} of them", slots(o));
    (raw(o, oop_words(o) + i) >> 32) as u8
}

pub fn set_slot_desc(o: Oop, i: usize, name: u32, kind: u8) {
    debug_assert!(i < slots(o), "slot {i} of an object with {} of them", slots(o));
    set_raw(o, oop_words(o) + i, name as u64 | ((kind as u64) << 32));
}

pub fn slot_value(o: Oop, i: usize) -> Oop {
    debug_assert!(i < slots(o), "slot {i} of an object with {} of them", slots(o));
    field(o, i)
}

pub fn set_slot_value(o: Oop, i: usize, v: Oop) {
    debug_assert!(i < slots(o), "slot {i} of an object with {} of them", slots(o));
    set_field(o, i, v)
}

/// By interned name, which is what every hot caller already has. A linear scan,
/// as `Obj::find_sym` is: an object's slots are few, and comparing numbers is
/// what makes it cheap.
pub fn find_slot(o: Oop, name: u32) -> Option<usize> {
    (0..slots(o)).find(|&i| slot_name(o, i) == name)
}

// ---------------------------------------------------------- the indexable part

/// Elements for an `ObjVector`, bytes for a `Bytes`, nothing for anything else.
/// Where a byte object keeps its exact length: after the slot values *and*
/// after their descriptors, which is the whole raw region a byte object has
/// before its bytes begin.
fn len_word(o: Oop) -> usize {
    oop_words(o) + slots(o)
}

pub fn ilen(o: Oop) -> usize {
    match kind(o) {
        Kind::ObjVector => oop_words(o) - elements_at(o),
        Kind::Bytes => raw(o, len_word(o)) as usize,
        _ => 0,
    }
}

fn elements_at(o: Oop) -> usize {
    slots(o) + anno_words(o)
}

pub fn element(o: Oop, i: usize) -> Oop {
    debug_assert!(kind(o) == Kind::ObjVector && i < ilen(o), "element {i} out of range");
    field(o, elements_at(o) + i)
}

pub fn set_element(o: Oop, i: usize, v: Oop) {
    debug_assert!(kind(o) == Kind::ObjVector && i < ilen(o), "element {i} out of range");
    set_field(o, elements_at(o) + i, v)
}

/// Bytes are packed into the raw words after the length. Reading one is a word
/// load and a shift, which is what a word-addressed arena costs for byte data;
/// `Value::bytes()` copies today anyway, so nothing regresses by it.
fn byte_words(o: Oop) -> usize {
    len_word(o) + 1
}

pub fn byte_at(o: Oop, i: usize) -> u8 {
    debug_assert!(kind(o) == Kind::Bytes && i < ilen(o), "byte {i} out of range");
    (raw(o, byte_words(o) + i / 8) >> ((i % 8) * 8)) as u8
}

pub fn set_byte_at(o: Oop, i: usize, b: u8) {
    debug_assert!(kind(o) == Kind::Bytes && i < ilen(o), "byte {i} out of range");
    let w = byte_words(o) + i / 8;
    let sh = (i % 8) * 8;
    set_raw(o, w, (raw(o, w) & !(0xffu64 << sh)) | ((b as u64) << sh));
}

pub fn set_bytes(o: Oop, src: &[u8]) {
    debug_assert_eq!(ilen(o), src.len(), "byte object is the wrong length");
    for (i, b) in src.iter().enumerate() {
        set_byte_at(o, i, *b);
    }
}

pub fn bytes_of(o: Oop) -> Vec<u8> {
    (0..ilen(o)).map(|i| byte_at(o, i)).collect()
}

/// A fresh object reads as nulls and zeroes.
///
/// Not a nicety. A space is reused by resetting a bump pointer, not by being
/// cleared, so the words of a new object are whatever the last occupant left
/// there -- and a stale word that happens to look like a pointer is one the
/// collector will follow into a space that no longer holds what it did. The
/// cell heap could not have this bug, because a cell held an `Option<Obj>`
/// and an absent one was absent. Here it costs a store per word, paid on
/// allocation rather than discovered later.
///
/// Evacuation and cloning skip it: they overwrite every word anyway.
fn zero_payload(o: Oop) {
    for i in 0..payload_words(o) {
        set_raw(o, i, 0);
    }
}

fn copy_payload(from: Oop, to: Oop) {
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
    Some(Oop(words_of(o).with_addr((m & !FORWARDED) as usize)))
}

fn set_forwarded(o: Oop, to: Oop) {
    debug_assert!(to.is_obj(), "forwarded to something that is not an object");
    set_mark(o, FORWARDED | to.addr() as u64);
}

// ------------------------------------------------------------------ the roots

/// Everything the collector must treat as live, handed over as slots it may
/// rewrite. With direct pointers a root is not merely read: an object moves,
/// and every reference to it -- including the VM's own -- has to be updated in
/// place. Miss one and it points into an abandoned space.
pub trait Roots {
    fn each(&mut self, f: &mut dyn FnMut(&mut Oop));

    /// References that must not keep their object alive, handed over after the
    /// collection has decided what lives. Answer `Some(where it went)` to keep
    /// the reference, `None` to drop it.
    ///
    /// The interpreter's one use is the list of activations a tail call
    /// displaced: it compares them for identity and never reads them, so an
    /// entry nothing else holds is useless -- and with a handle table it was
    /// merely useless, where a direct pointer into an abandoned space is not.
    /// `Rc::strong_count` used to answer this question; the collector answers
    /// it now.
    fn weak(&mut self, _f: &mut dyn FnMut(Oop) -> Option<Oop>) {}

    /// An object the collection is about to forget. The heap itself needs no
    /// such hook -- abandoning a space is one store -- but anything holding
    /// Rust memory on an object's behalf does, and during the switch-over a
    /// method still does. Called once per dead object, before its space is
    /// reused, and it must not allocate.
    fn dying(&mut self, _o: Oop) {}

    /// References an object owns that are not in its words. During the
    /// switch-over a method object is one: its literals are still in a Rust
    /// `Method` behind an index, and the collector reaches them through the
    /// object that names it rather than by walking every method there is.
    fn extra(&mut self, _o: Oop, _f: &mut dyn FnMut(&mut Oop)) {}
}

/// Survive this many scavenges and you are tenured, as in `gc.rs`.
const PROMOTE_AGE: u8 = 2;

// -------------------------------------------------------------- the collector

/// Generation scavenging over the arena: two young semispaces copied between by
/// Cheney's algorithm, and an old space swept into free lists.
pub struct Heap {
    young: [Space; 2],
    from: Cell<u8>,
    old: Space,
    /// exact-fit free runs in the old space, by size in words. ponytail: exact
    /// fit, no splitting and no coalescing -- promotion re-promotes the same
    /// sizes over and over, so it fits well; `old_free_words` is what would say
    /// otherwise.
    old_free: RefCell<HashMap<usize, Vec<usize>>>,
    old_live: Cell<usize>,
    /// old objects written since the last scavenge, so it need not scan them all
    remembered: RefCell<Vec<Oop>>,
    pub minors: Cell<u64>,
    pub majors: Cell<u64>,
}

impl Heap {
    pub fn new(young_words: usize, old_words: usize) -> Heap {
        Heap {
            young: [Space::new(young_words), Space::new(young_words)],
            from: Cell::new(0),
            old: Space::new(old_words),
            old_free: RefCell::new(HashMap::new()),
            old_live: Cell::new(0),
            remembered: RefCell::new(vec![]),
            minors: Cell::new(0),
            majors: Cell::new(0),
        }
    }

    fn from_space(&self) -> &Space {
        &self.young[self.from.get() as usize]
    }

    fn to_space(&self) -> &Space {
        &self.young[1 - self.from.get() as usize]
    }

    pub fn young_used(&self) -> usize {
        self.from_space().used()
    }

    pub fn young_capacity(&self) -> usize {
        self.young[0].capacity()
    }

    pub fn old_used(&self) -> usize {
        self.old.used()
    }

    pub fn old_live(&self) -> usize {
        self.old_live.get()
    }

    pub fn old_free_words(&self) -> usize {
        self.old_free.borrow().iter().map(|(n, v)| n * v.len()).sum()
    }

    /// The young space is filling; the interpreter should collect at its next
    /// safepoint. Allocation never collects on its own, because the caller's
    /// Rust locals are not roots.
    pub fn wants_collection(&self) -> bool {
        self.from_space().used() * 4 >= self.from_space().capacity() * 3
    }

    pub fn old_wants_major(&self) -> bool {
        self.old.used() * 4 >= self.old.capacity() * 3
    }

    pub fn remembered_len(&self) -> usize {
        self.remembered.borrow().len()
    }

    /// Allocate in the young generation. `None` means the space is full and the
    /// caller should collect -- allocation never collects on its own, because
    /// the caller's Rust locals are not roots.
    pub fn alloc(&self, s: Shape) -> Option<Oop> {
        self.from_space().alloc(s)
    }

    /// Allocate, putting into the old generation anything the young space
    /// cannot take -- an object bigger than a semispace, or one arriving when
    /// the space is full and no collection is possible yet. The caller gets an
    /// object either way, which is what lets allocation be infallible.
    pub fn alloc_or_tenure(&self, s: Shape) -> Oop {
        match self.alloc(s) {
            Some(o) => o,
            None => {
                let (oops, raw) = s.words();
                // born old with its fields already set and no barrier fired for
                // it: whatever it comes to hold, the next scavenge has to know
                let o = self.alloc_old(s.kind, oops, s.slots, oops + raw, s.annotated);
                zero_payload(o);
                self.record(o);
                if s.kind == Kind::Bytes {
                    set_raw(o, len_word(o), s.len as u64);
                }
                o
            }
        }
    }

    /// Allocate straight into the old generation: what a promotion does, and
    /// what pretenuring does.
    fn alloc_old(
        &self,
        kind: Kind,
        oops: usize,
        slots: usize,
        payload: usize,
        anno: bool,
    ) -> Oop {
        let size = HEADER_WORDS + payload;
        self.old_live.set(self.old_live.get() + 1);
        let o = if let Some(a) = self.old_free.borrow_mut().get_mut(&size).and_then(|v| v.pop()) {
            let p = from_addr(a);
            init_object(p, size, kind, oops, slots, anno);
            Oop::obj(p)
        } else {
            self.old
                .alloc_words(kind, oops, slots, payload, anno)
                .expect("old generation exhausted -- raise SERF_OLD_WORDS")
        };
        // Born old, so it reads as old. The write barrier asks the age rather
        // than the heap: a store already has the object's header in cache, and
        // asking the heap means a thread-local on every store in the VM.
        set_age(o, PROMOTE_AGE);
        o
    }

    /// A clone: the same shape, the same contents, its own identity. `_Clone`.
    pub fn clone_object(&self, o: Oop) -> Oop {
        let (oops, pay, ns, an) = (oop_words(o), payload_words(o), slots(o), is_annotated(o));
        let c = match self.from_space().alloc_words(kind(o), oops, ns, pay, an) {
            Some(c) => c,
            None => {
                let c = self.alloc_old(kind(o), oops, ns, pay, an);
                self.record(c);
                c
            }
        };
        copy_payload(o, c);
        c
    }

    pub fn is_young(&self, o: Oop) -> bool {
        self.young[0].contains(o) || self.young[1].contains(o)
    }

    /// Write barrier. An old object that may now hold a young reference has to
    /// be scanned by the next scavenge, which does not otherwise look at the
    /// old generation. Conservative -- it fires for writes that store no
    /// reference at all -- which is the trade the C++ VM's unconditional card
    /// store makes, and serf's is exact per object rather than per 128-byte
    /// card (`memory/rSet.hh:11`).
    pub fn record(&self, o: Oop) {
        if self.is_young(o) || dirty(o) {
            return;
        }
        set_dirty(o, true);
        self.remembered.borrow_mut().push(o);
    }

    /// A field write that goes through the barrier. The switch-over routes
    /// every mutation of an object word here.
    pub fn store(&self, o: Oop, i: usize, v: Oop) {
        set_field(o, i, v);
        if v.is_obj() {
            self.record(o);
        }
    }

    // ---------------------------------------------------------------- scavenge

    /// Move one object out of the from-space, answering where it went. Idempotent:
    /// the second reference to an object finds the forward the first one left.
    fn evacuate(&self, o: Oop, promoted: &mut Vec<Oop>) -> Oop {
        if let Some(f) = forwarded(o) {
            return f;
        }
        if !self.from_space().contains(o) {
            return o; // already old, or already copied into the to-space
        }
        let (k, oops, pay, ns, an) =
            (kind(o), oop_words(o), payload_words(o), slots(o), is_annotated(o));
        debug_assert!(
            oops <= pay && pay < 1 << 20,
            "corrupt object at {:#x}: kind {:?} oops {} payload {} slots {}",
            o.addr(), k, oops, pay, ns
        );
        let a = age(o).saturating_add(1);
        // Ask the to-space only when the object is actually staying young: the
        // bump happens inside `alloc`, so testing the age afterwards would
        // reserve to-space words for every promoted object and then walk away
        // from them.
        //
        // ponytail: the `None` arm is a safety valve rather than a live path.
        // Equal semispaces cannot overflow -- everything that survives came out
        // of a space the same size -- but promotion is what makes a scavenge
        // unable to fail, and that is worth keeping true by construction rather
        // than by argument (universe.cpp:87 keeps a whole generation in reserve
        // for the same reason).
        let dst = if a >= PROMOTE_AGE {
            self.alloc_old(k, oops, ns, pay, an)
        } else {
            match self.to_space().alloc_words(k, oops, ns, pay, an) {
                Some(d) => d,
                None => self.alloc_old(k, oops, ns, pay, an),
            }
        };
        let tenured = !self.is_young(dst);
        copy_payload(o, dst);
        set_hash(dst, hash(o));
        // An object tenured because the to-space filled up is old with an age
        // that has not reached `PROMOTE_AGE`, and the write barrier asks the
        // age -- so it would be old and not know it, and a young reference
        // stored into it would go unremembered. Stamp it.
        set_age(dst, if self.is_young(dst) { a } else { a.max(PROMOTE_AGE) });
        set_forwarded(o, dst);
        if tenured {
            // a promoted object has not been scanned yet, and the Cheney loop
            // only walks the to-space, so it needs its own queue
            promoted.push(dst);
        }
        dst
    }

    /// Walk one object's `Oop` fields, evacuating what they name and rewriting
    /// them. Answers whether it still points at anything young, which is what
    /// decides membership of the remembered set -- the self-cleaning card of
    /// `rSet.cpp:131`, decided per object rather than per card.
    fn scan(&self, o: Oop, promoted: &mut Vec<Oop>, roots: &mut dyn Roots) -> bool {
        let mut young = false;
        for i in 0..oop_words(o) {
            let v = field(o, i);
            if !v.is_obj() {
                continue;
            }
            debug_assert!(
                !self.from_space().contains(v)
                    || forwarded(v).is_some()
                    || {
                        let m = mark(v);
                        let sz = (m & SIZE_MASK) as usize;
                        sz >= HEADER_WORDS && oop_words(v) <= sz - HEADER_WORDS
                    },
                "scanning {:?} at {:#x} (oops {} slots {}): field {} holds {:#x}, which is not an object",
                kind(o), o.addr(), oop_words(o), slots(o), i, v.addr()
            );
            let n = self.evacuate(v, promoted);
            if n != v {
                set_field(o, i, n);
            }
            young |= self.is_young(n);
        }
        // whatever the VM holds on this object's behalf
        roots.extra(o, &mut |slot| {
            if slot.is_obj() {
                let n = self.evacuate(*slot, promoted);
                if n != *slot {
                    *slot = n;
                }
                young |= self.is_young(n);
            }
        });
        young
    }

    /// A minor collection: copy the young survivors into the to-space or the
    /// old generation, then abandon everything left behind.
    pub fn scavenge(&self, roots: &mut dyn Roots) {
        let mut promoted: Vec<Oop> = vec![];

        // the remembered set is rebuilt as objects are scanned, so clearing the
        // dirty bits here keeps bit and membership in step
        let rs = std::mem::take(&mut *self.remembered.borrow_mut());
        for o in rs.iter() {
            set_dirty(*o, false);
        }

        roots.each(&mut |slot| {
            if slot.is_obj() {
                *slot = self.evacuate(*slot, &mut promoted);
            }
        });
        for o in rs {
            if self.scan(o, &mut promoted, roots) {
                self.record(o);
            }
        }

        // Cheney: the to-space is the queue. Objects appear behind `scan` as
        // they are evacuated, so this loop finishes when it catches the bump.
        let mut cursor = 0usize;
        loop {
            let to = self.to_space();
            if cursor < to.used() {
                let o = to.object_at(cursor);
                cursor += size_words(o);
                self.scan(o, &mut promoted, roots);
                continue;
            }
            match promoted.pop() {
                Some(o) => {
                    if self.scan(o, &mut promoted, roots) {
                        self.record(o);
                    }
                }
                None => break,
            }
        }

        // weak references, now that what lives has been decided: an object
        // still sitting in the from-space was never reached
        {
            let from = self.from_space();
            roots.weak(&mut |o| {
                if !o.is_obj() || !from.contains(o) {
                    Some(o)
                } else {
                    forwarded(o)
                }
            });
        }

        // whatever is still in the from-space was never reached. Forgetting it
        // is the free -- one store -- but the walk first, because a dead object
        // may be the last thing holding some Rust memory on its own account.
        for o in self.from_space().walk() {
            if forwarded(o).is_none() {
                roots.dying(o);
            }
        }
        self.from_space().reset();
        self.from.set(1 - self.from.get());
        self.minors.set(self.minors.get() + 1);
    }

    // ------------------------------------------------------------ mark & sweep

    fn mark_from(&self, o: Oop, work: &mut Vec<Oop>) {
        if o.is_obj() && set_marked(o) {
            work.push(o);
        }
    }

    /// Mark the whole reachable graph, young and old: an old object is often
    /// only reachable through a young one.
    fn mark_all(&self, roots: &mut dyn Roots) {
        let mut work: Vec<Oop> = vec![];
        roots.each(&mut |slot| {
            if slot.is_obj() && set_marked(*slot) {
                work.push(*slot);
            }
        });
        while let Some(o) = work.pop() {
            for i in 0..oop_words(o) {
                let v = field(o, i);
                self.mark_from(v, &mut work);
            }
            roots.extra(o, &mut |slot| {
                if slot.is_obj() && set_marked(*slot) {
                    work.push(*slot);
                }
            });
        }
    }

    /// Drop every unmarked old object onto the free list and clear the marks on
    /// the rest. Runs before the scavenge, so a dead old object cannot drag its
    /// young referents through one more copy.
    fn sweep_old(&self, roots: &mut dyn Roots) {
        let mut free = self.old_free.borrow_mut();
        // rebuilt from scratch: a run that was free stays free, and this is
        // where it is rediscovered
        free.clear();
        let mut live = 0usize;
        let mut dead: Vec<Oop> = vec![];
        for o in self.old.walk() {
            if marked(o) {
                clear_marked(o);
                live += 1;
            } else {
                if dirty(o) {
                    set_dirty(o, false);
                }
                dead.push(o);
                free.entry(size_words(o)).or_default().push(o.addr());
            }
        }
        self.old_live.set(live);
        drop(free);
        for o in dead {
            roots.dying(o);
        }
        // a swept object may have been on the remembered set
        self.remembered.borrow_mut().retain(|o| dirty(*o));
    }

    /// Replace every reference to `from` with `to`, everywhere.
    ///
    /// This is `universe::switch_pointers` (`memory/universe.cpp:315`), and it
    /// is the bill that comes with direct pointers. An object is a fixed run of
    /// words, so `_AddSlots:` cannot grow one in place: it builds a wider object
    /// and has to make the world stop naming the old one. With a handle table
    /// this was a single store; here it is a walk of both generations and every
    /// root.
    ///
    /// Affordable because of *when* it happens -- programming a world, not
    /// running one. `serf_switch_pointers_total` is what would say otherwise.
    pub fn switch_pointers(&self, roots: &mut dyn Roots, from: Oop, to: Oop) {
        debug_assert!(from.is_obj() && to.is_obj(), "switching something that is not an object");
        roots.each(&mut |slot| {
            if *slot == from {
                *slot = to;
            }
        });
        for space in [self.from_space(), &self.old] {
            for o in space.walk() {
                if o == from {
                    continue; // the corpse may keep naming itself
                }
                for i in 0..oop_words(o) {
                    if field(o, i) == from {
                        set_field(o, i, to);
                        self.record(o);
                    }
                }
            }
        }
        crate::metrics::switched();
    }

    /// Walk every space and check that each object still describes itself.
    /// A moving collector's failures are all of one shape -- a word that is
    /// not what it says it is -- and the only useful place to notice is the
    /// step *before* the one that trips over it.
    pub fn verify(&self, when: &str) {
        for (which, sp) in [("young", self.from_space()), ("old", &self.old)] {
            let mut w = 0usize;
            while w < sp.used() {
                let o = Oop::obj(at(sp.start + w));
                let m = mark(o);
                let size = (m & SIZE_MASK) as usize;
                let bad = m & FORWARDED != 0
                    || size < HEADER_WORDS
                    || w + size > sp.used()
                    || oop_words(o) > size - HEADER_WORDS
                    || slots(o) > oop_words(o);
                assert!(
                    !bad,
                    "{when}: {which} space, word {w}: object at {:#x} says size {} oops {} slots {}",
                    o.addr(),
                    size,
                    oop_words(o),
                    slots(o)
                );
                w += size;
            }
        }
    }

    /// Collect. `major` sweeps the old generation as well.
    pub fn collect(&self, roots: &mut dyn Roots, major: bool) {
        if VERIFY.with(|v| *v) {
            self.verify("before");
        }
        if major {
            self.mark_all(roots);
            // before the sweep, because a swept run goes on the free list and
            // the scavenge's promotions may take it straight back
            roots.weak(&mut |o| (!o.is_obj() || marked(o)).then_some(o));
            self.sweep_old(roots);
            self.majors.set(self.majors.get() + 1);
        }
        self.scavenge(roots);
        if VERIFY.with(|v| *v) {
            self.verify("after");
        }
    }
}

thread_local! {
    /// `SERF_HEAP_VERIFY=1`: walk every space before and after each collection.
    /// Its own flag, not `SERF_GC_VERIFY`: this is O(heap) per collection, so
    /// on a loaded world it is a debugging session rather than a test.
    static VERIFY: bool = std::env::var_os("SERF_HEAP_VERIFY").is_some();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Roots as a plain list of slots, which is all the collector asks for.
    pub struct Vars(pub Vec<Oop>);

    impl Roots for Vars {
        fn each(&mut self, f: &mut dyn FnMut(&mut Oop)) {
            for o in self.0.iter_mut() {
                f(o);
            }
        }
    }

    fn heap() -> Heap {
        Heap::new(512, 2048)
    }

    /// A `Slots` object of `n` fields, all of them `Oop`s.
    fn obj(h: &Heap, n: usize) -> Oop {
        h.alloc(Shape::new(Kind::Slots, n)).expect("young space full")
    }

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
        let h = heap();
        let o = h.alloc(Shape::indexable(Kind::ObjVector, 2, 1)).unwrap();
        assert!(o.is_obj());
        assert!(!o.is_int(), "an object pointer must not read as an integer");
        assert_eq!(o.addr() & 7, 0, "object pointers carry no tag bits");
        assert_eq!(slots(o), 2);
        assert_eq!(ilen(o), 1);
        assert_eq!(oop_words(o), 3, "two slot values and one element");
        assert_eq!(payload_words(o), 5, "...and two descriptor words");
        assert_eq!(size_words(o), HEADER_WORDS + 5);
        assert_eq!(kind(o), Kind::ObjVector);
    }

    #[test]
    fn header_fields_do_not_tread_on_each_other() {
        let h = heap();
        let o = h.alloc(Shape::new(Kind::Method, 5)).unwrap();
        set_hash(o, HASH_MASK as u32);
        set_age(o, 200);
        set_dirty(o, true);
        assert!(set_marked(o));
        assert_eq!(hash(o), HASH_MASK as u32);
        assert_eq!(age(o), 200);
        assert!(dirty(o) && marked(o));
        assert_eq!(size_words(o), HEADER_WORDS + 10, "size was trampled");
        assert_eq!(kind(o), Kind::Method, "kind was trampled");
        assert_eq!(oop_words(o), 5);
        assert_eq!(slots(o), 5, "the slot count was trampled");
        set_hash(o, 1);
        assert_eq!(age(o), 200, "writing the hash moved the age");
        assert!(dirty(o) && marked(o), "writing the hash moved a flag");
        clear_marked(o);
        assert!(dirty(o) && !marked(o));
    }

    #[test]
    fn fields_hold_both_kinds_of_word() {
        let h = heap();
        let o = h.alloc(Shape::indexable(Kind::ObjVector, 2, 2)).unwrap();
        let other = obj(&h, 1);
        assert!(field(o, 0).is_null(), "a fresh heap word is not null");
        set_field(o, 0, Oop::int(-42));
        set_field(o, 1, other);
        set_raw(o, 2, 0xdead_beef_dead_beef);
        assert_eq!(field(o, 0).as_int(), Some(-42));
        assert_eq!(field(o, 1), other);
        assert!(field(o, 1).is_obj(), "a stored pointer came back as something else");
        assert_eq!(raw(o, 2), 0xdead_beef_dead_beef);
    }

    /// A pointer read back out of the heap must still be usable as a pointer.
    /// This is the property an `as usize` round trip loses and `with_addr`
    /// keeps, and it is why `Oop` is a pointer rather than an integer.
    #[test]
    fn a_pointer_stored_and_reloaded_is_still_dereferenceable() {
        let h = heap();
        let holder = obj(&h, 1);
        let target = obj(&h, 2);
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
        let made: Vec<Oop> = sizes.iter().map(|n| s.alloc(Shape::new(Kind::Slots, *n)).unwrap()).collect();
        let seen: Vec<Oop> = s.walk().collect();
        assert_eq!(seen, made, "the walk did not find the objects in order");
        assert_eq!(s.used(), sizes.iter().map(|n| 2 * n + HEADER_WORDS).sum::<usize>());
    }

    #[test]
    fn a_full_space_answers_none_rather_than_growing() {
        let each = HEADER_WORDS + 4; // two slot values and two descriptors
        let s = Space::new(2 * each);
        assert!(s.alloc(Shape::new(Kind::Slots, 2)).is_some());
        assert!(s.alloc(Shape::new(Kind::Slots, 2)).is_some(), "exactly full is still room");
        assert!(s.alloc(Shape::new(Kind::Slots, 0)).is_none(), "an overfull space allocated");
        assert_eq!(s.used(), 2 * each);
    }

    // ----------------------------------------------------------- collection

    #[test]
    fn a_scavenge_keeps_the_reachable_and_forgets_the_rest() {
        let h = heap();
        let keep = obj(&h, 1);
        set_field(keep, 0, Oop::int(7));
        let _lost = obj(&h, 4);
        let before = h.young_used();

        let mut roots = Vars(vec![keep]);
        h.scavenge(&mut roots);

        let keep = roots.0[0];
        assert!(h.young_used() < before, "nothing was reclaimed");
        assert_eq!(h.young_used(), HEADER_WORDS + 2, "more than the survivor came across");
        assert_eq!(field(keep, 0).as_int(), Some(7), "the survivor lost its contents");
        assert_eq!(age(keep), 1);
    }

    #[test]
    fn a_root_is_rewritten_to_where_its_object_went() {
        let h = heap();
        let o = obj(&h, 1);
        set_field(o, 0, Oop::int(1));
        let mut roots = Vars(vec![o]);
        h.scavenge(&mut roots);
        assert_ne!(roots.0[0], o, "the root was not moved");
        assert_eq!(forwarded(o), Some(roots.0[0]), "the corpse does not point at the copy");
        assert_eq!(field(roots.0[0], 0).as_int(), Some(1));
    }

    #[test]
    fn a_reference_between_survivors_follows_its_target() {
        let h = heap();
        let a = obj(&h, 1);
        let b = obj(&h, 1);
        set_field(a, 0, b);
        set_field(b, 0, Oop::int(5));
        let mut roots = Vars(vec![a]);
        h.scavenge(&mut roots);

        let a = roots.0[0];
        let b2 = field(a, 0);
        assert_ne!(b2, b, "the referent did not move");
        assert!(b2.is_obj(), "the reference is no longer an object");
        assert_eq!(field(b2, 0).as_int(), Some(5), "the reference points at the wrong thing");
    }

    /// A cycle must terminate and must not be copied twice.
    #[test]
    fn a_cycle_survives_exactly_once() {
        let h = heap();
        let a = obj(&h, 1);
        let b = obj(&h, 1);
        set_field(a, 0, b);
        set_field(b, 0, a);
        let mut roots = Vars(vec![a]);
        h.scavenge(&mut roots);

        let a = roots.0[0];
        let b2 = field(a, 0);
        assert_eq!(field(b2, 0), a, "the cycle did not close back on the copy");
        assert_eq!(h.young_used(), 2 * (HEADER_WORDS + 2), "an object was copied twice");
    }

    #[test]
    fn a_survivor_is_tenured_once_it_is_old_enough() {
        let h = heap();
        let o = obj(&h, 1);
        set_field(o, 0, Oop::int(3));
        let mut roots = Vars(vec![o]);
        for _ in 0..PROMOTE_AGE {
            h.scavenge(&mut roots);
        }
        let o = roots.0[0];
        assert!(!h.is_young(o), "a survivor was never tenured");
        assert_eq!(h.old_live(), 1);
        assert_eq!(field(o, 0).as_int(), Some(3), "tenuring lost the contents");
        // and it stops moving
        let before = roots.0[0];
        h.scavenge(&mut roots);
        assert_eq!(roots.0[0], before, "an old object was moved by a scavenge");
    }

    /// The scavenge does not scan the old generation, so only the write barrier
    /// can save a young object that only an old one still names.
    #[test]
    fn the_remembered_set_saves_a_young_object_an_old_one_holds() {
        let h = heap();
        let holder = obj(&h, 1);
        let mut roots = Vars(vec![holder]);
        for _ in 0..PROMOTE_AGE {
            h.scavenge(&mut roots);
        }
        let holder = roots.0[0];
        assert!(!h.is_young(holder), "the holder was not tenured");

        let young = obj(&h, 1);
        set_field(young, 0, Oop::int(42));
        h.store(holder, 0, young);
        assert_eq!(h.remembered_len(), 1, "the barrier did not fire");

        h.scavenge(&mut roots);
        let survivor = field(roots.0[0], 0);
        assert!(survivor.is_obj(), "the young referent was lost");
        assert_eq!(field(survivor, 0).as_int(), Some(42));
        assert!(h.is_young(survivor));
        // still pointing at something young, so still remembered
        assert_eq!(h.remembered_len(), 1);
    }

    #[test]
    fn an_old_object_leaves_the_remembered_set_once_it_points_at_nothing_young() {
        let h = heap();
        let holder = obj(&h, 1);
        let mut roots = Vars(vec![holder]);
        for _ in 0..PROMOTE_AGE {
            h.scavenge(&mut roots);
        }
        let holder = roots.0[0];
        h.store(holder, 0, Oop::int(1)); // an integer is not a reference
        assert_eq!(h.remembered_len(), 0, "an integer store was remembered");

        let young = obj(&h, 0);
        h.store(holder, 0, young);
        assert_eq!(h.remembered_len(), 1);
        // let the referent be tenured too, and the holder should fall out
        for _ in 0..PROMOTE_AGE + 1 {
            h.scavenge(&mut roots);
        }
        assert_eq!(h.remembered_len(), 0, "the remembered set never empties");
    }

    #[test]
    fn only_a_major_reclaims_the_old_generation() {
        let h = heap();
        let doomed = obj(&h, 1);
        let mut roots = Vars(vec![doomed]);
        for _ in 0..PROMOTE_AGE {
            h.scavenge(&mut roots);
        }
        assert_eq!(h.old_live(), 1);
        let used = h.old_used();

        roots.0.clear(); // nothing reaches it now
        h.scavenge(&mut roots);
        assert_eq!(h.old_live(), 1, "a minor collection swept the old generation");

        h.collect(&mut roots, true);
        assert_eq!(h.old_live(), 0, "a major collection did not sweep it");
        assert_eq!(h.old_free_words(), used, "the swept run did not reach the free list");
    }

    #[test]
    fn a_swept_run_is_handed_back_to_the_next_promotion() {
        let h = heap();
        let doomed = obj(&h, 3);
        let mut roots = Vars(vec![doomed]);
        for _ in 0..PROMOTE_AGE {
            h.scavenge(&mut roots);
        }
        let used = h.old_used();
        roots.0.clear();
        h.collect(&mut roots, true);
        assert!(h.old_free_words() > 0);

        // an object of the same shape should land in the hole, not past it
        let next = obj(&h, 3);
        roots.0.push(next);
        for _ in 0..PROMOTE_AGE {
            h.scavenge(&mut roots);
        }
        assert_eq!(h.old_used(), used, "the old space grew instead of reusing the run");
        assert_eq!(h.old_free_words(), 0, "the free run was not taken");
    }

    #[test]
    fn a_major_keeps_what_is_reachable_only_through_a_young_object() {
        let h = heap();
        let old = obj(&h, 1);
        let mut roots = Vars(vec![old]);
        for _ in 0..PROMOTE_AGE {
            h.scavenge(&mut roots);
        }
        let old = roots.0[0];
        assert!(!h.is_young(old));
        set_field(old, 0, Oop::int(77));

        // a fresh young object is the only thing naming it, and it is the root
        let young = obj(&h, 1);
        h.store(young, 0, old);
        roots.0[0] = young;

        h.collect(&mut roots, true);
        let reached = field(roots.0[0], 0);
        assert_eq!(h.old_live(), 1, "the old object was swept out from under a live reference");
        assert_eq!(field(reached, 0).as_int(), Some(77));
    }

    /// Allocation is infallible: what the young space cannot take is born old.
    /// Both reasons it cannot -- too big for a semispace at all, and a space
    /// that has filled up before a collection could run.
    #[test]
    fn what_the_young_space_cannot_take_is_born_old() {
        let h = Heap::new(64, 2048);
        let big = h.alloc_or_tenure(Shape::indexable(Kind::Bytes, 0, 1600));
        assert!(!h.is_young(big), "an object bigger than a semispace stayed young");
        assert_eq!(ilen(big), 1600, "the pretenured object is the wrong length");

        let mut roots = Vars(vec![big]);
        while h.alloc(Shape::new(Kind::Slots, 1)).is_some() {}
        let crammed = h.alloc_or_tenure(Shape::new(Kind::Slots, 1));
        assert!(!h.is_young(crammed), "an allocation into a full space was not tenured");
        set_field(crammed, 0, Oop::int(9));
        roots.0.push(crammed);

        // clear the filler out again -- none of it is reachable
        h.scavenge(&mut roots);
        assert_eq!(h.young_used(), 0, "the unreachable filler survived");

        // a pretenured object is remembered, so whatever it holds survives
        let young = obj(&h, 1);
        set_field(young, 0, Oop::int(4));
        h.store(crammed, 0, young);
        h.scavenge(&mut roots);
        let held = field(roots.0[1], 0);
        assert!(held.is_obj(), "a pretenured object's young referent was lost");
        assert_eq!(field(held, 0).as_int(), Some(4));
    }

    // ------------------------------------------------------- the object model

    const DATA: u8 = 0;
    const PARENT: u8 = 1;

    /// Named slots: values in the scanned region, names and kinds in the raw
    /// one, and the two indexed the same way.
    #[test]
    fn an_object_carries_its_slots() {
        let h = heap();
        let o = h.alloc(Shape::new(Kind::Slots, 3)).unwrap();
        let parent = obj(&h, 0);
        set_slot_desc(o, 0, 7, PARENT);
        set_slot_value(o, 0, parent);
        set_slot_desc(o, 1, 11, DATA);
        set_slot_value(o, 1, Oop::int(42));
        set_slot_desc(o, 2, 13, DATA);
        set_slot_value(o, 2, Oop::null());

        assert_eq!(slots(o), 3);
        assert_eq!(slot_name(o, 0), 7);
        assert_eq!(slot_kind(o, 0), PARENT);
        assert_eq!(slot_value(o, 0), parent);
        assert_eq!(slot_name(o, 1), 11);
        assert_eq!(slot_kind(o, 1), DATA, "the name overwrote the kind");
        assert_eq!(slot_value(o, 1).as_int(), Some(42));
        assert_eq!(find_slot(o, 11), Some(1));
        assert_eq!(find_slot(o, 13), Some(2));
        assert_eq!(find_slot(o, 99), None);
    }

    #[test]
    fn a_byte_object_is_its_bytes() {
        let h = heap();
        let text = b"hello: 720, and a tail long enough to need three words";
        let o = h.alloc(Shape::indexable(Kind::Bytes, 1, text.len())).unwrap();
        set_slot_desc(o, 0, 1, PARENT);
        set_bytes(o, text);
        assert_eq!(ilen(o), text.len(), "the length word did not survive the slots");
        assert_eq!(bytes_of(o), text, "the bytes did not round-trip");
        assert_eq!(byte_at(o, 0), b'h');
        assert_eq!(byte_at(o, text.len() - 1), b's');
        set_byte_at(o, 0, b'H');
        assert_eq!(byte_at(o, 0), b'H');
        assert_eq!(byte_at(o, 1), b'e', "writing one byte disturbed its neighbour");
        assert_eq!(slot_name(o, 0), 1, "the bytes ran over the descriptor");
    }

    /// An empty string and a one-byte one are the edges the length word and the
    /// word rounding both live on.
    #[test]
    fn a_byte_object_survives_its_edges() {
        let h = heap();
        for n in [0usize, 1, 7, 8, 9] {
            let src: Vec<u8> = (0..n).map(|i| i as u8 + 1).collect();
            let o = h.alloc(Shape::indexable(Kind::Bytes, 0, n)).unwrap();
            set_bytes(o, &src);
            assert_eq!(ilen(o), n);
            assert_eq!(bytes_of(o), src, "a {n}-byte object did not round-trip");
        }
    }

    #[test]
    fn a_vector_holds_references_the_collector_can_see() {
        let h = heap();
        let v = h.alloc(Shape::indexable(Kind::ObjVector, 1, 3)).unwrap();
        set_slot_desc(v, 0, 1, PARENT);
        let a = obj(&h, 1);
        set_slot_desc(a, 0, 5, DATA);
        set_slot_value(a, 0, Oop::int(1));
        set_element(v, 0, a);
        set_element(v, 1, Oop::int(2));
        set_element(v, 2, Oop::null());
        assert_eq!(ilen(v), 3);
        assert_eq!(element(v, 0), a);
        assert_eq!(element(v, 1).as_int(), Some(2));

        let mut roots = Vars(vec![v]);
        h.scavenge(&mut roots);
        let v = roots.0[0];
        assert_eq!(ilen(v), 3, "the vector lost its length");
        let a2 = element(v, 0);
        assert!(a2.is_obj() && a2 != a, "the element did not follow its object");
        assert_eq!(slot_value(a2, 0).as_int(), Some(1));
        assert_eq!(element(v, 1).as_int(), Some(2), "an immediate element was disturbed");
        assert_eq!(slot_name(v, 0), 1, "the descriptor did not survive the copy");
    }

    /// The whole layout through a collection: slot values traced, elements
    /// traced, descriptors and bytes copied verbatim and not mistaken for
    /// references.
    #[test]
    fn the_whole_layout_survives_a_scavenge() {
        let h = heap();
        let text = b"a string with bytes that look like pointers: \x08\x10\x18";
        let s = h.alloc(Shape::indexable(Kind::Bytes, 1, text.len())).unwrap();
        set_slot_desc(s, 0, 2, PARENT);
        set_bytes(s, text);

        let o = h.alloc(Shape::new(Kind::Slots, 2)).unwrap();
        set_slot_desc(o, 0, 3, DATA);
        set_slot_value(o, 0, s);
        set_slot_desc(o, 1, 4, DATA);
        set_slot_value(o, 1, Oop::int(-9));

        let mut roots = Vars(vec![o]);
        h.scavenge(&mut roots);

        let o = roots.0[0];
        assert_eq!(slot_name(o, 0), 3);
        assert_eq!(slot_name(o, 1), 4);
        assert_eq!(slot_value(o, 1).as_int(), Some(-9));
        let s2 = slot_value(o, 0);
        assert!(s2.is_obj() && s2 != s, "the string did not move with its holder");
        assert_eq!(bytes_of(s2), text, "the bytes were traced as if they were references");
        assert_eq!(slot_name(s2, 0), 2);
    }

    #[test]
    fn a_clone_shares_the_shape_and_nothing_else() {
        let h = heap();
        let o = h.alloc(Shape::new(Kind::Slots, 2)).unwrap();
        set_slot_desc(o, 0, 3, PARENT);
        set_slot_value(o, 0, Oop::int(1));
        set_slot_desc(o, 1, 4, DATA);
        set_slot_value(o, 1, Oop::int(2));
        set_hash(o, 55);

        let c = h.clone_object(o);
        assert_ne!(c, o, "a clone is the same object");
        assert_eq!(slots(c), 2);
        assert_eq!(slot_name(c, 1), 4);
        assert_eq!(slot_kind(c, 0), PARENT);
        assert_eq!(slot_value(c, 1).as_int(), Some(2));
        set_slot_value(c, 1, Oop::int(9));
        assert_eq!(slot_value(o, 1).as_int(), Some(2), "the clone shares its values");
        assert_ne!(hash(c), 55, "a clone inherited its prototype's identity hash");

        let text = b"cloned";
        let s = h.alloc(Shape::indexable(Kind::Bytes, 0, text.len())).unwrap();
        set_bytes(s, text);
        let sc = h.clone_object(s);
        assert_eq!(bytes_of(sc), text, "a byte object's clone lost its bytes");
        assert_eq!(ilen(sc), text.len());
    }

    #[test]
    fn untraced_words_are_not_followed() {
        let h = heap();
        // a proxy's foreign pointer is an arbitrary integer that must never be
        // mistaken for a reference, however much it looks like one
        let p = h.alloc(Shape::new(Kind::Proxy, 1).with_raw(1)).unwrap();
        set_slot_desc(p, 0, 1, PARENT);
        set_slot_value(p, 0, Oop::int(3));
        let fake = h.alloc(Shape::new(Kind::Slots, 0)).unwrap().addr() as u64;
        set_aux_word(p, 0, fake);

        let mut roots = Vars(vec![p]);
        h.scavenge(&mut roots);
        let p = roots.0[0];
        assert_eq!(aux_word(p, 0), fake, "the foreign pointer was rewritten");
        assert_eq!(slot_value(p, 0).as_int(), Some(3));
        assert_eq!(slot_name(p, 0), 1);
    }

    #[test]
    fn annotations_live_in_the_object_and_are_traced() {
        let h = heap();
        let plain = h.alloc(Shape::new(Kind::Slots, 2)).unwrap();
        assert!(!is_annotated(plain));
        assert!(obj_anno(plain).is_null(), "an unannotated object invented one");
        assert!(slot_anno(plain, 0).is_null());

        let o = h.alloc(Shape::new(Kind::Slots, 2).annotated()).unwrap();
        assert!(is_annotated(o));
        set_slot_desc(o, 0, 21, DATA);
        set_slot_value(o, 0, Oop::int(1));
        set_slot_desc(o, 1, 22, DATA);
        set_slot_value(o, 1, Oop::int(2));

        let note = h.alloc(Shape::indexable(Kind::Bytes, 0, 4)).unwrap();
        set_bytes(note, b"note");
        set_obj_anno(o, note);
        set_slot_anno(o, 1, Oop::int(7));

        let mut roots = Vars(vec![o]);
        h.scavenge(&mut roots);
        let o = roots.0[0];
        assert_eq!(slot_value(o, 0).as_int(), Some(1), "an annotation displaced a slot value");
        assert_eq!(slot_name(o, 1), 22, "an annotation displaced a descriptor");
        assert_eq!(slot_anno(o, 1).as_int(), Some(7));
        let note2 = obj_anno(o);
        assert!(note2.is_obj() && note2 != note, "the annotation was not traced");
        assert_eq!(bytes_of(note2), b"note");
    }

    #[test]
    fn an_annotated_vector_keeps_its_elements_and_its_notes_apart() {
        let h = heap();
        let v = h.alloc(Shape::indexable(Kind::ObjVector, 1, 2).annotated()).unwrap();
        set_slot_desc(v, 0, 1, PARENT);
        set_slot_value(v, 0, Oop::int(5));
        set_obj_anno(v, Oop::int(6));
        set_slot_anno(v, 0, Oop::int(7));
        set_element(v, 0, Oop::int(8));
        set_element(v, 1, Oop::int(9));
        assert_eq!(ilen(v), 2, "the annotations were counted as elements");
        assert_eq!(element(v, 0).as_int(), Some(8));
        assert_eq!(element(v, 1).as_int(), Some(9));
        assert_eq!(slot_value(v, 0).as_int(), Some(5));
        assert_eq!(obj_anno(v).as_int(), Some(6));
        assert_eq!(slot_anno(v, 0).as_int(), Some(7));
    }

    #[test]
    fn the_memoised_shape_is_valid_only_for_its_generation() {
        let h = heap();
        let o = obj(&h, 1);
        assert_eq!(shape_memo(o, 1), None, "a fresh object has a shape already");
        set_shape_memo(o, 1, 42);
        assert_eq!(shape_memo(o, 1), Some(42));
        assert_eq!(shape_memo(o, 2), None, "the memo outlived its generation");
        set_shape_memo(o, 2, 43);
        assert_eq!(shape_memo(o, 2), Some(43));
        // and it does not disturb anything else
        set_slot_desc(o, 0, 5, DATA);
        set_slot_value(o, 0, Oop::int(3));
        assert_eq!(slot_value(o, 0).as_int(), Some(3));
        assert_eq!(slot_name(o, 0), 5);
        assert_eq!(size_words(o), HEADER_WORDS + 2);
    }

    /// Anything holding Rust memory on a dead object's behalf has to hear about
    /// it, because abandoning a space is one store and runs no destructors.
    struct Undertaker {
        vars: Vec<Oop>,
        buried: Vec<usize>,
    }

    impl Roots for Undertaker {
        fn each(&mut self, f: &mut dyn FnMut(&mut Oop)) {
            for o in self.vars.iter_mut() {
                f(o);
            }
        }
        fn dying(&mut self, o: Oop) {
            self.buried.push(hash(o) as usize);
        }
    }

    #[test]
    fn the_collector_names_what_it_forgets() {
        let h = heap();
        let keep = obj(&h, 1);
        set_hash(keep, 1);
        let doomed = obj(&h, 1);
        set_hash(doomed, 2);
        let also = obj(&h, 1);
        set_hash(also, 3);

        let mut u = Undertaker { vars: vec![keep], buried: vec![] };
        h.scavenge(&mut u);
        u.buried.sort_unstable();
        assert_eq!(u.buried, vec![2, 3], "the wrong objects were reported dead");
        let _ = (doomed, also);

        // and again for the old generation, where a sweep does the forgetting
        let mut u = Undertaker { vars: vec![u.vars[0]], buried: vec![] };
        for _ in 0..PROMOTE_AGE {
            h.scavenge(&mut u);
        }
        assert!(u.buried.is_empty(), "a survivor was reported dead");
        u.vars.clear();
        h.collect(&mut u, true);
        assert_eq!(u.buried, vec![1], "the swept old object was not reported");
    }

    /// `_AddSlots:` cannot widen an object in place, so it builds a wider one
    /// and makes the world stop naming the old. Every reference has to move:
    /// roots, other young objects, and old ones -- which then have to be
    /// remembered, because the new object may be young.
    #[test]
    fn switching_pointers_finds_every_reference() {
        let h = heap();
        let holder = obj(&h, 1);
        let mut roots = Vars(vec![holder]);
        for _ in 0..PROMOTE_AGE {
            h.scavenge(&mut roots);
        }
        let old_holder = roots.0[0];
        assert!(!h.is_young(old_holder), "the holder was not tenured");

        let narrow = obj(&h, 1);
        set_slot_desc(narrow, 0, 1, DATA);
        set_slot_value(narrow, 0, Oop::int(5));
        h.store(old_holder, 0, narrow);
        let young_holder = obj(&h, 2);
        set_slot_desc(young_holder, 0, 1, DATA);
        set_slot_value(young_holder, 0, narrow);
        set_slot_desc(young_holder, 1, 2, DATA);
        set_slot_value(young_holder, 1, Oop::int(1));
        roots.0.push(young_holder);
        roots.0.push(narrow);

        // the wider object `_AddSlots:` would have built
        let wide = h.alloc(Shape::new(Kind::Slots, 2)).unwrap();
        set_slot_desc(wide, 0, 1, DATA);
        set_slot_value(wide, 0, Oop::int(5));
        set_slot_desc(wide, 1, 9, DATA);
        set_slot_value(wide, 1, Oop::int(6));

        h.switch_pointers(&mut roots, narrow, wide);

        assert_eq!(roots.0[2], wide, "a root still names the narrow object");
        assert_eq!(slot_value(young_holder, 0), wide, "a young holder was missed");
        assert_eq!(slot_value(old_holder, 0), wide, "an old holder was missed");
        assert_eq!(slot_value(young_holder, 1).as_int(), Some(1), "a bystander was switched");

        // and the switched-to object survives, because everything now names it
        h.scavenge(&mut roots);
        assert_eq!(slot_value(roots.0[1], 0), roots.0[2]);
        assert_eq!(slot_value(roots.0[2], 1).as_int(), Some(6));
        assert_eq!(slot_value(roots.0[0], 0), roots.0[2], "the old holder lost the new object");
    }

    // ------------------------------------------------------------- under load

    /// A fingerprint of everything reachable from the roots that does *not*
    /// depend on where anything is: kinds, shapes, descriptors, immediates and
    /// bytes, in a fixed traversal order. A collection moves objects and must
    /// change nothing else, so this number may not move either.
    fn fingerprint(roots: &[Oop]) -> u64 {
        let mut seen: Vec<usize> = vec![];
        let mut work: Vec<Oop> = roots.iter().rev().copied().collect();
        let mut acc: u64 = 0xcbf2_9ce4_8422_2325;
        let mix = |acc: &mut u64, v: u64| {
            *acc = (*acc ^ v).wrapping_mul(0x100_0000_01b3);
        };
        while let Some(o) = work.pop() {
            if let Some(i) = o.as_int() {
                mix(&mut acc, i as u64 ^ 0x1111);
                continue;
            }
            if !o.is_obj() {
                mix(&mut acc, 0x2222);
                continue;
            }
            if seen.contains(&o.addr()) {
                mix(&mut acc, 0x3333);
                continue;
            }
            seen.push(o.addr());
            mix(&mut acc, kind(o) as u64);
            mix(&mut acc, slots(o) as u64);
            mix(&mut acc, ilen(o) as u64);
            for i in 0..slots(o) {
                mix(&mut acc, slot_name(o, i) as u64);
                mix(&mut acc, slot_kind(o, i) as u64);
            }
            for i in 0..ilen(o) {
                if kind(o) == Kind::Bytes {
                    mix(&mut acc, byte_at(o, i) as u64);
                }
            }
            // children last, so the shape is fingerprinted before the graph
            for i in (0..oop_words(o)).rev() {
                work.push(field(o, i));
            }
        }
        acc
    }

    /// A graph of a few thousand objects of assorted shapes, collected
    /// repeatedly -- minors, majors, and a `switch_pointers` in the middle --
    /// with the fingerprint checked after every step. Deterministic: the same
    /// sequence every run, so a failure is reproducible.
    #[test]
    fn a_graph_survives_being_collected_over_and_over() {
        // Miri interprets every load and store, so the full graph would take
        // the best part of an hour there. A smaller one walks the same paths.
        let (rounds, batch) = if cfg!(miri) { (6usize, 12usize) } else { (40, 60) };
        let h = Heap::new(1 << 13, 1 << 14);
        let mut rng: u64 = 0x5eed;
        let mut next = move || {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (rng >> 33) as usize
        };

        let mut live: Vec<Oop> = vec![];
        for round in 0..rounds {
            // build a batch, wiring each new object to ones already there
            for _ in 0..batch {
                let pick = next() % 10;
                let o = if pick < 6 {
                    let n = next() % 5;
                    let o = h.alloc_or_tenure(Shape::new(Kind::Slots, n));
                    for i in 0..n {
                        set_slot_desc(o, i, (next() % 32) as u32, (next() % 2) as u8);
                        let v = if !live.is_empty() && next() % 2 == 0 {
                            live[next() % live.len()]
                        } else {
                            Oop::int((next() % 1000) as i64)
                        };
                        // through the barrier: `o` may already be old
                        h.store(o, i, v);
                    }
                    o
                } else if pick < 8 {
                    let n = next() % 40;
                    let o = h.alloc_or_tenure(Shape::indexable(Kind::Bytes, 1, n));
                    set_slot_desc(o, 0, 1, PARENT);
                    for i in 0..n {
                        set_byte_at(o, i, (next() % 251) as u8);
                    }
                    o
                } else {
                    let n = next() % 6;
                    let o = h.alloc_or_tenure(Shape::indexable(Kind::ObjVector, 1, n));
                    set_slot_desc(o, 0, 1, PARENT);
                    for i in 0..n {
                        let v = if !live.is_empty() {
                            live[next() % live.len()]
                        } else {
                            Oop::int(i as i64)
                        };
                        set_element(o, i, v);
                        if v.is_obj() {
                            h.record(o);
                        }
                    }
                    o
                };
                live.push(o);
            }

            // drop about half of the roots, so there is always garbage
            let keep = live.len() / 2;
            while live.len() > keep {
                live.swap_remove(next() % live.len());
            }

            let before = fingerprint(&live);
            let mut roots = Vars(std::mem::take(&mut live));
            if round % 7 == 6 {
                // widen one object the way `_AddSlots:` would
                if let Some(&victim) = roots.0.iter().find(|o| kind(**o) == Kind::Slots) {
                    let n = slots(victim);
                    let wide = h.alloc_or_tenure(Shape::new(Kind::Slots, n));
                    for i in 0..n {
                        set_slot_desc(wide, i, slot_name(victim, i), slot_kind(victim, i));
                        h.store(wide, i, slot_value(victim, i));
                    }
                    h.switch_pointers(&mut roots, victim, wide);
                }
            }
            h.collect(&mut roots, round % 5 == 4);
            live = roots.0;
            assert_eq!(
                fingerprint(&live),
                before,
                "round {round}: the graph changed across a collection"
            );
        }
        assert!(!live.is_empty(), "everything died");
        assert!(h.old_live() > 0, "nothing was ever tenured");
    }

    // ------------------------------------------------------- activation shape

    // What an activation will look like once `Scope` moves here: a chain link,
    // a marker standing in for the receiver, and its locals. All `Oop`s,
    // because an immediate is one -- a program counter is `Oop::int(pc)` and
    // the collector simply steps over it.
    const A_LEXICAL: usize = 0;
    const A_MARK: usize = 1;
    const A_LOCALS: usize = 2;

    fn activation(h: &Heap, lexical: Oop, marker: i64, locals: usize) -> Oop {
        let a = h.alloc_or_tenure(Shape::indexable(Kind::Activation, 0, A_LOCALS + locals));
        h.store(a, A_LEXICAL, lexical);
        set_field(a, A_MARK, Oop::int(marker));
        for i in 0..locals {
            set_field(a, A_LOCALS + i, Oop::int(marker * 1000 + i as i64));
        }
        a
    }

    /// Walk an activation's lexical chain, checking each link is the one that
    /// made it and its locals are intact.
    fn check_chain(top: Oop, depth: i64) {
        let mut cur = top;
        let mut d = depth;
        while cur.is_obj() {
            assert_eq!(field(cur, A_MARK).as_int(), Some(d), "the chain is out of order");
            for i in 0..oop_words(cur) - A_LOCALS {
                assert_eq!(
                    field(cur, A_LOCALS + i).as_int(),
                    Some(d * 1000 + i as i64),
                    "activation {d} lost local {i}"
                );
            }
            cur = field(cur, A_LEXICAL);
            d -= 1;
        }
        assert_eq!(d, -1, "the chain ended {} links early", d + 1);
    }

    /// The chain a block captures is as deep as the recursion that built it,
    /// so a scavenge follows it link by link. Cheney does that iteratively --
    /// the to-space is the queue -- which is what `walk_scope` in gc.rs needed
    /// an explicit stack and a memo table to manage.
    #[test]
    fn a_deep_activation_chain_survives_intact() {
        // shorter under Miri, which interprets every load; the chain is the
        // point, and sixty links exercise it the same way
        let deep: i64 = if cfg!(miri) { 60 } else { 600 };
        let h = Heap::new(1 << 14, 1 << 15);
        let mut top = Oop::null();
        for d in 0..deep {
            top = activation(&h, top, d, 3);
        }
        check_chain(top, deep - 1);

        let mut roots = Vars(vec![top]);
        for _ in 0..4 {
            h.scavenge(&mut roots);
            check_chain(roots.0[0], deep - 1);
        }
        h.collect(&mut roots, true);
        check_chain(roots.0[0], deep - 1);
        assert!(h.old_live() >= deep as usize, "a tenured chain lost links");
    }

    /// What a running interpreter actually does: push a frame, do a little
    /// work, pop it. Almost everything dies immediately and in reverse order,
    /// which is the case generation scavenging is built for -- and the case
    /// `test.self` makes 226,222 times.
    #[test]
    fn activations_churn_without_accumulating() {
        let (rounds, deep, ret) = if cfg!(miri) { (8usize, 10usize, 9) } else { (30, 50, 45) };
        let h = Heap::new(1 << 14, 1 << 15);
        let mut stack: Vec<Oop> = vec![];
        let mut captured: Vec<Oop> = vec![];
        let mut made = 0i64;

        for round in 0..rounds {
            // recurse a little, keeping the frame stack as the root set
            for _ in 0..deep {
                let lex = *stack.last().unwrap_or(&Oop::null());
                stack.push(activation(&h, lex, made, 4));
                made += 1;
            }
            // a block captures one frame in ten, and outlives it
            if round % 3 == 0 {
                if let Some(&a) = stack.get(stack.len() / 2) {
                    let blk = h.alloc_or_tenure(Shape::indexable(Kind::Block, 0, 1));
                    h.store(blk, 0, a);
                    captured.push(blk);
                }
            }
            // ...and then they return
            stack.truncate(stack.len().saturating_sub(ret));

            let mut roots = Vars(stack.iter().chain(captured.iter()).copied().collect());
            h.collect(&mut roots, round % 6 == 5);
            let n = stack.len();
            stack = roots.0[..n].to_vec();
            captured = roots.0[n..].to_vec();

            // every captured activation still holds the frame it closed over
            for blk in captured.iter() {
                let a = field(*blk, 0);
                assert!(a.is_obj(), "a block lost the activation it captured");
                let d = field(a, A_MARK).as_int().expect("the activation is not one");
                for i in 0..4 {
                    assert_eq!(
                        field(a, A_LOCALS + i).as_int(),
                        Some(d * 1000 + i as i64),
                        "a captured activation was mangled"
                    );
                }
            }
        }

        assert!(!captured.is_empty(), "no activation was ever captured");
        // everything made, against at most a few hundred ever live at once: the
        // rest must have been forgotten rather than tenured
        assert_eq!(made as usize, rounds * deep);
        // the frames near the bottom never return, so they do tenure -- which
        // is what says the bound above is a real one and not a vacuous pass
        assert!(h.old_live() > 0, "nothing survived long enough to be tenured");
        assert!(
            h.old_live() < 300,
            "activations accumulated in the old generation: {} of {made}",
            h.old_live()
        );
    }

    /// Promotion must not reserve to-space it then walks away from -- the bump
    /// happens inside `alloc`, so asking the to-space before checking the age
    /// leaks a whole object's worth of words per tenured object.
    #[test]
    fn tenuring_costs_the_to_space_nothing() {
        let h = heap();
        let mut roots = Vars(vec![]);
        for _ in 0..8 {
            let o = obj(&h, 1);
            set_field(o, 0, Oop::int(1));
            roots.0.push(o);
        }
        for _ in 0..PROMOTE_AGE {
            h.scavenge(&mut roots);
        }
        assert_eq!(h.old_live(), 8, "not everything was tenured");
        assert_eq!(h.young_used(), 0, "tenuring left words behind in the young space");
    }
}

#[cfg(test)]
mod bench {
    use super::*;
    use super::tests::Vars;

    /// Not a check -- a number. `cargo test --release -- --ignored --nocapture
    /// heap::bench`. The cell heap it replaces scavenges ~50k young objects in
    /// about 880us (`SERF_GC_STATS=1`); this says what the arena does with the
    /// same shape of work, and so whether porting the VM onto it is worth it.
    #[test]
    #[ignore]
    fn scavenge_throughput() {
        let h = Heap::new(1 << 20, 1 << 20);
        let mut roots = Vars(vec![]);
        let t0 = std::time::Instant::now();
        let mut allocated = 0u64;
        let mut scavenges = 0u64;
        let mut pause = std::time::Duration::ZERO;
        for _ in 0..40 {
            loop {
                match h.alloc(Shape::new(Kind::Slots, 3)) {
                    Some(o) => {
                        for i in 0..3 {
                            set_slot_desc(o, i, i as u32, 0);
                            set_slot_value(o, i, Oop::int(i as i64));
                        }
                        allocated += 1;
                        // keep about one in a hundred, as a real world does
                        if allocated % 100 == 0 {
                            roots.0.push(o);
                        }
                    }
                    None => break,
                }
            }
            let live = roots.0.len();
            let t = std::time::Instant::now();
            h.scavenge(&mut roots);
            pause += t.elapsed();
            scavenges += 1;
            let _ = live;
            roots.0.truncate(roots.0.len() / 2);
        }
        let total = t0.elapsed();
        eprintln!(
            "[bench] {} objects, {} scavenges, {:.1}ms total, {:.0}ns/object alloc, \
             {:.0}us mean pause, {} tenured",
            allocated,
            scavenges,
            total.as_secs_f64() * 1e3,
            (total - pause).as_nanos() as f64 / allocated as f64,
            pause.as_micros() as f64 / scavenges as f64,
            h.old_live(),
        );
    }
}
