# The memory subsystem

A design for serf's second heap: direct tagged pointers, one arena, every
Self-universe entity in it, and `malloc` reached for only to grow an arena.

Today serf has *two* memory systems and only one of them is a collector.

* `gc.rs` manages **identity and lifetime**: a handle table, and two arrays of
  fixed-size cells that an `Obj` struct is dropped into.
* Rust's allocator manages **everything an object contains**: slot vectors past
  the fourth, byte payloads, object vectors, methods (`Rc<Method>` plus ten
  inner `Vec`s), activations (`Rc<Scope>` plus two `Vec<Value>`), and the six
  side tables that hold annotations, identity hashes, object kinds and process
  stacks.

The collector collects cells. The bytes go through `malloc` and `free`, one
call per string, per vector, per spilled slot list, per activation, per operand
stack. That is the thing to fix.

## What it costs, measured

A counting `GlobalAlloc` under `self/test.self` (46 checks) and under
`--load core.snap`:

| | mallocs | reallocs | live at exit |
|---|---|---|---|
| `self/test.self` | 1,856,289 | 989 | ~12k |
| `--load core.snap -e '3 + 4'` | 1,411,664 | 115,655 | ~427k |

Broken down by size, `test.self` is two allocations wearing many hats:

```
64 B × 1,563,574     Vec<Value> at capacity 4 -- a frame's operand stack,
                     a send's argument vector, an activation's locals
112 B ×  226,222     Rc<Scope> -- one per activation the pool missed
16 B/32 B × ~48,000  byte payloads and spilled slot vectors
```

The pools in `value.rs` (`VALS`, `SCOPES`) exist to dodge exactly these and
still miss 1.8M times, because they are patches on a symptom: `Frame::new`
takes a fresh `vec![]` while `give_vals` is filling a pool that only
`new_scope` drains. Repointing `Frame::new` at the pool removes 653k mallocs
and leaves 1.2M. There is no arrangement of pools that gets to zero, because
the objects being pooled are not the objects being collected.

Loading a real world is worse in a different way: 1.4M mallocs and 116k
reallocs to build it, and **~427k separate allocator blocks still live**
afterwards — one per string, per vector, per spilled slot list, per method's
ten vectors, per hash-map bucket array. At roughly 16 bytes of allocator
bookkeeping each that is ~7 MB of pure overhead, scattered across the address
space in the order the image reader happened to visit.

And the cells themselves:

```
sizeof Value=16  Slot=24  Slots=104  Payload=32  Obj=136  Cel=144  Scope=96
```

A cell is 144 bytes because `Slots::Inline` reserves four 24-byte slots
whether or not the object has any. Two semispaces of 65,536 cells is
**18.9 MB resident before a single object exists**, and it does not adapt: an
object with five slots spills to `malloc` anyway, and a 46-byte string carries
a 104-byte inline slot array it will never use. `core.snap`'s world averages
**3.8 slots per object** and **46 bytes per byte object** — both sides of that
average lose.

Everything below follows from one observation: a cell is the wrong unit. The
right unit is a word, and the right free operation is subtracting from a bump
pointer.

## The model: direct tagged pointers

A `Value` is the machine word. No handle table, no indirection, no array
lookup between a reference and the object it names — an `Oop` that names an
object *is* that object's address.

```
w & 1 == 0   →  object pointer, 8-aligned, dereferenced with no masking at all
w & 1 == 1   →  smallint, value = (w as i64) >> 1        (63-bit)
```

The C++ VM spends two tag bits — `Int_Tag = 0`, `Mem_Tag = 1`, `Float_Tag = 2`,
`Mark_Tag = 3` (`objects/tag.hh:13`) — because on 32-bit hardware it needed
four tags and could not spare an untagged pointer. serf is 64-bit and can do
better: give **pointers the zero tag**, so a deref is a plain load with no mask
and no shift, which is the instruction a JIT most wants to emit. The integer
tag in bit 0 keeps `_IntAdd:` at an add and a decrement.

