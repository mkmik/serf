//! The object heap: a generational collector, after vm/src/any/memory/.
//!
//! Objects live in the heap, not in `Rc`s, and a `Value` names one by handle --
//! an index into a table that says where the object currently is. The young
//! generation is two semispaces that objects are copied between and eventually
//! out of; the old generation is chunks swept into a free list. Because a
//! handle never changes, moving an object is one table store: nothing else in
//! the VM, and nothing in a snapshot, has to be fixed up. That is the whole
//! trick, and it is why `_Define`'s `switch_pointers` (a full heap scan in the
//! C++ VM) is a single assignment here.
//!
//! The heap is leaked at first use and lives for the process, so every
//! reference into it is genuinely `'static` and `ObjRef::borrow()` can hand out
//! an ordinary `Ref<Obj>` with no lifetime to thread and no `unsafe`.

use std::cell::{Cell, Ref, RefCell, RefMut};
use std::rc::Rc;

use crate::value::{Method, Obj, Payload, Root, Scope, Slots, Value, Vm};

/// One object slot in a space. Empty means free (young: not yet bumped over,
/// or already evacuated; old: swept).
type Cel = RefCell<Option<Obj>>;

/// A handle. Stable for the object's whole life, so it is also its identity.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct ObjRef(pub u32);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Loc {
    Young { space: u8, idx: u32 },
    Old { chunk: u32, idx: u32 },
    /// swept; the id is on the free list unless we are keeping it poisoned
    Free,
}

#[derive(Clone, Copy)]
struct Entry {
    loc: Loc,
    /// scavenges survived; at `PROMOTE_AGE` the object is tenured
    age: u8,
    /// old object that may hold a young reference (the remembered set)
    dirty: bool,
    /// mark bit, used by the old generation's mark & sweep
    mark: bool,
}

/// Survive this many scavenges and you are tenured. The C++ VM recomputes a
/// threshold from an age histogram after every scavenge (ageTable.cpp:16); a
/// fixed age plus "promote if to-space is full" is most of the benefit.
/// ponytail: feedback tenuring if survivor space turns out to thrash.
const PROMOTE_AGE: u8 = 2;

const OLD_CHUNK: u32 = 1 << 15;

/// Do not bother with a major collection until the old generation is at least
/// this big; below it the mark phase costs more than the garbage is worth.
const MIN_OLD: usize = 1 << 16;

fn env_usize(name: &str, dflt: usize) -> usize {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(dflt)
}

fn space(n: usize) -> &'static [Cel] {
    let v: Vec<Cel> = (0..n).map(|_| RefCell::new(None)).collect();
    Box::leak(v.into_boxed_slice())
}

fn owners(n: usize) -> &'static [Cell<u32>] {
    let v: Vec<Cell<u32>> = (0..n).map(|_| Cell::new(0)).collect();
    Box::leak(v.into_boxed_slice())
}

pub struct Gc {
    table: RefCell<Vec<Entry>>,
    free_ids: RefCell<Vec<u32>>,
    /// the two semispaces; `from` says which one is being allocated into
    young: [&'static [Cel]; 2],
    /// which handle owns each young slot, so a scavenge can free the objects
    /// left behind in the from space without scanning the whole handle table
    owner: [&'static [Cell<u32>]; 2],
    from: Cell<u8>,
    bump: Cell<u32>,
    chunks: RefCell<Vec<&'static [Cel]>>,
    /// next free index in the last chunk
    old_bump: Cell<u32>,
    old_free: RefCell<Vec<Loc>>,
    old_live: Cell<usize>,
    /// live handles, kept as a running total so that reporting the heap size
    /// does not have to walk the whole table
    live: Cell<usize>,
    /// objects allocated since the VM started
    allocs: Cell<u64>,
    /// old generation size at which the next collection is a major one
    next_major: Cell<usize>,
    /// old objects written since the last scavenge, so it need not scan them all
    remembered: RefCell<Vec<ObjRef>>,
    /// the young space filled up: collect at the next safepoint
    want: Cell<bool>,
    /// ...and make that collection a major one (`_GarbageCollect` asked)
    want_major: Cell<bool>,
    /// nesting depth of phases that keep roots in Rust locals we do not walk
    /// (image load and save, compilation). Nonzero means "do not collect".
    pub disabled: Cell<u32>,
    /// collect at the first safepoint after *any* allocation, and never reuse
    /// a handle id, so a reference the collector failed to trace panics where
    /// it is used instead of quietly reattaching to a recycled object
    pub stress: bool,
    pub verify: bool,
    pub stats: bool,
    pub off: bool,
    pub minors: Cell<u64>,
    pub majors: Cell<u64>,
}

thread_local! {
    static HEAP: Cell<Option<&'static Gc>> = const { Cell::new(None) };
}

/// The heap, made on first use. One per thread, never replaced: a second `Vm`
/// shares it, so handles from the first stay meaningful (its objects simply
/// become unreachable, which is the collector's problem, not the caller's).
pub fn gc() -> &'static Gc {
    HEAP.with(|h| match h.get() {
        Some(g) => g,
        None => {
            let g: &'static Gc = Box::leak(Box::new(Gc::new()));
            h.set(Some(g));
            g
        }
    })
}

impl Gc {
    fn new() -> Gc {
        let stress = std::env::var_os("SERF_GC_STRESS").is_some();
        // stress collects after every allocation, so a big young space would
        // only make every scavenge sweep more empty cells
        let n = env_usize("SERF_GC_YOUNG", if stress { 512 } else { 1 << 16 });
        Gc {
            // handle 0 is never handed out: the VM's side tables key
            // immediates as 0, and an object must not collide with them
            table: RefCell::new(vec![Entry { loc: Loc::Free, age: 0, dirty: false, mark: false }]),
            free_ids: RefCell::new(vec![]),
            young: [space(n), space(n)],
            owner: [owners(n), owners(n)],
            from: Cell::new(0),
            bump: Cell::new(0),
            chunks: RefCell::new(vec![]),
            old_bump: Cell::new(OLD_CHUNK),
            old_free: RefCell::new(vec![]),
            old_live: Cell::new(0),
            live: Cell::new(0),
            allocs: Cell::new(0),
            next_major: Cell::new(MIN_OLD),
            remembered: RefCell::new(vec![]),
            want: Cell::new(false),
            want_major: Cell::new(false),
            disabled: Cell::new(0),
            stress,
            verify: std::env::var_os("SERF_GC_VERIFY").is_some(),
            stats: std::env::var_os("SERF_GC_STATS").is_some(),
            off: std::env::var("SERF_GC").map_or(false, |v| v == "off"),
            minors: Cell::new(0),
            majors: Cell::new(0),
        }
    }

