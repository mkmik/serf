# serf internals

How the VM is put together: scanner, parser, bytecode compiler, interpreter,
generational collector, the C++ VM's snapshot format, and a small world written
in Self. [README.md](README.md) is the short version.

`unsafe` lives in five modules and nowhere else: `heap.rs` (a moving collector
cannot be written in safe Rust), `ffi.rs` and the X11 glue in `prims.rs`
(foreign calls), `native.rs` (the C structs the world hands it), and
`metrics.rs` (a counting `GlobalAlloc`). The heap is checked under Miri.

```sh
cargo build --release
cargo build --release --no-default-features   # the VM alone: no dependencies
```

The VM has no dependencies, and `run-tests.sh` builds it that way every run so
it stays true — that is what makes it portable. The `native` feature, on by
default, adds the canvas that draws without an X server, and it takes three
crates: `cosmic-text` for text, `winit` for the window and `softbuffer` for its
pixels. Nothing in that tree needs a C toolchain — no `bindgen`, no
`pkg-config`, and the only `-sys` crates are Apple's own framework bindings — so
it cross-compiles much the way the VM does.

Paths below like `vm/…` and `objects/…` refer to the C++ Self implementation at
<https://github.com/russellallen/self>, vendored as a submodule under
`reference/self`:

```sh
git submodule update --init          # or clone with --recurse-submodules
```

The snapshots in this repo — `Clean-4.4.snap` and `Demo-4.4.snap` as shipped
there, and `core.snap`, `morphic.snap` and `gas.snap` written by that VM — carry
the Self copyright; see [LICENSE.self](LICENSE.self).

## Running it

```sh
./target/release/serf                 # REPL, Ctrl-D to leave
./target/release/serf -e '3 + 4 * 2'  # 14 -- binary sends have no precedence
./target/release/serf file.self       # run a file
./target/release/serf file.self -i    # run it, then drop into the REPL
./target/release/serf self/test.self  # 81 tests, non-zero exit on failure
./run-tests.sh                        # unit tests, Miri, the Self checks, GC
                                      # checks, image round-trips, X11
```

### A C++-format image

`--load` binds the image's lobby as `snapshotLobby`; `--run` evaluates with that
lobby as `self`; `-e` prints each statement's value. Naming an image
positionally instead *boots* it, as `Self -s` does — the world takes the
process over from there, so nothing after it on the command line runs.

```sh
./target/release/serf morphic.snap                    # boot: banner, then the
                                                      # world's own prompt
./target/release/serf --load morphic.snap --run 'desktop worlds size'
./target/release/serf --load core.snap --run '3 + 4'
./target/release/serf --load core.snap -e "(snapshotLobby _SlotAt: 'traits') _SlotNames _Size"
```

Anywhere a file can be named, so can an `http://` or `https://` URL: it is
fetched once into `$XDG_CACHE_HOME/serf` (`$SERF_CACHE` overrides, `~/.cache/serf`
by default) and revalidated from then on — If-Modified-Since and If-None-Match,
so an unchanged world costs one 304 and no download. Fetching is curl's job.
When the server cannot be reached, the cached copy is used.

```sh
./target/release/serf --load https://example.org/worlds/core.snap --run '3 + 4'
```

`printLine` does **not** work on a loaded world: it goes through that world's
stdout, which needs Self processes. Use `-e`, which prints the result itself, or
`'...' _StringPrint`.

```sh
./target/release/serf world.self --save w.snap        # write a snapshot
./target/release/serf --load core.snap --save w.snap  # round-trip a real one
./target/release/serf --verify-image w.snap           # bytes + structural walk
./target/release/serf --recompress in.snap out.snap   # gzip one without booting it
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

`run-tests.sh` runs that demo against an `Xvfb` it starts itself.
`SERF_X11=real` uses `$DISPLAY` instead (XQuartz, a visible window);
`SERF_X11=off` skips it.

## Drawing without an X server

The world draws the way X does — `XCreateGC`, `XFillRectangle`, `XCopyArea`,
`XLoadQueryFont` with an XLFD and then `XDrawString` — but nothing says an X
server has to be behind that. The native backend answers those calls in serf
itself, onto a buffer of `0x00RRGGBB` pixels, with text from the fonts installed
on the host. No Self code changes: it is the same interface the image already
codes against.

It is the default on macOS, which ships no X server; elsewhere ask for it with
`SERF_BACKEND=native`. `SERF_BACKEND=x11` goes back to a real server either way.

```sh
./target/release/serf morphic.snap                     # native, on macOS
SERF_BACKEND=native ./target/release/serf morphic.snap # native, anywhere
SERF_SHOT=f.png ./target/release/serf Demo-4.4.snap