Objects are 8-aligned, so bits 1 and 2 are spare for later (immediate
characters, a second immediate type) without disturbing either fast path.

### Floats

Boxed — a float becomes a two-word heap object, as Self's own `floatMap` does.
The alternative is NaN-boxing, which keeps `f64` inline in the word by hiding
the other types in a quiet NaN's payload, and it costs three things here:
integer range drops to ~48 bits (serf has no bignums — `_IntAdd:` is
`checked_add` and overflow is an error, so range is not free to give away); a
deref becomes a mask; and the pointer has to fit in 48 bits, which is an
assumption about the address space rather than about the language.

What the worlds say: `core.snap` holds **155** floats in its entire reachable
graph and `morphic.snap` **3,467**, against ~138k objects. Floats are rare in
Self, whose own floats are 30-bit. A boxed float that dies before the next
scavenge costs a bump pointer increment and is never looked at again.

Recommendation: box them. This is not a one-way door — the encoding is one
module.

## Object layout

One object is a contiguous run of words, self-describing so that any arena can
be walked linearly.

```
word 0   mark:  forwarded:1 │ identity hash:23 │ age:8 │ flags:8 │ tag:8 │ pad:8
word 1   map pointer  ──▶ shape: slot descriptors, constant slot values, kind,
                          annotations, size — shared by every object like this
word 2   assignable slot 0 value      Oop
  ⋮      … one word per assignable slot …
         [kind indexable] length in elements, then the bytes or the Oops
```

This is the C++ VM's layout: mark word plus map pointer. Self's mark word is
`tag:2 │ hash:22 │ age:7 │ marked:1` (`objects/markOop.hh:15`); serf's is
widened to 64 bits and gains the scavenge's `forwarded` flag.

* `hash` is the identity hash Self keeps in the mark word, and its being here
  is what deletes `Vm::id_hash`. An address cannot serve, because the object
  moves.
* Everything else that was going to need a home in the object — slot names and
  kinds, the `kind` byte that replaces `Payload`, the annotations — is in the
  map. That deletes `Vm::obj_kind`, `Vm::anno_obj`, `Vm::anno_slot`,
  `Vm::anno_young`, `note_anno` and the annotation write barrier, and it leaves
  an object holding only the words that actually differ between clones.

### Maps

An earlier draft of this design skipped maps, on the grounds that sharing slot
descriptors between clones saves 265,153 × 8 ≈ 2 MB on `core.snap` and costs
map canonicalisation, map transitions on `_AddSlots:` and dependency lists —
the largest subsystem in `objects/`. That costed the wrong axis. **A map is
the right key for a method cache, and object identity is the wrong one.**

Self keys its lookups on the receiver's map, not the receiver:
`MethodLookupKey` "adds the receiver map to that info, and is specific to a
given receiver map" (`lookup/key.hh:49`). serf keys on the receiver object —
`m.site_hit(s, vm.lookup_key(&cur_recv))` in `interp.rs:604`, where
`lookup_key` answers the receiver's own `ObjRef`. So a loop over a thousand
clones of one prototype misses a monomorphic inline cache a thousand times,
and fills the `lookup_cache` with a thousand entries that say the same thing.
Only immediates cache well today, because all integers share their traits
object as a key.

Interning a map per distinct shape is something serf's image writer already
does (`image_obj.rs:892`), so the compression ratio is measurable rather than
hypothetical:

| | objects | maps | maps with one object | objects sharing a map |
|---|---|---|---|---|
| `core.snap` | 111,844 | 26,584 | 26,503 | 85,341 across **81 maps** |
| `morphic.snap` | 219,746 | 53,495 | 53,312 | 166,434 across **183 maps** |

The singleton maps are the methods and blocks — 15,595 and 8,583 in
`core.snap` — which each hold their own code and are never a polymorphic
receiver. The other **76% of objects collapse onto 81 shapes**. That is the
cache key ratio: about **1,000:1**, against the 4:1 the raw map count suggests
and the 1:1 serf gets now. Largest families in `core.snap`: 19,888 / 17,354 /
11,355 / 5,925 / 4,662.