    fn entry(&'static self, h: ObjRef) -> Entry {
        self.table.borrow()[h.0 as usize]
    }

    /// The slot an object sits in. `'static` because the spaces are leaked and
    /// never freed -- only their contents are.
    fn cel(&'static self, h: ObjRef) -> &'static Cel {
        match self.entry(h).loc {
            Loc::Free => panic!("use of collected object {}", h.0),
            loc => self.at(loc),
        }
    }

    /// Allocation never collects and never fails: it fills the young space,
    /// then tenures the overflow and asks for a collection at the next
    /// safepoint. The C++ VM keeps a whole young generation in reserve for the
    /// same reason -- a scavenge that cannot run out of room needs no unwind
    /// path (universe.cpp:87).
    fn alloc(&'static self, o: Obj) -> ObjRef {
        self.allocs.set(self.allocs.get() + 1);
        let i = self.bump.get();
        let loc = if (i as usize) < self.young[0].len() {
            self.bump.set(i + 1);
            let l = Loc::Young { space: self.from.get(), idx: i };
            // stress asks for a collection after every allocation, which is
            // dense enough to catch any missed root but skips the millions of
            // bytecodes that allocate nothing
            if self.stress || (i as usize) * 4 >= self.young[0].len() * 3 {
                self.want.set(true);
            }
            l
        } else {
            self.want.set(true);
            self.alloc_old()
        };
        *self.at(loc).borrow_mut() = Some(o);
        let h = self.new_id(loc);
        match loc {
            Loc::Young { space, idx } => self.owner[space as usize][idx as usize].set(h.0),
            // a pretenured object is born old with its slots already filled in,
            // and no write barrier has fired for it: if any of them names a
            // young object the next scavenge has to know
            _ => self.record(h),
        }
        h
    }

    /// A slot in the old generation: a swept one if there is one, else bump,
    /// adding a chunk when the last is full.
    fn alloc_old(&'static self) -> Loc {
        self.old_live.set(self.old_live.get() + 1);
        if let Some(l) = self.old_free.borrow_mut().pop() {
            return l;
        }
        let i = self.old_bump.get();
        if i >= OLD_CHUNK {
            self.chunks.borrow_mut().push(space(OLD_CHUNK as usize));
            self.old_bump.set(1);
            return Loc::Old { chunk: self.chunks.borrow().len() as u32 - 1, idx: 0 };
        }
        self.old_bump.set(i + 1);
        Loc::Old { chunk: self.chunks.borrow().len() as u32 - 1, idx: i }
    }

    fn at(&'static self, loc: Loc) -> &'static Cel {
        match loc {
            Loc::Young { space, idx } => &self.young[space as usize][idx as usize],
            Loc::Old { chunk, idx } => {
                // copy the (leaked, so `'static`) chunk out before the borrow ends
                let c = self.chunks.borrow()[chunk as usize];
                &c[idx as usize]
            }
            Loc::Free => unreachable!(),
        }
    }

    fn new_id(&'static self, loc: Loc) -> ObjRef {
        let e = Entry { loc, age: 0, dirty: false, mark: false };
        self.live.set(self.live.get() + 1);
        if let Some(id) = self.free_ids.borrow_mut().pop() {
            self.table.borrow_mut()[id as usize] = e;
            return ObjRef(id);
        }
        let mut t = self.table.borrow_mut();
        t.push(e);
        ObjRef(t.len() as u32 - 1)
    }

    /// Write barrier. Every mutation goes through `ObjRef::borrow_mut`, so an
    /// old object that might now point into the young generation cannot escape
    /// being recorded. Conservative -- it fires for writes that store no
    /// reference at all -- which is the same trade the C++ VM's unconditional
    /// card store makes (codeGen_i386.cpp:528).
    fn record(&'static self, h: ObjRef) {
        let fresh = {
            let mut t = self.table.borrow_mut();
            let e = &mut t[h.0 as usize];
            let fresh = matches!(e.loc, Loc::Old { .. }) && !e.dirty;
            e.dirty |= fresh;
            fresh
        };
        if fresh {
            self.remembered.borrow_mut().push(h);
        }
    }

    /// True when a collection is due. Checked at the interpreter's safepoint.
    pub fn wanted(&'static self) -> bool {
        !self.off && self.disabled.get() == 0 && self.want.get()
    }

    /// Ask for a collection at the next safepoint. `_GarbageCollect` and
    /// friends go through this rather than collecting on the spot: a primitive
    /// runs with the interpreter's Rust locals underneath it, which are not
    /// roots. A request made while collection is disabled stays pending.
    pub fn request(&'static self, major: bool) {
        self.want.set(true);
        if major {
            self.want_major.set(true);
        }
    }

    fn set_entry(&'static self, h: ObjRef, e: Entry) {
        self.table.borrow_mut()[h.0 as usize] = e;
    }

    /// Retire a handle. Its object is already gone; the id goes back on the
    /// free list unless we are keeping freed handles poisoned.
    fn free_id(&'static self, id: u32) {
        self.set_entry(ObjRef(id), Entry { loc: Loc::Free, age: 0, dirty: false, mark: false });
        self.live.set(self.live.get() - 1);
        // in stress mode ids are never reused, so a reference the collector
        // failed to trace lands on a Free entry and panics at the exact site
        // instead of silently attaching to whatever object took the id over
        if !self.stress {
            self.free_ids.borrow_mut().push(id);
        }
    }

    /// Set the mark bit; true if this is the first time, i.e. the object's
    /// contents still have to be walked.
    fn mark(&'static self, h: ObjRef) -> bool {
        let mut t = self.table.borrow_mut();
        let e = &mut t[h.0 as usize];
        if e.loc == Loc::Free {
            panic!("use of collected object {}", h.0);
        }
        if e.mark {
            return false;
        }
        e.mark = true;
        true
    }

    fn clear_marks(&'static self) {
        for e in self.table.borrow_mut().iter_mut() {
            e.mark = false;
        }
    }

    /// Every handle currently living in the old generation. Copied out, because
    /// the callers go on to move objects and free handles.
    fn old_handles(&'static self) -> Vec<ObjRef> {
        self.table
            .borrow()
            .iter()
            .enumerate()
            .filter(|(_, e)| matches!(e.loc, Loc::Old { .. }))
            .map(|(i, _)| ObjRef(i as u32))
            .collect()
    }

    /// For the weak side tables: did the object with this identity die? Handle
    /// 0 is the tables' key for immediates, which never die.
    fn is_dead(&'static self, id: usize) -> bool {
        id != 0 && self.table.borrow().get(id).map_or(false, |e| e.loc == Loc::Free)
    }

    pub fn young_used(&'static self) -> usize {
        self.bump.get() as usize
    }

    pub fn old_used(&'static self) -> usize {
        self.old_live.get()
    }

    /// Live handles, i.e. objects the heap is holding on to.
    pub fn count(&'static self) -> usize {
        self.live.get()
    }

    pub fn young_capacity(&'static self) -> usize {
        self.young[0].len()
    }
}

/// Guard for a phase that keeps object references in Rust locals the collector
/// does not walk: image load and save, and compilation, which re-enters the
/// interpreter with half-built literal vectors on the Rust stack.
pub struct NoGc;

impl NoGc {
    pub fn new() -> NoGc {
        gc().disabled.set(gc().disabled.get() + 1);
        NoGc
    }
}

impl Drop for NoGc {
    fn drop(&mut self) {
        gc().disabled.set(gc().disabled.get() - 1);
    }
}

pub fn alloc(slots: Slots, payload: Payload) -> ObjRef {
    gc().alloc(Obj::new(slots, payload))
}

impl ObjRef {
    pub fn borrow(self) -> Ref<'static, Obj> {
        Ref::map(gc().cel(self).borrow(), |o| o.as_ref().expect("use of collected object"))
    }

    pub fn borrow_mut(self) -> RefMut<'static, Obj> {
        let g = gc();
        g.record(self);
        RefMut::map(g.cel(self).borrow_mut(), |o| o.as_mut().expect("use of collected object"))
    }