./target/release/serf --draw-demo draw.png    # every drawing call, one sheet
./target/release/serf --text-demo fonts.png   # the fonts the world asks for
./target/release/serf --event-demo            # a scripted click, read back
./target/release/serf --window-demo           # a real window, with real input
```

Two pieces of luck make it small. **The struct paths already work**: `XEvent_new`
mallocs into `vm.c_heap` and the field accessors read at `struct_table` offsets,
so an event filled in by `events::encode` needs no special case, and the same
trick answers `XLoadQueryFont` — hand back a real `XFontStruct` and the world's
`XFontStruct_ascentascent` finds what it expects. **And everything else is a
handle**: a `Display`, `Screen`, `GC` or drawable is an integer the world only
hands back, so it is tagged and indexed rather than allocated. A window, a
pixmap and an `XImage` are one type, because to this backend they are the same
rectangle of pixels.

An X call the backend does not implement is an error rather than a fall-through
to `dlsym`. On a host that has libX11 installed, handing it one of these handles
would not fail — it would crash — and the world's `IfFail:` routes around an
error perfectly well.

Careful with anything that reaches *into* a struct Xlib allocated. A wrapper
that merely calls Xlib fails loudly when Xlib is absent; one that reads its
memory fails silently. `XImagePutData_wrap` dispatches every pixel through a
function pointer it loads out of an `XImage`, which given a handle is not a call
that fails but a jump into whatever that address holds. Both such wrappers in
`prims.rs` branch on the backend.

### Text

The contract is smaller than X's font machinery suggests. `src/struct_table.rs`
shows the image only ever reads `XFontStruct` ascent, descent, `fid`,
`min`/`max_char_or_byte2` and `per_char` — and `per_char` has no accessor that
indexes it, only one that asks whether it is there, so answering NULL forces
every width through `XTextWidth`. Three numbers and a width function is all of
it. Two things that are easy to get wrong:

* **Measure and draw from one layout.** `XTextWidth` and `XDrawString` have to
  agree exactly, or a text morph's cursor and its selection walk off the glyphs
  they belong to, so a string is laid out once and both read that.
* **Grayscale antialiasing, never subpixel.** Morphic moves rendered pixels
  around constantly, and subpixel-filtered text refringes the moment it is
  blitted somewhere else.

Family names need one step of help: `fontdb` matches them with `==`, and the
world spells them the way X did (`helvetica`, `lucidaTypewriter`), so they are
resolved against the host's own spelling first. A family the host does not have
falls back by shape rather than by name — the world's consoles have to stay
monospaced.

### The display's scale

On a screen with more than one real pixel per logical one, the world would draw
everything at half size: it is an X client and thinks in device pixels. So a
`Canvas` keeps *its* coordinates and carries a scale, and the widening happens
in `plot`, which every drawing call funnels through.

Text and blits are the exceptions, on purpose. Glyphs rasterise at the scale and
blend at real pixels, so they are sharp rather than drawn small and doubled —
and blits copy real pixels, because Morphic draws its text into a backing pixmap
and copies that to the window, so a copy that worked a logical pixel at a time
would flatten every glyph on the way. `SERF_SCALE=1` forces it off, which is the
only way a headless run has of saying.

### Input, which is bytes rather than a call

An event is the one place the world does not go through a function at all. It
allocates 192 bytes with `XEvent_new`, has the server fill them, and then loads
fields straight out of that buffer — `XButtonEvent_xx` is a four-byte read at
offset 64. So `src/events.rs` does not hand the world a struct, it writes a
*layout*, through the same `src/struct_table.rs` the reader reads through.

Three things about that layout are not optional:

* **`XLookupString` is handed the event and nothing else**, so the keysym has to
  come back out of the encoded keycode. Keycodes are derived from the keysym in
  two disjoint bands, because folding both into a low byte puts `XK_Return` on
  the same keycode as Ctrl-M.
* **Events carry a timestamp**, at offset 56. Morphic tells a click from a
  double click from a press-and-hold by *when* they arrived, so a frozen clock
  makes every gesture identical — a single click does nothing at all and two in
  a row come out as something else.
* **X reports motion when the pointer *moves*.** A window system reports a
  cursor position on other occasions too, and a motion that did not move is not
  nothing to the world: motion with a button held is a *drag*, so passing those
  on turns every click into one and picks morphs up instead of clicking them.

Only a hand on the mouse can click a world, which makes a bug in what a click
*does* the one kind this cannot chase on its own — so it can be told to click:

```sh
SERF_TRACE_INPUT=1 …    # every event as it is handed to the world
SERF_CLICK=254,681@20   # click there after 20s; `x2` for twice
```

`SERF_CLICK` spreads its press and release over time on purpose. A real click is
a press, a pause and a release, and the world sees each in a different turn of
its own loop; firing them into the queue together is a different gesture, and
one the world reads differently.

### What is deliberately not there

Tiles, stipples, dashes and fill styles are accepted and ignored, so everything
draws solid — the 4.4 worlds never set one. Line joins and caps are whatever a
square pen dragged along the run leaves. There is one window, because winit
allows one event loop per process, so a world that opens a second gets the
first. `MapNotify`, `ReparentNotify` and the rest are not generated: nothing
produces them without a window manager in the loop.

## What it is

| | |
|---|---|
| `src/lexer.rs` | tokens, after `vm/src/any/parser/scanner.cpp` |
| `src/parser.rs` | grammar, after `vm/src/any/parser/parser.cpp` |
| `src/compile.rs` | bytecodes, the set from `vm/src/any/parser/byteCodes.hh` |
| `src/interp.rs` | the interpreter loop, after `vm/src/any/interpreter/` |
| `src/heap.rs` | the arena and the collector: tagged pointers, one allocation |
| `src/obj.rs` | the Self object laid out in that arena: slots, payloads, fields |
| `src/gc.rs` | when a collection may run, and what it is allowed to see |
| `src/value.rs` | the `Vm`, slots, lookup across multiple parents, maps |
| `src/prims.rs` | primitives (`_IntAdd:`, `_Clone`, `_AddSlots:`, …) and X11 glue |
| `src/ffi.rs` | resolving glue primitives against a shared library by name |
| `src/image.rs` | snapshot file format, after `memory/universe.cpp` and `space.cpp` |
| `src/image_obj.rs` | snapshot words <-> serf objects: maps, slot descriptors, layout |
| `src/metrics.rs` | Prometheus metrics, over a one-page HTTP server |
| `src/canvas.rs` | drawables, the graphics context, and X's drawing calls |
| `src/text.rs` | X's core font calls, answered from the host's own fonts |
| `src/events.rs` | the event queue, and an `XEvent` laid out as the world reads it |
| `src/window.rs` | winit and softbuffer, translated into X events and one surface |
| `src/native.rs` | the world's `_X…` primitives, answered without an X server |
| `self/init.self` | the world: traits object/boolean/block/number/indexable |

Object model as in the C++ VM: prototypes, no classes; slots are data, parent
(`p* = x`) or assignment (`x <- 3` also makes `x:`); a slot holding code is a
method, activated on lookup; lookup searches the receiver then all parents in
parallel and reports an ambiguity if two distinct slots match. Blocks are
objects with a `value`/`value:`/… slot closed over the enclosing activation;
`^` returns from the home method.

Bytecodes are Self's: 4-bit opcode, 4-bit index, wider indices shifted in by
`INDEX_CODE` prefixes; `LEXICAL_LEVEL` before a local access; `DELEGATEE` and
the undirected-resend flag before a send.

Two things worth knowing about the interpreter:

* Frames live in a `Vec`, not on the Rust stack, so Self recursion is bounded by
  memory (500k frames, then a clean error) rather than by a segfault. The
  activations themselves are heap objects, operand stack included.
* A send in tail position always reuses the caller's frame, which is why
  `whileTrue:` — genuine recursion in Self — runs in constant space. An
  activation that tail-calls has not returned, so the callee's frame carries it:
  a `^` out of one of its blocks finds the continuation there and returns
  through it.

Known problems that are reproducible and not fixed live in [OPEN.md](OPEN.md).

## Maps, and what a send caches on

A send caches what lookup found, and the question is what to key that on. The
receiver's identity is the wrong answer: a loop over a thousand clones of one
prototype presents a thousand receivers and misses every time. The C++ VM keys
on the receiver's *map* (`lookup/key.hh:49`), and so does serf.

serf's map is the shape a lookup depends on: every slot's name and kind, plus
the value of every **parent** slot, since a search recurses into those. A data
slot's value is not in it, because it cannot change what a lookup finds. Shapes
are interned, so two objects of one shape name the same `MapRef` — `core.snap`
is 6,064 shapes over its reachable graph, one per 11.5 objects; morphic 12,830,
one per 10.8.

Each send site keeps one entry and probes it twice: the same receiver as last
time answers without touching the object at all, and a *different* receiver of
the same shape answers after one deref to read its map. The first is what a
monomorphic site wants; the second is the one receiver keying could never make.

```sh
SERF_MAP_VERIFY=1 ./target/release/serf …   # check every memoised map against a
                                            # freshly computed shape, so a
                                            # mutation that changed a shape
                                            # without saying so fails on the spot
