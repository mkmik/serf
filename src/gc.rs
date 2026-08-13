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

use crate::value::{Obj, Payload, Slot};

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

fn env_usize(name: &str, dflt: usize) -> usize {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(dflt)
}

fn space(n: usize) -> &'static [Cel] {
    let v: Vec<Cel> = (0..n).map(|_| RefCell::new(None)).collect();
    Box::leak(v.into_boxed_slice())
}

pub struct Gc {
    table: RefCell<Vec<Entry>>,
    free_ids: RefCell<Vec<u32>>,
    /// the two semispaces; `from` says which one is being allocated into
    young: [&'static [Cel]; 2],
    from: Cell<u8>,
    bump: Cell<u32>,
    chunks: RefCell<Vec<&'static [Cel]>>,
    /// next free index in the last chunk
    old_bump: Cell<u32>,
    old_free: RefCell<Vec<Loc>>,
    old_live: Cell<usize>,
    /// old objects written since the last scavenge, so it need not scan them all
    remembered: RefCell<Vec<ObjRef>>,
    /// the young space filled up: collect at the next safepoint
    want: Cell<bool>,
    /// nesting depth of phases that keep roots in Rust locals we do not walk
    /// (image load and save, compilation). Nonzero means "do not collect".
    pub disabled: Cell<u32>,
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
        let n = env_usize("SERF_GC_YOUNG", 1 << 16);
        Gc {
            // handle 0 is never handed out: the VM's side tables key
            // immediates as 0, and an object must not collide with them
            table: RefCell::new(vec![Entry { loc: Loc::Free, age: 0, dirty: false, mark: false }]),
            free_ids: RefCell::new(vec![]),
            young: [space(n), space(n)],
            from: Cell::new(0),
            bump: Cell::new(0),
            chunks: RefCell::new(vec![]),
            old_bump: Cell::new(OLD_CHUNK),
            old_free: RefCell::new(vec![]),
            old_live: Cell::new(0),
            remembered: RefCell::new(vec![]),
            want: Cell::new(false),
            disabled: Cell::new(0),
            stress: std::env::var_os("SERF_GC_STRESS").is_some(),
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
        let i = self.bump.get();
        let loc = if (i as usize) < self.young[0].len() {
            self.bump.set(i + 1);
            let l = Loc::Young { space: self.from.get(), idx: i };
            if (i as usize) * 4 >= self.young[0].len() * 3 {
                self.want.set(true);
            }
            l
        } else {
            self.want.set(true);
            self.alloc_old()
        };
        *self.at(loc).borrow_mut() = Some(o);
        self.new_id(loc)
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
        !self.off && self.disabled.get() == 0 && (self.want.get() || self.stress)
    }

    pub fn young_used(&'static self) -> usize {
        self.bump.get() as usize
    }

    pub fn old_used(&'static self) -> usize {
        self.old_live.get()
    }

    /// Live handles, i.e. objects the heap is holding on to.
    pub fn count(&'static self) -> usize {
        self.table.borrow().iter().filter(|e| e.loc != Loc::Free).count()
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

pub fn alloc(slots: Vec<Slot>, payload: Payload) -> ObjRef {
    gc().alloc(Obj { slots, payload })
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