    /// Identity key for the VM's side tables. Stable for the object's life --
    /// unlike the address it replaces, which a later object could reuse.
    pub fn id(self) -> usize {
        self.0 as usize
    }
}

// ------------------------------------------------------------------ tracing

/// The visited sets are keyed by address and are hit once per frame, so a deep
/// stack hashes millions of times per collection: SipHash is far too slow for
/// that and a multiply-rotate (rustc's own FxHash) is plenty for pointers.
#[derive(Default)]
struct PtrHash(u64);

impl std::hash::Hasher for PtrHash {
    fn write_usize(&mut self, n: usize) {
        self.0 = (self.0.rotate_left(5) ^ n as u64).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.write_usize(*b as usize);
        }
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

type PtrMap = std::collections::HashMap<usize, bool, std::hash::BuildHasherDefault<PtrHash>>;

/// Scopes and methods already walked in this collection, by address, each with
/// the answer the walk came to: does it reach a young object? Frames share
/// their lexical chains and a method is shared by every object holding it, so
/// walking one twice would be quadratic on a deep stack -- but the second
/// visitor still has to *learn* what the first one found, or it concludes the
/// object it is scanning has no young references and drops it from the
/// remembered set. Memoise the answer, not the visit.
/// Safe against address reuse: nothing is dropped while a traversal runs.
#[derive(Default)]
struct Seen {
    scopes: PtrMap,
    methods: PtrMap,
}

/// The two collectors walk the object graph the same way and differ only in
/// what they do with each `Value` they find. Every walk answers whether what it
/// walked still names a young object, which is what decides membership of the
/// remembered set.
trait Visit {
    fn seen(&mut self) -> &mut Seen;
    fn value(&mut self, v: Value) -> bool;
}

/// Every live `Rc<Method>` is held by a `Scope`, by a `Payload::Method` or
/// `Payload::Block`, so there is no such thing as a free-floating method root
/// and `Root` needs no variant for one.
fn walk_root<V: Visit>(v: &mut V, r: Root) {
    match r {
        Root::Val(x) => {
            v.value(x);
        }
        Root::Scope(s) => {
            walk_scope(v, &s);
        }
    }
}

fn walk_obj<V: Visit>(v: &mut V, o: &Obj) -> bool {
    let mut young = false;
    for s in o.slots.iter() {
        young |= v.value(s.value);
    }
    match &o.payload {
        Payload::Vector(xs) => {
            for x in xs.iter() {
                young |= v.value(*x);
            }
        }
        Payload::Mirror(x) => young |= v.value(*x),
        Payload::Method(m) => young |= walk_method(v, m),
        Payload::Block(m, sc) => {
            young |= walk_method(v, m);
            if let Some(sc) = sc {
                young |= walk_scope(v, sc);
            }
        }
        Payload::None | Payload::Bytes(_) | Payload::Proxy(_) => {}
    }
    young
}

fn walk_method<V: Visit>(v: &mut V, m: &Rc<Method>) -> bool {
    let key = Rc::as_ptr(m) as usize;
    if let Some(&young) = v.seen().methods.get(&key) {
        return young;
    }
    v.seen().methods.insert(key, false);
    let mut young = false;
    for x in m.lits.iter().chain(m.slot_inits.iter()) {
        young |= v.value(*x);
    }
    // the source string of a loaded method is an ordinary heap object, and it
    // is the one reference in here that is easy to miss
    if let Some((s, _, _)) = m.source {
        young |= v.value(s);
    }
    v.seen().methods.insert(key, young);
    young
}

/// Iterative on purpose: a block that captures the activation that created it
/// makes the lexical chain as deep as the recursion that built it, which would
/// blow the Rust stack. Borrows rather than clones the chain -- `s` holds it
/// all alive -- because this runs once per frame and refcount traffic on a
/// 300k-deep stack is not free.
fn walk_scope<V: Visit>(v: &mut V, s: &Rc<Scope>) -> bool {
    let key = Rc::as_ptr(s) as usize;
    if let Some(&young) = v.seen().scopes.get(&key) {
        return young;
    }
    let mut chain = vec![key];
    let mut todo: Vec<&Scope> = Vec::new();
    let mut cur: &Scope = s;
    let mut young = false;
    loop {
        v.seen().scopes.insert(cur as *const Scope as usize, false);
        young |= v.value(cur.recv);
        young |= v.value(cur.holder);
        for x in cur.slots.borrow().iter() {
            young |= v.value(*x);
        }
        young |= walk_method(v, &cur.method);
        for next in [&cur.lexical, &cur.home] {
            if let Some(n) = next {
                let k = Rc::as_ptr(n) as usize;
                match v.seen().scopes.get(&k) {
                    Some(&y) => young |= y,
                    None => {
                        chain.push(k);
                        todo.push(n);
                    }
                }
            }
        }
        match todo.pop() {
            Some(n) => cur = n,
            None => break,
        }
    }
    // one answer for the whole chain: an outer scope reaching young makes the
    // inner ones reach it too, and the reverse only costs a redundant scan.
    // The object holding a scope is pinned by `holds_scope` regardless.
    for k in chain {
        v.seen().scopes.insert(k, young);
    }
    young
}

/// An old object holding this cannot be dropped from the remembered set: a
/// `Scope`'s slots are mutated through a plain `RefCell` (`interp.rs`'s
/// assignment bytecode), with no barrier to notice a young value being stored
/// into a scope that only an old block still reaches.
fn holds_scope(o: &Obj) -> bool {
    matches!(o.payload, Payload::Block(_, Some(_)))
}

// --------------------------------------------------------------- scavenging

/// A minor collection: copy the young survivors into the to-space (or the old
/// generation), then drop everything left behind.
///
/// Cheney's algorithm with one difference: an object moves by writing a single
/// handle-table entry, so *no reference anywhere is ever rewritten*. Scanning
/// exists only to find young referents, never to fix pointers up.
struct Scav {
    g: &'static Gc,
    seen: Seen,
    from: u8,
    to: u8,
    to_bump: u32,
    /// evacuated but not yet scanned
    queue: Vec<ObjRef>,
    promoted: usize,
    /// verify mode: report young referents an old object should already have
    /// been remembered for
    checking: bool,
    violations: usize,
}

impl Visit for Scav {
    fn seen(&mut self) -> &mut Seen {
        &mut self.seen
    }