For the map to be a *sound* lookup key it must hold constant slot **values**,
not just names and kinds — two objects of the same shape whose constant parent
slots point at different parents must not share a cache entry. That is Self's
design (a slot descriptor is name, type, data, annotation) and it is what the
table above already measures, since the interned body includes those words. An
assignable parent slot is the remaining hole, and Self plugs it with an
explicit `assignableDependencyList` (`lookup/simpleLookup.hh:48`).

The cost is `_Define` and `_AddSlots:`, which become map transitions plus a
`switch_pointers` scan of the heap. **Direct pointers already require that
scan**, so accepting them has largely pre-paid the biggest bill maps come with.
The two decisions reinforce each other, which is why maps belong in phase 2
rather than in a phase that never happens.

*ponytail: canonicalise maps in a hash table keyed on the body, as
`image_obj.rs` already does. Dependency lists are Self's mechanism for
invalidating caches precisely; serf's `LOOKUP_GEN` whole-cache flush is the
lazy version and stays until a mutation-heavy workload thrashes it.*

### Sizing check

`core.snap`'s reachable world, laid out this way:

```
69,954 objects × (2 + ~1.3 assignable) words   1.8 MB
26,584 maps, carrying the shared descriptors   4.3 MB
694,669 bytes of string data                   0.7 MB
76,093 vector elements                         0.6 MB
161,177 bytecodes + 54,846 literals            0.6 MB
                                             ────────
                                               ~8 MB, in one arena, no malloc blocks
```

against today's 18.9 MB of mostly-empty young space plus ~427k allocator
blocks. Steady-state interpreter allocation goes from 1.86M mallocs per test
run to **zero**: a send bumps a pointer.

## Methods

An `Rc<Method>` is eleven allocations: the `Rc` and ten `Vec`s. 15,774 methods
in `core.snap` is ~173k of them. As heap objects a method is three:

```
Method object    kind=Method
  slots:         selector, file, line, nargs, source, sites index
  indexable:     literal Oops
  ├─ Bytes       the bytecodes
  └─ ObjVector   slot names, flags and initialisers
```

`walk_method`, the `Seen.methods` memo table and the `PtrHash` hasher that
exists to make it fast all go away: a method is traced like any other object.

## Activations

This is the change that pays for the rest, and it is what Self does —
`ovframeMap`, `bvframeMap`, `outerActivationObj`, `blockActivationObj` are
already in the VM-root and map-type tables `image.rs` reads.

```
Activation       kind=Activation
  slots:         method, receiver, holder, lexical parent, home, pc, sp, flags
  indexable:     locals, then the operand stack growing up
```

Locals and the expression stack in one run of words is what removes the two
64-byte `Vec<Value>`s; the object itself is what removes the 112-byte
`Rc<Scope>`. Together that is 1.79M of the 1.86M allocations in a test run.

Activations are allocated in the young space like everything else, and die
there — which is exactly the hypothesis generation scavenging is built on.
`Frame` becomes an `Oop` and a cached base pointer.

*ponytail: a 500,000-frame recursion puts ~50 MB of live activations in the
young space and tenures all of it, because the frame stack is a root array.
The upgrade path is a per-process stack arena with evacuation on escape —
`clone_block` (`interp.rs:304`) is the one place an activation is first
captured, so the escape point is already known. Not worth building until a
deep-recursion benchmark says so.*

### What this deletes

`Rc<Scope>`, `Scope::drop`, `give_scope`, `take_scope`, `give_vals`,
`take_vals`, the `SCOPES` and `VALS` pools, `Frame::retire`, `Scope::dead`,
`Root::Scope`, `walk_scope` and its iterative chain walk, `Seen`, `PtrHash`,
`holds_scope`, and `frame_roots`. It also closes the one write-barrier hole
gc.rs documents: a `Scope`'s slots are mutated through a plain `RefCell` with
no barrier, which is why `holds_scope` has to pin every block that captured one
in the remembered set forever. An activation object is stored into through the
same barrier as everything else.

