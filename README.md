# serf — a Self VM in Rust

A from-scratch Self implementation: scanner, parser, bytecode compiler, and a
bytecode interpreter, plus a small world written in Self. No JIT, no
dependencies. Built and tested on macOS arm64; it is plain safe Rust, so any
target `rustc` supports should work.

```sh
cargo build --release
```

Paths below like `vm/…` and `objects/…` refer to the C++ Self implementation at
<https://github.com/russellallen/self>. The snapshots in this repo —
`Clean-4.4.snap` as shipped there, and `core.snap`/`morphic.snap` built from
`objects/` by that VM — carry the Self copyright; see [LICENSE.self](LICENSE.self).

That implementation is vendored as a git submodule under `reference/self`, so
those paths resolve locally — `reference/self/vm/…`, `reference/self/objects/…` —
as a reference and test bench while implementing serf:

```sh
git submodule update --init          # or clone with --recurse-submodules
```

### serf's own world

```sh
./target/release/serf                 # REPL, Ctrl-D to leave
./target/release/serf -e '3 + 4 * 2'  # 14 -- binary sends have no precedence
./target/release/serf file.self       # run a file
./target/release/serf file.self -i    # run it, then drop into the REPL
./target/release/serf self/test.self  # 46 checks, non-zero exit on failure
./run-tests.sh                        # everything below, as far as it applies
```

### A C++-format image

`--load` binds the image's lobby as `snapshotLobby`; `-e` prints each
statement's value; `--run` evaluates with the image's lobby as `self`.

```sh
./target/release/serf morphic.snap        # boot an image: banner, then a prompt
                                          # in that world, as `Self -s` does
./target/release/serf morphic.snap -e 'desktop worlds size'   # or drive it directly
./target/release/serf --load core.snap --run '3 + 4'
./target/release/serf --load core.snap -e "(snapshotLobby _SlotAt: 'traits') _SlotNames _Size"
./target/release/serf --load core.snap -e "((snapshotLobby _SlotAt: 'traits') _SlotAt: 'smallInt') _SlotNames"
```

`printLine` does **not** work on a loaded world: it goes through that world's
stdout, which needs Self processes. Use `-e`, which prints the result itself,
or `'...' _StringPrint` for explicit output.

Reading, writing and inspecting images:

```sh
./target/release/serf world.self --save w.snap        # write a snapshot
./target/release/serf --load core.snap --save w.snap  # round-trip a real one
./target/release/serf --verify-image w.snap           # bytes + structural walk
./target/release/serf --dump-image w.snap             # header, spaces, object walk
./target/release/serf --load w.snap --stats           # summary of the reachable graph
./target/release/serf --load w.snap --prims           # primitives the image calls
```

### X11 from Self

Needs a display; headless works:

```sh
Xvfb :99 -screen 0 1024x768x24 &
DISPLAY=:99 ./target/release/serf self/x11-demo.self   # opens a window, draws
DISPLAY=:99 xwd -root -silent > shot.xwd               # look at the result
```

`run-tests.sh` runs that demo against an `Xvfb` it starts itself, so the suite
never pops a window. `SERF_X11=real ./run-tests.sh` uses `$DISPLAY` instead
(XQuartz, a visible window); `SERF_X11=off` skips it.

## What it is

| | |
|---|---|
| `src/lexer.rs` | tokens, after `vm/src/any/parser/scanner.cpp` |
| `src/parser.rs` | grammar, after `vm/src/any/parser/parser.cpp` |
| `src/compile.rs` | bytecodes, the set from `vm/src/any/parser/byteCodes.hh` |
| `src/interp.rs` | the interpreter loop, after `vm/src/any/interpreter/` |
| `src/value.rs` | objects, slots, and the multiple-parent lookup |
| `src/prims.rs` | primitives (`_IntAdd:`, `_Clone`, `_AddSlots:`, …) |
| `src/gc.rs` | the object heap: a generational collector, after `memory/` |
| `src/image.rs` | snapshot file format, after `memory/universe.cpp` and `space.cpp` |
| `src/image_obj.rs` | snapshot words <-> serf objects: maps, slot descriptors, layout |
| `self/init.self` | the world: traits object/boolean/block/number/indexable |

Object model as in the C++ VM: prototypes, no classes; slots are data, parent
(`p* = x`) or assignment (`x <- 3` also makes `x:`); a slot holding code is a
method, activated on lookup; lookup searches the receiver then all parents in
parallel and reports an ambiguity if two distinct slots match. Blocks are
objects with a `value`/`value:`/`value:With:` slot closed over the enclosing
activation; `^` returns from the home method.