```

## Garbage collection

Generation Scavenging, as in `memory/`: one arena of direct tagged pointers, a
young generation of two semispaces that survivors are copied between, and an old
generation swept by mark and sweep. An object that survives two scavenges is
tenured. There is no handle table — a `Value` *is* the pointer — so a scavenge
leaves a forwarding pointer behind and updates every reference it scans. That is
also the bill `_AddSlots:` pays: adding a slot widens an object, which means
rebuilding it and walking the heap to switch pointers
(`memory/universe.cpp:315`), affordable only because it happens while a world is
being programmed, not while it runs.

Old objects that are written to are remembered, so a scavenge scans them and not
the whole old generation; the remembered set is exact rather than card-marked,
because every field write goes through `obj::record_if_old` and can do the
recording itself.

Allocation never collects: it fills the young space and asks for a collection,
which happens at a safepoint between two bytecodes, where every live reference is
reachable from the `Vm`. The interpreter lends its activation stack to the `Vm`
across anything that can re-enter it, and image load, image save and compilation
— which keep half-built graphs in Rust locals — suspend collection outright.

The arena is one allocation, sized up front (`SERF_HEAP_WORDS`), and it holds
the whole Self universe: slots, payloads, methods, activations, annotations,
identity hashes. What is meant to be left on `malloc` is the exemption list in
[MEMORY.md](MEMORY.md) — the symbol table, VM-side caches, the compiler's AST,
foreign buffers — but the interpreter has not reached that yet: a 200k-iteration
loop still makes about 1.4M of them, seven per iteration. It also runs ~1.3x
slower than the handle-table heap it replaced, which is [OPEN.md](OPEN.md).

```sh
SERF_GC=off          ./target/release/serf …   # never collect
SERF_GC_STRESS=1     ./target/release/serf …   # collect after every allocation, so
                                               # touching a missed root fails at once