    /// One handle-table lookup per reference, which on a deep stack happens
    /// millions of times per collection. Answers whether the object is young
    /// once it has been dealt with -- a promoted one is not.
    fn value(&mut self, v: Value) -> bool {
        let h = match v {
            Value::Obj(h) => h,
            _ => return false,
        };
        let e = self.g.entry(h);
        match e.loc {
            // still in the from space: this is the reference that moves it
            Loc::Young { space, idx } if space == self.from => {
                if self.checking {
                    self.violations += 1;
                }
                matches!(self.evacuate(h, e, idx as usize), Loc::Young { .. })
            }
            // already copied into the to space this collection
            Loc::Young { .. } => true,
            Loc::Old { .. } => false,
            // a live reference to a freed handle: only possible if a root was
            // missed, and in stress mode the id was never reused, so say so
            Loc::Free => panic!("use of collected object {}", h.0),
        }
    }
}

impl Scav {
    /// Move one object out of the from space; answers where it went.
    fn evacuate(&mut self, h: ObjRef, e: Entry, idx: usize) -> Loc {
        let g = self.g;
        let obj = g.young[self.from as usize][idx]
            .borrow_mut()
            .take()
            .expect("young object missing from its slot");
        let age = e.age.saturating_add(1);
        // promotion cannot fail: the old generation grows a chunk on demand,
        // which is what lets the to-space overflow tenure instead of abort
        let loc = if age >= PROMOTE_AGE || self.to_bump as usize >= g.young[0].len() {
            self.promoted += 1;
            g.alloc_old()
        } else {
            let i = self.to_bump;
            self.to_bump += 1;
            g.owner[self.to as usize][i as usize].set(h.0);
            Loc::Young { space: self.to, idx: i }
        };
        *g.at(loc).borrow_mut() = Some(obj);
        // dirty is dropped: a promoted object earns its place in the remembered
        // set back in `scan`, once we know whether it still points at young
        g.set_entry(h, Entry { loc, age, dirty: false, mark: e.mark });
        self.queue.push(h);
        loc
    }