## The collector

Generation scavenging, as now, but moving bytes rather than cells — and with
direct pointers, **every reference to a moved object has to be rewritten**.
That is the price of dropping the handle table, and it shows up in three
places.

### Scavenge — Cheney with forwarding pointers

1. Take the remembered set, clear the dirty bits.
2. Trace roots (below), rewriting each root as its object is evacuated.
3. Evacuate: copy the object's words to the to-space bump pointer, then write
   the forwarding address into the corpse's mark word with the `forwarded` bit
   set. A second reference to the same object finds the flag and reads the new
   address out. This is `mark_memOop` / `is_marked_memOop`
   (`objects/memOop.hh:50`) — Self overloads the mark word's top bit for
   exactly this.
4. Scan the to-space from `scan` to `bump`, rewriting each `Oop` field to the
   evacuated address.
5. Reset the from-space bump pointer to zero. That is the free: no destructor
   runs, no `free` is called, and the whole space is forgotten at once.

Step 5 is the difference that motivates all of this. Today, freeing the
from-space runs a `Drop` per dead object — a `free` per `Vec` and per `Rc`,
ending in `madvise` — which is what the pools were built to avoid.

### Old generation — mark and sweep, non-moving

Marking is a bit in the mark word, as now. Sweeping puts dead runs on
size-classed free lists.

Non-moving on purpose: a *compacting* old generation with direct pointers needs
a pass over every reference in the heap to repoint it, and the C++ VM's answer
is a transient side table — "the object table is used for pointer forwarding
during a full GC" (`memory/oTable.hh:11`). That is the right thing to build
*second*, when a metric says so, not first.

Three things are given up, not one, and fragmentation is only the first:

* **External fragmentation.** Free runs that no promoted object fits. Bounded
  by size classes, and bounded further by the fact that promotion is the only
  source of old-generation allocation — so the sizes arriving are the sizes
  that were already surviving.
* **Locality.** A compacting collector lays a promoted subgraph out
  contiguously; a free list scatters it in whatever order holes appear. For a
  Self world this is the one that is easy to under-rate, because traversing an
  object graph is what the VM spends its time on.
* **RSS never comes back down.** Without compaction there is nothing to release
  to the OS after a world shrinks.

*ponytail: size-classed free lists, no compaction. `serf_mem_old_fragmentation`
and a promoted-subgraph locality benchmark decide whether to build the
mark-compact pass and its forwarding table.*

### Remembered set

Unchanged in spirit and better than the reference: the C++ VM card-marks at
128-byte granularity (`memory/rSet.hh:11`) and rescans a whole card. serf can
stay exact and per-object, because every mutation of an object word goes
through one store accessor, which is the single site the barrier lives at.

### Roots — the hard part

With a handle table nothing had to be rewritten and a stale reference was
merely stale. With direct pointers **a root the collector does not know about
is a pointer into the from-space after the next scavenge**, and the from-space
gets reused. Missing a root is now silent corruption, not a panic.