SERF_GC_VERIFY=1     ./target/release/serf …   # scan every old object, not just the
                                               # remembered ones: one written without
                                               # the barrier firing fails the run
SERF_HEAP_VERIFY=1   ./target/release/serf …   # walk every space before and after
                                               # each collection
SERF_GC_STATS=1      ./target/release/serf …   # a line per collection
SERF_YOUNG_WORDS=n   ./target/release/serf …   # words per semispace (512k)
SERF_OLD_WORDS=n     ./target/release/serf …   # old generation reserve (16M words)
SERF_MEM_TRACE=1     ./target/release/serf …   # mallocs, frees and bytes at exit
```

`memory scavenge` and `memory garbageCollect` work from Self, through
`_Scavenge` and `_GarbageCollect`; both take effect at the next safepoint.

### Metrics

Every VM serves Prometheus metrics on a port the OS picks, so any number of them
can run at once, and says which on startup:

```
$ ./target/release/serf morphic.snap
serf: metrics on http://127.0.0.1:53318/metrics
```

`serf_gc_collections_total`, `serf_gc_pause_seconds` (histogram and `_max`),
`serf_gc_objects_{allocated,freed,promoted}_total`,
`serf_gc_{young,old,remembered}_objects`, `serf_gc_young_capacity_objects`,
`serf_maps_total`, `serf_switch_pointers_total`,
`serf_send_site_{hits,map_hits,misses}_total`, `serf_mem_{mallocs,frees}_total`,
`serf_mem_malloc_bytes_total`. A collection is stop-the-world — one thread, and
it only runs at a safepoint — so `serf_gc_pause_seconds` is the whole pause, not
a component of it. `SERF_METRICS=off` keeps the port shut.

## Images

`--save` and `--load` read and write the C++ VM's snapshot format: the ASCII
header, then a raw dump of the 32-bit tagged heap, the canonical string table
(20011 buckets, `hashpjw`), the 182 `VMString[]` handles and the vtbl table.
Reading rebuilds serf objects from the heap's maps — slot descriptors become
slots, `obj` slots read the object's words, a constant slot holding an assignment
object becomes an assignable slot, and method maps become serf methods whose
bytecodes the interpreter runs directly, since it is the same instruction set.
Writing goes the other way: it canonicalises a map per distinct object shape,
lays out one old space, and rebuilds the string table. The two hard sections are
empty by construction — with `Snapshot code: n` the C++ `zone::write_snapshot`
writes nothing, and `Process::write_snapshot` is a no-op there too.

### Checked against a real world

`core.snap` is an 8.4 MB image built from `objects/` by `worldBuilder.self` on a
VM compiled from `vm/`. serf reads it, walks all 185,644 objects, reaches 82,613
from the lobby, runs its methods, and writes it back out. Round-tripping it
reproduces the reachable graph exactly: same objects, slots, parent slots,
assignment slots, annotations, methods, bytecodes, literals, blocks, vectors, and
the same checksum over every integer and float. `run-tests.sh` asserts that
whenever `core.snap` is present.

That world found nine bugs the synthetic tests could not, the two that mattered
most being:

* **Assignment slots did not exist.** Self stores only the data slot and derives
  `x:` from that slot being an *obj* slot (`slotDesc::assignment_slot_name`). The
  whole world decoded with zero assignable slots, so nothing in a loaded world
  could be written to. There are 90,038 of them.
* `blockMethodMap` adds `_sourceOffset` and `_sourceLen`, so its slot descriptors
  start 11 words into the map, not 9 — which is why it overrides `slots()` in
  `codeSlotsMap.hh`. The reader desynchronised after 6,963 objects; the writer
  emitted the 9-word form, so its own images desynced too.

The rest: an arg slot's `data` is its index into the argument list, not a word
offset; every map annotation was dropped (206,214 maps carry one); mirrors,
proxies, processes and vframes were demoted to plain slots objects; method slot
flags were lost; loaded strings were written back as byte vectors, because the
string test compared against *serf's* `traits string` rather than the image's;
and each block minted a duplicate of its value method object.

The C++ VM is 32-bit i386 and cannot run here, so verification is serf's own:
`--verify-image` re-serialises a snapshot and requires the binary section back
byte for byte — gzip framing aside, so the shipped compressed images are held to
serf's writer as well: `Clean-4.4.snap`, written by the C++ VM, re-serialises to
the same 18,608,852 bytes. It then walks every space linearly the way the C++
enumeration does, computing each object's size from its map — the objects must
tile the region exactly. (That check earned its keep: it found a map word being
written over the shared assignment object.) `run-tests.sh` additionally saves a
world with methods, blocks, closures and recursion, reloads it, and runs it.

Caveats before pointing this at a real Self 4 world:

* Integers must fit Self's 30-bit `smi` and floats its 30-bit float; `--save`
  errors rather than truncating.
* A block still bound to a home activation is refused by `--save`.
* A compressed image is piped through the decompression filter its header names
  (from a short allowlist); `Snapshot code: y` images are refused on read.
* Everything serf writes is gzipped and compact, the shipped snapshots' own form,
  so saving needs `gzip` on the path.
* An image written from serf's own small world is well-formed but has nothing for
  the C++ VM to boot into — round-tripping a real image is the useful path.
* Only reachable objects are carried over, so a round trip collects garbage.
* Canonical strings with equal content are written as one object. The world holds
  ~200 such pairs; Self's own invariant says canonical strings are unique by
  content, so this merges what should already have been merged.
* A `mapOop` reachable as an ordinary value decodes to an empty object — serf has
  no maps to decode it into.

## Running the Morphic GUI

`morphic.snap` is 3.9 MB gzipped (16.7 MB of heap), 328,307 objects, 163,743
reachable from the lobby. It boots to the world's own console, and the world's
Morphic desktop draws on a real X server.

```sh
export DISPLAY=/private/tmp/com.apple.launchd.XXXXXXXX/org.xquartz:0   # XQuartz
./target/release/serf morphic.snap
```

That is all: the world comes up, prints its banner, opens its desktop and offers
its own prompt. `shots/18-morphic-autodisplay.png` is what it looks like — the
saved world's outliners, drawn by the real Morphic with real fonts. Expressions
run against that world with `--run`: `3 + 4`, `desktop open`, `paintNames at:
'black'`.

A snapshot remembers the display it was saved on, and `morphic.snap` was built in
a container against an Xvfb, so its answer is `8f40c1e90598:99.0` and it is
nobody's machine. serf falls back to `$DISPLAY` and says so; set
`SERF_DISPLAY_STRICT` to keep the world's own answer instead.

What it took, beyond the interpreter and the image reader:

* **Foreign calls.** `src/ffi.rs` resolves glue primitives against libX11 by
  name, so the 362 `_X…` primitives the image mentions dispatch generically
  rather than one by one. The generated conventions matter: `…ResultProxy:` fills
  the last argument and the unary `…ResultProxy` fills the receiver, a `_len`
  string is passed as pointer *and* length, and a NULL from a `proxy`-typed call
  is a failure. The wrappers `objects/glue/xlib_glue.cpp` compiles into the C++
  VM are in `prims.rs`.
* **Self processes.** TWAINS is *asymmetric* — the scheduler runs a process with
  `p _TWAINSResultVector: … SingleStep: … StopAt: … IfFail: …` and the process
  transfers back with `_Yield:` — so a process is a nested interpreter loop rather
  than a rewrite into symmetric coroutines. `run_stack` answers `Done` or
  `Yielded`, so a stack parks and resumes. `scheduler schedule` answers
  `schedulerProcess` when its readyQ is empty, meaning *wait for the next
  signal*: serf sleeps a tick and reports `sigio` and `sigrealtimer`, which is
  what drives the console and the timer queue.
* **`IfFail:` and doesNotUnderstand.** A primitive whose selector ends in
  `IfFail:` hands the error string to the block, as the C++ VM does for
  `primitiveNotDefinedError`, so the world routes around primitives serf lacks. A
  failed lookup is a message, not an error: the VM sends
  `undefinedSelector:Receiver:…` to the current process and the world forwards it
  to the receiver's own handler.
* **Identity hashes and canonical strings.** Self keeps an object's identity hash
  in its header so hashed collections survive a snapshot, and `string hash` is
  `canonicalize identityHash`. Deriving a hash from the address, or compiling a
  string literal to a fresh object, leaves every dictionary in the image unable to
  find its own keys.
* **The parser, handed back to the world.** `_ParseObjectFileName:ErrorObj:`
  parses a string as an object body and answers a mirror on it;
  `_MirrorEvaluate:` runs such a method with a mirror's reflectee as `self`. That
  is how every button script, editor accept and doIt in Morphic gets compiled.
  What serf compiles carries no source — its parser records no per-method spans —
  so such a method answers `''`.

Everything else stops at the first primitive serf lacks: a loaded world runs only
as far as the primitives it calls exist here. Activation mirrors are missing, so
the debugger cannot show a stack.

## Deliberately not here

* **No JIT, and maps only as a key.** Every object still carries its own slot
  descriptors; what is interned is its *shape*, which is what the send caches key
  on. A real map would share the descriptors too — see [MEMORY.md](MEMORY.md).
* **No compaction and no finalization.** The old generation is swept into a free
  list, never compacted, and nothing runs when an object dies.
* **No `{ }` slot annotations** in the parser, so the Self 4 world fileouts in
  `objects/` don't load as-is. (Annotations *in an image* round-trip fine.)
* **No `|` binary selector** — a bare `|` always closes a slot list. Use `bitOr:`.

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