    /// Walk one object's references, evacuating the young ones. Answers whether
    /// it must be on the remembered set -- which is the self-cleaning card of
    /// rSet.cpp:131, decided per object rather than per 32-word card.
    fn scan(&mut self, h: ObjRef) {
        let loc = self.g.entry(h).loc;
        if loc == Loc::Free {
            return;
        }
        let cel = self.g.at(loc);
        let r = cel.borrow();
        let o = match r.as_ref() {
            Some(o) => o,
            None => return,
        };
        let keep = walk_obj(self, o) || holds_scope(o);
        drop(r);
        if keep {
            // no-op unless the object is old, which is exactly right
            self.g.record(h);
        }
    }

    fn drain(&mut self) {
        while let Some(h) = self.queue.pop() {
            self.scan(h);
        }
    }

    /// Verify mode: after the remembered set has done its job, no old object
    /// may still name a from-space object. One that does was written without
    /// the barrier firing.
    fn verify_old(&mut self) {
        self.checking = true;
        for h in self.g.old_handles() {
            // no memo across objects here: a shared method answered from the
            // cache would hide the very thing this is looking for
            self.seen = Seen::default();
            let before = self.violations;
            self.scan(h);
            if self.violations > before {
                eprintln!(
                    "[gc] VERIFY FAILED: old object {} holds {} young reference(s) \
                     the remembered set did not know about -- the write barrier is broken",
                    h.0,
                    self.violations - before
                );
            }
        }
        self.checking = false;
        // the barrier may be broken, but the heap must still come out
        // consistent: whatever the check found has been evacuated, so finish
        // copying it
        self.seen = Seen::default();
        self.drain();
        if self.violations > 0 {
            // a check nobody notices is not a check: this is a debugging mode,
            // so make the run fail rather than print into the void
            eprintln!("[gc] {} unremembered young reference(s); exiting", self.violations);
            std::process::exit(1);
        }
    }
}

fn scavenge(vm: &mut Vm, g: &'static Gc) -> usize {
    let from = g.from.get();
    let mut s = Scav {
        g,
        seen: Seen::default(),
        from,
        to: 1 - from,
        to_bump: 0,
        queue: vec![],
        promoted: 0,
        checking: false,
        violations: 0,
    };

    // the remembered set is rebuilt from scratch as objects are scanned:
    // `Gc::record` refills it, so clearing the dirty bits here keeps bit and
    // membership in step
    let rs = std::mem::take(&mut *g.remembered.borrow_mut());
    {
        let mut t = g.table.borrow_mut();
        for h in rs.iter() {
            t[h.0 as usize].dirty = false;
        }
    }

    vm.each_root(false, &mut |r| walk_root(&mut s, r));
    for h in rs {
        if matches!(g.entry(h).loc, Loc::Old { .. }) {
            s.scan(h);
        }
    }
    s.drain();
    if g.verify {
        s.verify_old();
    }

    // the annotation barrier list is rebuilt the way the remembered set is:
    // whatever this scavenge promoted is old now, and the next major is what
    // traces it. A value whose table entry has since gone stays until it is
    // promoted -- floating garbage, bounded by `PROMOTE_AGE` scavenges.
    vm.anno_young.retain(|x| match x {
        Value::Obj(h) => matches!(g.entry(*h).loc, Loc::Young { .. }),
        _ => false,
    });

    // whatever is still sitting in the from space was never reached: dropping
    // it is the free, and it costs one pass over the space we are done with
    let cap = g.young[0].len();
    let bump = (g.bump.get() as usize).min(cap);
    for i in 0..bump {
        let mut dead = g.young[from as usize][i].borrow_mut().take();
        if dead.is_some() {
            g.free_id(g.owner[from as usize][i].get());
        }
        // A block holds the activation it closed over, so this loop is where
        // most activations actually die -- a frame that returned while a block
        // of its was still live could not reclaim it. Offer it now; the pool
        // declines it if anything else still names it.
        if let Some(Payload::Block(_, s)) = dead.as_mut().map(|o| &mut o.payload) {
            if let Some(s) = s.take() {
                crate::value::give_scope(s);
            }
        }
        drop(dead);
    }

    g.bump.set(s.to_bump);
    g.from.set(s.to);
    s.promoted
}

// -------------------------------------------------------------- mark & sweep

struct Mark {
    g: &'static Gc,
    seen: Seen,
    queue: Vec<ObjRef>,
}

impl Visit for Mark {
    fn seen(&mut self) -> &mut Seen {
        &mut self.seen
    }