serf starts from an unusually good position here, and it is worth being
explicit about why. The one thing that makes precise rooting tractable in a
Rust interpreter is *not keeping interpreter state on the Rust stack* — the
MMTk-in-Rust write-up calls this out as its central refactor ("we keep a big
`Vec<Value>` stack, and all variables we work with are pushed onto it").
serf did that from the start: activations live in a `Vec<Frame>`, not on the
Rust stack, precisely so Self recursion is bounded by memory. That work is
already done.

What remains:

* **`Vm::each_root` becomes rewriting rather than read-only.** Every root it
  yields must be a `&mut Oop` the collector can store through.
* **A shadow stack** for Rust locals held across an allocation: a VM-owned
  `Vec<Oop>` with a `Root` guard that pushes on creation and pops on drop, and
  which the collector updates in place. This is `temp_roots` grown up, and it
  is V8's `HandleScope`/`Local<T>` by another name.
* **`NoGc` stays** for image load, image save and compilation — bounded phases
  that keep half-built graphs in Rust locals, where disabling collection is
  cheaper than rooting every one.
* **Every primitive that allocates twice must root its intermediate.** This is
  the real migration cost: `prims.rs` is 2,113 lines and each allocation site
  needs an audit. `SERF_GC_STRESS` collecting after every allocation is what
  makes that auditable rather than hopeful.
* **Caches keyed by object address must be fixed up, not just flushed.** The
  lookup cache and the inline caches hold map and holder pointers, and a map is
  an ordinary heap object that moves like any other; they live in known
  VM-owned tables, so the collector walks and rewrites them. Maps tenure almost
  immediately and then stop moving, so in the steady state this pass finds
  nothing to do.

## Doing this in Rust

This is where the README's "plain safe Rust" claim ends, so the boundary should
be drawn deliberately rather than allowed to spread.

### The unsafe core

One module — call it `heap.rs` — owns the arenas and is the only place with
`unsafe`. Everything above it sees a safe API: `Oop` is `Copy`, field accessors
take and return `Oop`, and no `&Obj` reference ever outlives the statement that
made it. The MMTk-in-Rust experience report is worth copying almost exactly: a
newtype over a raw pointer with `Deref` and, crucially, a `debug_assert` on
every deref that the address lies inside a live arena, so a stale pointer
fails at the deref rather than three collections later.

Arenas are one allocation each (`Box<[u64]>`, leaked), which matters for more
than tidiness: every object pointer is *derived from that one allocation's
pointer*, so they all share its provenance and moving an object within the
arena is provenance-legal.

### Tag the pointer, do not cast it

The single most useful thing the research turned up. `objects/tag.hh` in the
C++ VM contains this:

> The empty asm is an optimization barrier that emits no instructions but
> forces the optimizer to forget that the pointer came from an aligned type.
> Without it, clang 13+ assumes `this` is aligned and folds the tag bits to
> zero, miscompiling every tag accessor.

The Rust port of that hazard is real and has bitten people — the MMTk-in-Rust
author needed `std::hint::black_box` to stop the release build from breaking,
and describes it as a fragile workaround. Both are the same bug: the compiler
has been told something about a pointer that the tagging then violates.

Rust has a principled fix that C++ does not, and it is stable (these APIs
landed in 1.84; this repo is on 1.97). **Keep the tagged value a pointer, and
change only its address:**

```rust
let tagged = p.map_addr(|a| a | TAG);        // tag
let obj    = tagged.map_addr(|a| a & !MASK); // untag, provenance intact
if oop.addr() & 1 == 1 { /* smallint */ }    // test without a cast
```

Never `as usize` → `as *mut T`. A `usize` cannot represent a pointer — it drops
the provenance, and reconstituting it is exactly the ambiguity that lets the
optimizer conclude something false. `map_addr` preserves it, so nothing is
being lied to and no barrier is needed. It also keeps the design checkable
under Miri, which the pointer-integer round trip does not.

### How you know it works

* **Miri** on the unit tests. Strict provenance plus single-allocation arenas
  is what makes this achievable; it is the closest thing to a proof that the
  unsafe core is sound, and it is worth keeping green from the first commit.
* **`SERF_GC_STRESS`** collects after every allocation. Extend it: poison the
  from-space with a pattern after each scavenge, so a missed root trips the
  deref assertion at the exact use site.
* **`SERF_GC_VERIFY`** already scans every old object for unremembered young
  references. Keep it.

## What this costs, against the handle table it replaces

Being explicit, because these are real regressions and the JIT is what pays for
them:

* **`_Define` goes from O(1) to a heap scan.** `universe::switch_pointers`
  (`memory/universe.cpp:315`) walks the new generation, the old generation, the
  code zone, the profilers, the process list, the string table, the VM strings
  and the slot iterators. serf will need the same, minus the code zone.
* **Object identity stops being a stable integer.** Anything keyed by it needs
  the identity hash from the mark word instead — which is why the hash is in
  the header, and why the side tables move into the object.
* **A missed root is corruption, not a panic.** Mitigated by stress mode,
  poisoning and Miri; not eliminated.
* **`unsafe` enters the codebase.** Confined to one module, but the README
  claim changes.

What is bought: no dependent load before every object access, no second working
set competing for cache, no free-list pop and table store on every allocation,
and an object access a JIT can emit as one instruction. That is the trade, and
for a VM that intends to compile Self to machine code it is the right one — the
C++ VM made the same call for the same reason.

## What still uses `malloc`

* growing an arena, the symbol table and the site arena — each a doubling
  `Vec`, amortised O(1), which is the exemption the design is written around;
* the lookup cache and the canonical-string index: VM caches, not Self objects;
* the parser and compiler's AST, transient scaffolding never reachable from a
  Self object;
* `Ffi`, `c_heap` and I/O buffers, which are foreign memory by definition;
* `String`s in error paths and `--stats`.

Nothing in the Self universe is on that list. That is the acceptance criterion.

## Getting there

Six phases. Each lands on its own, keeps `run-tests.sh` and the `core.snap`
round-trip green, and is provable by a number.

| | | proves it |
|---|---|---|
| 0 | `serf_mem_*` metrics, `SERF_MEM_TRACE=1` allocation counter, Miri in `run-tests.sh` | **done** — 1,835,488 mallocs for the suite, now a metric rather than a probe |
| 1 | `heap.rs`: the arena, `Oop` as a tagged pointer, `map_addr` tagging, the deref assertion | **done** — 10 tests green under `cargo miri test heap::` |
| 2 | Objects in the arena: mark word, map pointer, byte and vector payloads; maps interned per shape; Cheney with forwarding; mark-sweep old gen | `core.snap` resident ~19 MB → ~8 MB; `Slots`, `Payload`, the handle table gone |
| 2b | Key the inline caches and `lookup_cache` on the map | **done, ahead of the rest** — see below |
| 3 | Roots: `each_root` rewrites, the shadow stack, the `prims.rs` audit | `SERF_GC_STRESS` green across the suite and a morphic boot |
| 4 | Methods in the heap | image-load mallocs −170k; `walk_method`, `Seen.methods` gone |
| 5 | Activations in the heap | `test.self` mallocs 1.86M → <1k; both pools and `walk_scope` gone |
| 6 | Annotations, identity hash and kind in the object | `sweep_weak` and four side tables gone |

Phase 3 is the one that can silently corrupt, which is why phase 0 builds the
tools first and phase 3 is its own step rather than a rider on phase 2. Phase 5
is where the payoff is, which argues for not stopping after 2.

### Phases 0 and 1, and what Miri changed

Phase 0 is the malloc counter made permanent: a counting `GlobalAlloc` behind
two relaxed atomics, `serf_mem_mallocs_total` / `_frees_total` /
`_malloc_bytes_total` in the exposition, and `SERF_MEM_TRACE=1` for a line at
exit. The suite is **1,835,488 mallocs**, which is the number the rest of this
document is trying to drive to zero, and it is no longer an ad-hoc probe.

Phase 1 is `src/heap.rs`: the tagged `Oop`, the object header, spaces,
bump allocation, the self-describing linear walk, and forwarding. Nothing in
the VM stands on it yet.

**One design decision came out of Miri rather than out of the plan.** The first
draft gave each space its own `alloc_zeroed`, which reads as the tidy thing to
do. Miri rejected `forwarded()` immediately:

```
error: Undefined Behavior: memory access failed: attempting to access 8 bytes,
but got alloc91680+0x1940 which is at or beyond the end of the allocation of
size 512 bytes
```

A scavenge rebuilds the reference to the copy out of a pointer to the corpse,
so `with_addr` was handing a to-space address the *from-space's* provenance.
That is not a bug to patch; it says the whole heap must be **one allocation**,
with every object pointer derived from its base. Which it now is — spaces are
views on it, and the "is this reference young?" range compare that a direct
model was supposed to give back comes free with it.

Worth noting what this cost to find: nothing. The tests passed in release and
in debug. Only Miri saw it, and it saw it the first time it ran, which is the
argument for phase 0 having built the harness before phase 1 wrote any
`unsafe`.

The remaining 27 Miri errors are `memory leaked` — the heap is deliberately
leaked, as `gc.rs`'s spaces already are, so `run-tests.sh` runs Miri with
`-Zmiri-ignore-leaks` and skips with a note when Miri is not installed.

### Phase 2b, landed early

2b turned out not to need the arena at all, so it went first. Objects still own
their slot vectors; what is interned is the shape — slot names and kinds plus
the value of every parent slot — memoised on the object and forgotten by the
three places that can change it (`put`, a parent `assign`, `_RemoveSlot:`).

A send site keeps one entry and probes it twice: same receiver as last time,
answered without touching the object; else one deref to read the map, and a
receiver of a shape the site has seen answers from there. The two-level probe
is not decoration — keying on the map *alone* costs a deref per send and made a
monomorphic integer loop 10% slower (0.94s → 1.03s), which the identity probe
gives back (0.95s).

| | misses | time |
|---|---|---|
| clone-and-send loop, keyed on the receiver | 800,186 | 0.36s |
| the same, keyed on receiver then shape | **221** | **0.30s** |

`serf_send_site_map_hits_total` counts the probes that hit on a receiver the
site had never seen: 799,965 on that benchmark, which is the old miss count
almost exactly. Real worlds intern about one shape per eleven objects —
`core.snap` 69,954 objects to 6,062 shapes, morphic 138,202 to 12,828.

The invariant needs its own check. The Self-level tests cover the semantics
(dispatch after adding a slot, removing one, rewiring a parent) but *cannot*
catch a missing `forget_map`, because a shape change also bumps `LOOKUP_GEN`
and flushes every site before the stale key is consulted. `SERF_MAP_VERIFY=1`
recomputes each memoised shape and panics where a stale one is used; it runs
over the suite and over `core.snap` in `run-tests.sh`.

Still owed at phase 2: the descriptors themselves move into the map, an object
shrinks to a mark word plus a map pointer plus its assignable values, and the
annotations, the kind byte and `Vm::obj_kind` go with them.

## Risks

* **A missed root**, discussed above. The dominant risk, and the reason for the
  phase ordering.
* **Stale base pointers.** Caching an object's address across an allocation is
  the classic derived-pointer bug. Rule: never hold a base across a safepoint;
  `Frame`'s cached base is re-derived after every collection.
* **The optimizer**, discussed above. Strict provenance is the answer;
  `as usize` round trips are the thing to ban in review.
* **Float boxing** in numeric loops.
* **Old-generation fragmentation and promotion locality**, since the old
  generation no longer moves. Metric first, mark-compact second.
* **Map churn.** `_AddSlots:` on a unique shape mints a map that nothing else
  will ever share, so a program that reshapes objects in a loop allocates a map
  per iteration and gains nothing from the cache. `core.snap` already shows
  26,503 single-object maps; that is fine for methods, which are born once, and
  would not be fine for a hot loop. `serf_mem_maps_minted` is the metric.
* **Scavenge frequency** rises once activations are allocated rather than
  pooled. Scavenge *cost* is proportional to survivors and activations die
  immediately, so this should be a win — but the young space must be sized in
  bytes rather than in objects, and `SERF_GC_YOUNG` changes units.

## Decisions

1. **Boxed floats**, 63-bit smallints, zero-tag pointers. Settled: 155 floats
   in `core.snap` and 3,467 in `morphic.snap` say the integer range and the
   unmasked deref are worth more than inline `f64`.
2. **Maps**, holding slot descriptors, constant slot values, kind and
   annotations. Settled, and this reverses the first draft: the case is not the
   ~2 MB of shared descriptors but the cache key, where 76% of a world's
   objects collapse onto 81 shapes. Self keys `MethodLookupKey` on the receiver
   map for exactly this reason.
3. **Non-moving old generation** to start. Settled: size-classed free lists,
   accepting fragmentation, scattered promotion locality and an RSS floor;
   build the forwarding table and the compaction pass when a metric asks.
