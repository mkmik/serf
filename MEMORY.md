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
word 0   mark:  forwarded:1 │ identity hash:23 │ age:8 │ kind:8 │ flags:8 │ tag:8
word 1   size in words:32 │ nslots:32
word 2   slot 0 descriptor:  name (Sym):32 │ kind:8 │ flags:8 │ pad:16
word 3   slot 0 value        Oop
  ⋮      … nslots × 2 words …
         [FLAG_ANNO]     object annotation Oop, then nslots slot-annotation Oops
         [kind indexable] length in elements, then the bytes or the Oops
```

This is the C++ VM's header, widened. Self's mark word is
`tag:2 │ hash:22 │ age:7 │ marked:1` (`objects/markOop.hh:15`) and its second
word is the map pointer; serf has no maps, so the second word carries the size
and slot count that a map would otherwise hold.

* `hash` is the identity hash Self keeps in the mark word, and its being here
  is what deletes `Vm::id_hash`. An address cannot serve, because the object
  moves.
* `kind` replaces `Payload`: `Slots`, `Bytes`, `ObjVector`, `Method`, `Block`,
  `Mirror`, `Proxy`, `Float`, `Activation`, `Process`. It deletes
  `Vm::obj_kind`.
* `forwarded` is the scavenge's flag; see below.
* Annotations are a trailing region present only when `FLAG_ANNO` is set —
  serf's own world has none and should not pay for them; a loaded world has
  218,474 and needs them somewhere that is not a Rust hash map. It deletes
  `Vm::anno_obj`, `Vm::anno_slot`, `Vm::anno_young`, `note_anno`, and the
  annotation write barrier.

**No maps.** The C++ VM shares slot descriptors between clones through a map
and pays for it with map canonicalisation, map transitions on every
`_AddSlots:`, and dependency lists — the largest single subsystem in
`objects/`. The win over per-object descriptors on `core.snap` is 265,153 slots
× 8 bytes ≈ 2 MB. Not worth it yet. Add maps if a world ever clones one shape
hundreds of thousands of times; `serf_mem_slot_words` will say so.

### Sizing check

`core.snap`'s reachable world, laid out this way:

```
69,954 objects × (2 + 2×3.8) words        5.4 MB
694,669 bytes of string data              0.7 MB
76,093 vector elements                    0.6 MB
161,177 bytecodes + 54,846 literals       0.6 MB
annotations, on the objects that have any ~1.7 MB
                                        ────────
                                          ~9 MB, in one arena, zero malloc blocks
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
*second*, when a fragmentation metric says so, not first.

*ponytail: size-classed free lists, no compaction. `serf_mem_old_fragmentation`
decides whether to build the mark-compact pass and its forwarding table.*

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
  lookup cache and the inline caches hold receiver and holder pointers; they
  live in known VM-owned tables, so the collector walks and rewrites them.

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
| 0 | `serf_mem_*` metrics, `SERF_MEM_TRACE=1` allocation counter, Miri in `run-tests.sh` | the numbers above stop being ad-hoc; the harness exists before the unsafe does |
| 1 | `heap.rs`: the arena, `Oop` as a tagged pointer, `map_addr` tagging, the deref assertion | Miri green on a heap-only unit test |
| 2 | Objects in the arena: header, slots, byte and vector payloads; Cheney with forwarding; mark-sweep old gen | `core.snap` resident ~19 MB → ~9 MB; `Slots`, `Payload`, the handle table gone |
| 3 | Roots: `each_root` rewrites, the shadow stack, the `prims.rs` audit | `SERF_GC_STRESS` green across the suite and a morphic boot |
| 4 | Methods in the heap | image-load mallocs −170k; `walk_method`, `Seen.methods` gone |
| 5 | Activations in the heap | `test.self` mallocs 1.86M → <1k; both pools and `walk_scope` gone |
| 6 | Annotations, identity hash and kind in the object | `sweep_weak` and four side tables gone |

Phase 3 is the one that can silently corrupt, which is why phase 0 builds the
tools first and phase 3 is its own step rather than a rider on phase 2. Phase 5
is where the payoff is, which argues for not stopping after 2.

Maps are phase 7 and probably never.

## Risks

* **A missed root**, discussed above. The dominant risk, and the reason for the
  phase ordering.
* **Stale base pointers.** Caching an object's address across an allocation is
  the classic derived-pointer bug. Rule: never hold a base across a safepoint;
  `Frame`'s cached base is re-derived after every collection.
* **The optimizer**, discussed above. Strict provenance is the answer;
  `as usize` round trips are the thing to ban in review.
* **Float boxing** in numeric loops.
* **Old-generation fragmentation**, since the old generation no longer moves.
  Metric first, mark-compact second.
* **Scavenge frequency** rises once activations are allocated rather than
  pooled. Scavenge *cost* is proportional to survivors and activations die
  immediately, so this should be a win — but the young space must be sized in
  bytes rather than in objects, and `SERF_GC_YOUNG` changes units.

## Decisions to confirm

1. **Boxed floats** (63-bit smallints, zero-tag pointers) versus NaN-boxing.
   Recommendation: box them; 155 floats in `core.snap` and 3,467 in
   `morphic.snap` say the integer range and the unmasked deref are worth more.
2. **No maps**, per-object slot descriptors. Recommendation: no maps, and
   measure before revisiting.
3. **Non-moving old generation** to start. Recommendation: yes; build the
   forwarding table and the compaction pass when a metric asks for it.