Bytecodes are Self's: 4-bit opcode, 4-bit index, wider indices shifted in by
`INDEX_CODE` prefixes; `LEXICAL_LEVEL` before a local access; `DELEGATEE` and
the undirected-resend flag before a send.

Two things worth knowing about the interpreter:

* Activations live in a `Vec`, not on the Rust stack, so Self recursion is
  bounded by memory (500k frames, then a clean error) rather than by a segfault.
* A send in tail position always reuses the caller's frame, which is why
  `whileTrue:` — genuine recursion in Self — runs in constant space. An
  activation that tail-calls has not returned, so the callee's frame carries
  it: a `^` out of one of its blocks finds the continuation there and returns
  through it. (Before there was a collector this was decided by asking whether
  anything else still held the scope, which only worked because `Rc` freed a
  discarded block immediately.)

## Garbage collection

Generation Scavenging, as in `memory/`: a young generation of two semispaces
that surviving objects are copied between, and an old generation swept by mark
and sweep. An object that survives two scavenges is tenured. A `Value` is a
handle — an index into a table saying where the object currently is — so moving
an object is one table store and no reference anywhere else changes. That is
also what makes `_Define` cheap: the C++ VM scans the whole heap to switch
pointers (`memory/universe.cpp:315`), serf assigns.

Old objects that are written to are remembered, so a scavenge scans them and
not the whole old generation; the remembered set is exact rather than
card-marked, because `ObjRef::borrow_mut` is the only way to mutate an object
and can therefore do the recording itself.

Allocation never collects: it fills the young space and asks for a collection,
which happens at a safepoint between two bytecodes, where every live reference
is reachable from the `Vm`. The interpreter lends its activation stack to the
`Vm` across anything that can re-enter it, and image load, image save and
compilation — which keep half-built graphs in Rust locals — suspend collection
outright.

A collection **stops the world**, and the world is everything: serf runs one
Self process on one thread, so between the two lines `SERF_GC_TRACE` prints
nothing interprets, allocates or calls out. Nothing is incremental or
concurrent. On a loaded Morphic world a scavenge pauses for a few ms and a full
collection for ~30 ms.

```sh
SERF_GC_TRACE=1  ./target/release/serf …   # a line as each pause starts and
                                           # ends, with what it cost
SERF_GC=off      ./target/release/serf …   # never collect
SERF_GC_STRESS=1 ./target/release/serf …   # collect after every allocation and
                                           # never recycle a handle, so touching
                                           # a missed root panics on the spot
SERF_GC_VERIFY=1 ./target/release/serf …   # scan every old object, not just the
                                           # remembered ones: one that was
                                           # written without the barrier firing
                                           # fails the run
SERF_GC_STATS=1  ./target/release/serf …   # a line per collection
SERF_GC_YOUNG=n  ./target/release/serf …   # objects per semispace (65536)
```

`memory scavenge` and `memory garbageCollect` work from Self, through
`_Scavenge` and `_GarbageCollect`; both take effect at the next safepoint.

## Images

`--save` and `--load` read and write the C++ VM's snapshot format: the ASCII
header, then a raw dump of the 32-bit tagged heap, the canonical string table
(20011 buckets, `hashpjw`), the 182 `VMString[]` handles and the vtbl table.
Reading rebuilds serf objects from the heap's maps -- slot descriptors become
slots, `obj` slots read the object's words, a constant slot holding an
assignment object becomes an assignable slot, and method maps become serf
methods whose bytecodes the interpreter runs directly (it is the same
instruction set). Writing goes the other way: it canonicalises a map per
distinct object shape, lays out one old space, and rebuilds the string table.

The two hard sections are empty by construction: with `Snapshot code: n` the
C++ `zone::write_snapshot` writes nothing, and `Process::write_snapshot` is a
no-op in the C++ VM as well.

### Checked against a real world

`core.snap` is an 8.4 MB image built from `objects/` by
`worldBuilder.self` on a VM compiled from `vm/`. serf reads it, walks all
185,644 objects, reaches 69,940 from the lobby, runs its methods, and writes
it back out. Round-tripping it reproduces the reachable graph exactly: same
objects, slots, parent slots, assignment slots, annotations, methods,
bytecodes, literals, blocks, vectors, and the same checksum over every integer
and float. `run-tests.sh` asserts that whenever `core.snap` is present.

That world found nine bugs that the synthetic tests could not:

* `blockMethodMap` adds `_sourceOffset` and `_sourceLen`, so its slot
  descriptors start 11 words into the map, not 9 -- which is why it overrides
  `slots()` in `codeSlotsMap.hh`. The reader desynchronised after 6,963 objects.
* The writer emitted the 9-word form of that map, so its own images desynced.
* **Assignment slots did not exist.** Self stores only the data slot and
  derives `x:` from that slot being an *obj* slot
  (`slotDesc::assignment_slot_name`, `assert(is_obj_slot(), ...)`). The whole
  world decoded with zero assignable slots, so nothing in a loaded world could
  be written to. There are 90,033 of them.
* An arg slot's `data` is its **index into the argument list**, not a word
  offset into the object, and argument order comes from that index rather than
  from the slot's position in the map.
* Every map annotation was dropped -- 206k maps carry one, so a round trip lost
  every comment, category and module-info in the world.
* Mirrors, proxies, processes and vframes were demoted to plain slots objects.
* Method slot flags were lost: block methods' lexical-parent slots (8,626 of
  them) came back as ordinary data slots, and constant slots became per-object.
* Loaded strings were written back as byte vectors, because the string test
  compared against *serf's* `traits string` rather than the image's.
* Each block minted a duplicate copy of its value method object.

Verification, given the C++ VM is 32-bit i386 and cannot run here:
`--verify-image` re-serialises a snapshot and requires the bytes to come back
identical, then walks every space linearly the way the C++ enumeration does,
computing each object's size from its map -- the objects must tile the region
exactly. (That check earned its keep: it found a map word being written over
the shared assignment object.) `run-tests.sh` additionally saves a world with
methods, blocks, closures and recursion, reloads it, and runs it.

Caveats worth knowing before pointing this at a real Self 4 world:

* Integers must fit Self's 30-bit `smi` and floats its 30-bit float; `--save`
  errors rather than truncating.
* Live blocks cannot be written -- their home activation is not a heap object.
* Compressed images and `Snapshot code: y` images are refused on read.
* A loaded world runs only as far as the primitives it calls exist here; the
  Self 4 world wants hundreds, plus processes and the `IfFail:` protocol.
* An image written from serf's own small world is well-formed but has nothing
  for the C++ VM to boot into -- round-tripping a real image is the useful path.
* Only reachable objects are carried over, so a round trip collects garbage:
  `core.snap` holds 185,644 objects and 69,940 are reachable.
* Canonical strings with equal content are written as one object. The world
  holds ~200 such pairs; Self's own invariant says canonical strings are
  unique by content, so this merges what should already have been merged.
* A `mapOop` reachable as an ordinary value decodes to an empty object -- serf
  has no maps to decode it into.
* Running a loaded world stops at the first primitive serf lacks (`_Mirror`,
  `_ThisProcess`, ...); reading, writing and calling ordinary methods work.

## Running an image's own world

`--load` binds the image's lobby as `snapshotLobby`, `--run` evaluates an
expression with that lobby as `self`, and `--prims` lists every primitive the
image's methods mention. Loaded methods execute directly: the bytecode set is
the same one serf compiles to.

```sh
./target/release/serf --load morphic.snap --run "snapshotAction postRead"
./target/release/serf --load morphic.snap --prims | head
```

Two things make a loaded world run rather than merely decode. Primitives whose
selector ends in `IfFail:` hand the error string to the block, exactly as the
C++ VM does for `primitiveNotDefinedError`, so the world routes around
primitives serf lacks instead of stopping. And everything serf hands back --
integers, error strings, booleans, vectors -- must inherit from the *image's*
traits, taken from its `smi_map`/`float_map` and its prototype roots; using
serf's own left `42 pred` not understood and put `init.self` frames in the
middle of Self backtraces.

### Running the Morphic GUI

`morphic.snap` is 16.5 MB, 316,021 objects, 151,636 reachable from the lobby.
It boots to the world's own console, and the world's Morphic desktop draws on
a real X server.

```sh
export DISPLAY=/private/tmp/com.apple.launchd.XXXXXXXX/org.xquartz:0   # XQuartz
./target/release/serf morphic.snap
```

That is all: the world comes up, prints its banner, opens its desktop and
offers its own prompt. `shots/18-morphic-autodisplay.png` is what it looks
like -- the saved world's outliners, drawn by the real Morphic with real
fonts. Expressions run against that world with `--run`: `3 + 4`, `desktop
open`, `paintNames at: 'black'`.

A snapshot remembers the display it was saved on, and `morphic.snap` was built
in a container against an Xvfb, so its answer is `8f40c1e90598:99.0` and it is
nobody's machine. serf falls back to `$DISPLAY` and says so:

```
serf: no display "8f40c1e90598:99.0"; using $DISPLAY
```

Set `SERF_DISPLAY_STRICT` to keep the world's own answer instead, and it will
offer you its "Could not open display" menu, where choice 2 is Try Local
Display and choice 1 lets you type a name.

What it took, beyond the interpreter and the image reader:

* **Foreign calls.** `src/ffi.rs` resolves glue primitives against libX11 by
  name, so the 362 `_X...` primitives the image mentions dispatch generically
  rather than one by one. The generated conventions matter: `...ResultProxy:`
  fills the last argument and the unary `...ResultProxy` fills the receiver, a
  `_len` string is passed as pointer *and* length, a NULL answer from a
  `proxy`-typed call is a failure, and `int_or_errno` answers the errno name.
  The wrappers `objects/glue/xlib_glue.cpp` compiles into the C++ VM are in
  `prims.rs`: the region calls are renames, and XCreateImage, XImagePutData,
  XFillPolygon, XDrawLines, XNextEvent and friends are a few lines each.
* **Self processes.** TWAINS is *asymmetric* -- the scheduler runs a process
  with `p _TWAINSResultVector: r SingleStep: b StopAt: a IfFail: fb` and the
  process transfers back with `_Yield: action` -- so a process is a nested
  interpreter loop rather than a rewrite into symmetric coroutines.
  `run_stack` takes a `&mut Vec<Frame>` and answers `Done` or `Yielded`, so a
  stack parks and resumes; `Vm::procs` holds one per process object.
* **The event loop.** `scheduler schedule` answers `schedulerProcess` when its
  readyQ is empty, so the world runs TWAINS on the process it is already in.
  That means *wait for the next signal* (`TWAINS_await_signal`). serf sleeps a
  tick and reports `sigio` and `sigrealtimer`, which is what drives the
  console and the timer queue; `select_wrap` answers which descriptors are
  ready.
* **doesNotUnderstand.** A failed lookup is a message, not an error: the VM
  sends `undefinedSelector:Receiver:Type:Delegatee:MethodHolder:Arguments:` to
  the current process, and the world forwards it to the receiver's own
  handler. `x11Globals fontFamily` conjures font families that way.
* **Identity hashes and canonical strings.** Self keeps an object's identity
  hash in its mark word so hashed collections survive a snapshot, and `string
  hash` is `canonicalize identityHash`. Deriving a hash from the address, or
  compiling a string literal to a fresh object, leaves every dictionary in the
  image unable to find its own keys.
* **The parser, handed back to the world.** `_ParseObjectFileName:ErrorObj:`
  parses a string as an object body and answers a mirror on it -- a method
  when the body has code, an object when it is only slots -- and
  `_MirrorEvaluate:` runs such a method with a mirror's reflectee as `self`.
  That is how every button script, editor accept and doIt in Morphic gets
  compiled -- without it a ui2Button kept the default `event:From:` it
  inherits, so pressing one sent `buttonPress:Event:` to an outliner that has
  no such slot. What serf compiles carries no source: its parser records no
  per-method spans, so such a method answers `''`.

Still rough: activation mirrors are missing, so the debugger cannot show a
stack.

## Deliberately not here

* **No JIT and no maps.** Every object owns its slot vector and lookup is a
  linear scan. ~1.5M sends/s; a 3M-iteration loop takes ~4s. Add maps and
  inline caches when that number matters.
* **No compaction and no finalization.** The old generation is swept into a
  free list, never compacted, and nothing runs when an object dies.
* **No processes, no `IfFail:` protocol, no debugger.** Errors abort to the
  REPL with a Self-level backtrace.
* **No `{ }` slot annotations**, so the Self 4 world fileouts in `objects/`
  don't load as-is.
* **No `|` binary selector** — a bare `|` always closes a slot list. Use
  `bitOr:`.

## Sample

```self
traits _AddSlots: ( | shape = (| parent* = traits object.
    area = ( _Error: 'subclass responsibility' ).
    describe = ( 'area ' , area printString ) |) | ).

traits _AddSlots: ( | circle = (| parent* = traits shape.
    r <- 1.
    area = ( 3.14159 * r squared ).
    describe = ( 'circle: ' , resend.describe ) |) | ).

((traits circle copy r: 2) describe) printLine.   "circle: area 12.56636"
```

## License

Apache 2.0, see [LICENSE](LICENSE). The `*.snap` images are part of the Self
system and keep its own licence, see [LICENSE.self](LICENSE.self).