    /// Marking computes a full closure and is idempotent, so it has no use for
    /// the young/old answer the scavenger needs.
    fn value(&mut self, v: Value) -> bool {
        if let Value::Obj(h) = v {
            if self.g.mark(h) {
                self.queue.push(h);
            }
        }
        false
    }
}

/// Mark the whole reachable graph, young and old: an old object is often only
/// reachable through a young one.
fn mark_all(vm: &Vm, g: &'static Gc) {
    let mut m = Mark { g, seen: Seen::default(), queue: vec![] };
    vm.each_root(true, &mut |r| walk_root(&mut m, r));
    while let Some(h) = m.queue.pop() {
        let cel = g.cel(h);
        let r = cel.borrow();
        if let Some(o) = r.as_ref() {
            walk_obj(&mut m, o);
        }
    }
}

/// Drop every unmarked old object and put its slot on the free list. Runs
/// before the scavenge, so that a dead old object cannot drag its young
/// referents through one more copy.
fn sweep_old(g: &'static Gc) {
    for h in g.old_handles() {
        let e = g.entry(h);
        if e.mark {
            continue;
        }
        let dead = g.at(e.loc).borrow_mut().take();
        drop(dead);
        g.old_free.borrow_mut().push(e.loc);
        g.old_live.set(g.old_live.get() - 1);
        g.free_id(h.0);
    }
}

// ------------------------------------------------------------------- driver

/// Collect. `major` runs a mark & sweep of the old generation as well.
///
/// Only ever called from the interpreter's safepoint (or from a primitive's
/// request, which the safepoint picks up): everything the VM can still reach
/// has to be in `Vm::each_root` by then, and `disabled` covers the phases whose
/// roots live in Rust locals instead.
pub fn collect(vm: &mut Vm, major: bool) {
    let g = gc();
    if g.off || g.disabled.get() > 0 {
        return;
    }
    let t0 = std::time::Instant::now();
    let before = g.count();

    if major {
        mark_all(vm, g);
        sweep_old(g);
    }
    let promoted = scavenge(vm, g);
    if major {
        g.clear_marks();
        g.next_major.set(std::cmp::max(MIN_OLD, g.old_live.get() * 2));
        g.majors.set(g.majors.get() + 1);
    } else {
        g.minors.set(g.minors.get() + 1);
    }

    // side tables keyed by identity: their entries are traced as roots above,
    // so this drops only the ones whose object turned out to be dead. Handle
    // ids are recycled, so a survivor would later reattach to a stranger.
    vm.sweep_weak(&|id| g.is_dead(id));
    // the same recycling would let a memoised lookup reattach a stale hit
    crate::value::lookup_gen_bump();

    g.want.set(false);
    g.want_major.set(false);

    // the pause is the whole collection: one thread, stopped at a safepoint
    crate::metrics::record(crate::metrics::Collection {
        major,
        pause: t0.elapsed(),
        freed: (before - g.count()) as u64,
        promoted: promoted as u64,
        allocated: g.allocs.get(),
        young: g.young_used() as u64,
        young_capacity: g.young_capacity() as u64,
        old: g.old_used() as u64,
        remembered: g.remembered.borrow().len() as u64,
    });

    if g.stats {
        eprintln!(
            "[gc] {} {}us objs {}->{} promoted {} young {}/{} old {} remembered {}",
            if major { "major" } else { "minor" },
            t0.elapsed().as_micros(),
            before,
            g.count(),
            promoted,
            g.young_used(),
            g.young[0].len(),
            g.old_used(),
            g.remembered.borrow().len(),
        );
    }
}

/// The safepoint's entry point: collect if anything asked for it, majoring when
/// the old generation has outgrown the threshold set by the last major.
pub fn collect_if_wanted(vm: &mut Vm) {
    let g = gc();
    if !g.wanted() {
        return;
    }
    collect(vm, g.want_major.get() || g.old_live.get() >= g.next_major.get());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::{slot, SlotKind};

    // one heap per thread, and the test harness gives each test its own
    fn loc_of(h: ObjRef) -> Loc {
        gc().entry(h).loc
    }

    fn root(vm: &Vm, name: &str, v: Value) {
        vm.globals.as_obj().unwrap().borrow_mut().put(slot(name, SlotKind::Data, v));
    }

    fn method() -> Rc<Method> {
        Rc::new(Method {
            sel: "t".into(),
            nargs: 0,
            arg_slots: vec![],
            slot_names: vec![],
            slot_flags: vec![],
            slot_inits: vec![],
            code: vec![],
            lits: vec![],
            lit_strs: vec![],
            is_block: false,
            file: "t".into(),
            line: 0,
            source: None,
            sites: Default::default(),
        })
    }

    #[test]
    fn scavenge_keeps_the_reachable_and_frees_the_rest() {
        // these collect, and a collection records into process-wide totals
        let _totals = crate::metrics::TOTALS.lock().unwrap_or_else(|e| e.into_inner());
        let mut vm = Vm::new();
        let keep = Value::obj([], Payload::None);
        let kh = keep.as_obj().unwrap();
        root(&vm, "keep", keep);
        let lost = Value::obj([], Payload::None).as_obj().unwrap();
        let before = gc().count();
        collect(&mut vm, false);
        assert!(gc().count() < before, "nothing was collected");
        assert_eq!(loc_of(lost), Loc::Free, "an unreachable object survived");
        assert!(matches!(loc_of(kh), Loc::Young { .. }), "a survivor was tenured too early");
        assert!(kh.borrow().slots.is_empty(), "the survivor is not usable");
    }

    #[test]
    fn tenuring_and_the_remembered_set() {
        // these collect, and a collection records into process-wide totals
        let _totals = crate::metrics::TOTALS.lock().unwrap_or_else(|e| e.into_inner());
        let mut vm = Vm::new();
        let holder = Value::obj([], Payload::None);
        let hh = holder.as_obj().unwrap();
        root(&vm, "holder", holder);
        for _ in 0..PROMOTE_AGE + 1 {
            collect(&mut vm, false);
        }
        assert!(matches!(loc_of(hh), Loc::Old { .. }), "surviving objects are not tenured");

        // a young object reachable only through a tenured one: the scavenge
        // does not scan the old generation, so only the write barrier can save it
        let young = Value::obj([], Payload::None);
        let yh = young.as_obj().unwrap();
        hh.borrow_mut().put(slot("y", SlotKind::Data, young));
        collect(&mut vm, false);
        assert_ne!(loc_of(yh), Loc::Free, "the remembered set lost a young referent");
        assert!(hh.borrow().slots[0].value.id_eq(&Value::Obj(yh)), "the reference moved");
    }

    #[test]
    fn only_a_major_frees_the_old_generation() {
        // these collect, and a collection records into process-wide totals
        let _totals = crate::metrics::TOTALS.lock().unwrap_or_else(|e| e.into_inner());
        let mut vm = Vm::new();
        let doomed = Value::obj([], Payload::None);
        let dh = doomed.as_obj().unwrap();
        root(&vm, "doomed", doomed);
        for _ in 0..PROMOTE_AGE + 1 {
            collect(&mut vm, false);
        }
        assert!(matches!(loc_of(dh), Loc::Old { .. }));
        let old = gc().old_used();

        {
            let mut g = vm.globals.as_obj().unwrap().borrow_mut();
            g.forget_map();
            g.slots.retain(|s| crate::value::sym_str(s.name) != "doomed");
        }
        collect(&mut vm, false);
        assert_ne!(loc_of(dh), Loc::Free, "a minor collection swept the old generation");
        collect(&mut vm, true);
        assert_eq!(loc_of(dh), Loc::Free, "a major collection did not sweep it");
        assert!(gc().old_used() < old, "the old generation did not shrink");
    }

    /// The one edge with no write barrier behind it: a `Scope`'s slots are
    /// mutated through a plain `RefCell`, so an old object holding a captured
    /// scope has to stay remembered for good.
    #[test]
    fn a_captured_scope_is_traced_after_tenuring() {
        // these collect, and a collection records into process-wide totals
        let _totals = crate::metrics::TOTALS.lock().unwrap_or_else(|e| e.into_inner());
        let mut vm = Vm::new();
        let scope = Rc::new(Scope {
            method: method(),
            recv: Value::Int(0),
            holder: Value::Int(0),
            slots: RefCell::new(vec![]),
            lexical: None,
            home: None,
            dead: Cell::new(false),
        });
        let blk = Value::obj([], Payload::Block(method(), Some(scope.clone())));
        let bh = blk.as_obj().unwrap();
        root(&vm, "blk", blk);
        for _ in 0..PROMOTE_AGE + 1 {
            collect(&mut vm, false);
        }
        assert!(matches!(loc_of(bh), Loc::Old { .. }));

        let inner = Value::obj([], Payload::None);
        let ih = inner.as_obj().unwrap();
        scope.slots.borrow_mut().push(inner);
        collect(&mut vm, false);
        assert_ne!(loc_of(ih), Loc::Free, "a value stored into a captured scope was lost");
    }

    #[test]
    fn weak_tables_lose_their_dead_keys() {
        // these collect, and a collection records into process-wide totals
        let _totals = crate::metrics::TOTALS.lock().unwrap_or_else(|e| e.into_inner());
        let mut vm = Vm::new();
        let live = Value::obj([], Payload::None);
        let lh = live.as_obj().unwrap();
        root(&vm, "live", live);
        let dead = Value::obj([], Payload::None).as_obj().unwrap();
        vm.id_hash.insert(lh.id(), 1);
        vm.id_hash.insert(dead.id(), 2);
        collect(&mut vm, false);
        assert_eq!(vm.id_hash.get(&lh.id()), Some(&1), "a live object lost its identity hash");
        assert!(!vm.id_hash.contains_key(&dead.id()), "a dead object kept its identity hash");
    }

fn method_with(lits: Vec<Value>) -> Rc<Method> {
    Rc::new(Method {
        sel: "t".into(),
        nargs: 0,
        arg_slots: vec![],
        slot_names: vec![],
        slot_flags: vec![],
        slot_inits: vec![],
        code: vec![],
        lits,
        lit_strs: vec![],
        is_block: false,
        file: "t".into(),
        line: 0,
        source: None,
        sites: Default::default(),
    })
}

#[test]
fn every_holder_of_a_shared_method_stays_remembered() {
    let mut vm = Vm::new();
    // young literal, reachable only through the method
    let y = Value::obj([], Payload::None);
    let yh = y.as_obj().unwrap();
    let m = method_with(vec![y]);

    // fill the young space so the next allocations are pretenured
    let cap = gc().young[0].len();
    while gc().young_used() < cap {
        let _ = Value::obj([], Payload::None);
    }

    let o1 = Value::obj([], Payload::Method(m.clone()));
    let o2 = Value::obj([], Payload::Method(m.clone()));
    assert!(matches!(loc_of(o1.as_obj().unwrap()), Loc::Old { .. }), "o1 was not pretenured");
    assert!(matches!(loc_of(o2.as_obj().unwrap()), Loc::Old { .. }), "o2 was not pretenured");
    root(&vm, "o1", o1);
    root(&vm, "o2", o2);

    collect(&mut vm, false);
    assert_ne!(loc_of(yh), Loc::Free, "the literal died in the first scavenge");
    // only o1 is left remembered; o2's scan was memoised away
    let remembered: Vec<u32> = gc().remembered.borrow().iter().map(|h| h.0).collect();
    eprintln!("remembered after scan: {:?} (o1={} o2={})",
              remembered, o1.as_obj().unwrap().0, o2.as_obj().unwrap().0);

    // o1 dies; o2 is now the only path to the method and its literal
    {
        let mut g = vm.globals.as_obj().unwrap().borrow_mut();
        g.forget_map();
        g.slots.retain(|s| crate::value::sym_str(s.name) != "o1");
    }
    collect(&mut vm, true);
    assert_ne!(loc_of(yh), Loc::Free, "a live literal of a shared method was collected");
}
}
