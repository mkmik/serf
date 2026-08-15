# Finishing the flip

Delete this file when the branch is green.

`MEMORY.md` is the design and it has not changed. This is the operational note:
what state the branch is in, and the order to work in.

## Where it is

Branch `flip`, five commits ahead of `worktree-precious-tumbling-hinton`.
The first four are green. The fifth does not compile, on purpose.

```
fa02b85  WIP: the flip, half done -- DOES NOT COMPILE
0ac9c74  Carry a miniature Self world on the new object model      green
495c37d  Build the Self object model against the arena             green
0d1ecb7  Give the header an aux byte and an untraced region        green
680c056  Collect the shape an interpreter actually makes           green
```

`cargo build --release 2>&1 | grep -c '^error'` was **187** at `fa02b85`.

Rewritten and believed correct: `heap.rs`, `obj.rs`, `value.rs`, `gc.rs`, and
the activation half of `interp.rs`. Nothing below is design work; it is call
sites.

## The order

1. **`interp.rs`** (~27). Finish what the activation change started:
   `nth_lexical`, `clone_block`, `run`, and the send path. A `Scope` field is
   now a function -- `act_method(a)`, `act_recv(a)`, `act_holder(a)`,
   `act_lexical(a)`, `act_local(a, i)`, `act_set_local`, `act_dead`,
   `act_set_dead`, `home_of`.
2. **`prims.rs`** (~104). Almost all of it is `match &b.payload { Payload::X(..) }`
   becoming `match b.payload.kind() { PayKind::X }` with the accessor beside it:
   `b.payload.bytes()`, `.vector()`, `.element(i)`, `.method()`,
   `.block_scope()`, `.mirror()`, `.proxy()`. `_AddSlots:` and `_RemoveSlot:`
   are now `vm.put_slot` / `vm.remove_slot`, which widen by building a new
   object and switching every pointer -- so they need `&mut Vm`, and a caller
   holding an `ObjRef` across one is holding a stale pointer.
3. **`image_obj.rs`** (~29) and **`main.rs`** (~27). The three side tables are
   gone and their contents move into the object:
   * `vm.id_hash` -> `heap::set_hash` / `heap::hash`, 22 bits, which is what
     Self's mark word gives it (`objects/markOop.hh:15`).
   * `vm.obj_kind` -> `heap::set_aux` / `heap::aux`, the byte added for it.
   * `vm.anno_obj`, `vm.anno_slot` -> `heap::set_obj_anno` /
     `heap::set_slot_anno`, on an object built with `Shape::annotated()`.
     The image reader has to know an object is annotated *before* it builds it,
     which is the one place this reorders work.
   `image_obj.rs`'s own `index: HashMap<usize, usize>` keyed on `o.id()` is
   fine: it lives inside a `NoGc` phase, so nothing moves under it.

## Then, in this order, because each finds what the last cannot

```sh
cargo test --release          # heap.rs and obj.rs, which should still pass
./target/release/serf self/test.self
./run-tests.sh                # the core.snap round-trip is the real test
SERF_GC_STRESS=1 ./target/release/serf self/test.self
```

The `core.snap` round-trip walks 69,954 objects, 265,153 slots, annotations,
methods, blocks and floats, and compares every one after a save and a reload.
It is what will find the mistakes.

Then `SERF_GC_STRESS=1` over the whole suite: it collects after every
allocation, which is what turns a root nobody told the collector about from
silent corruption into a failure at the site. **Expect this to find real bugs
in `prims.rs`** -- a primitive that allocates twice and holds an `ObjRef`
across the second one is now holding a stale pointer, where before it was
merely holding a stale handle that still worked. Anything it catches wants
`vm.temp_roots`.

## Two things that will bite

* **An `ObjRef` is an address and it moves.** Nothing may hold one across an
  allocation or a safepoint unless it is in `Vm`. This is the whole difference
  from the handle table, and `prims.rs` was written when it did not matter.
* **`put_slot` invalidates every pointer to the object it widens.** It returns
  nothing; the caller has to re-fetch. `switch_pointers` fixes the heap and the
  roots, not a Rust local.

## What "done" looks like

`./run-tests.sh` exit 0, and then the number the whole exercise is for:

```sh
SERF_MEM_TRACE=1 ./target/release/serf self/test.self
```

was `mallocs 1835488` before. Steady-state interpretation should now allocate
nothing: expect four figures, not seven, and nearly all of it in the parser and
the compiler rather than in the interpreter.
