# Where X11 actually lives, and what it would take to leave

A survey of serf's coupling to X11, and two designs that follow from it: a
non-X native backend (SDL and friends), and serf compiled to wasm with Morphic
drawing on a `<canvas>`.

Everything below was measured against this tree: `morphic.snap`,
`reference/self` at `ac3faa6`, and serf built and run on Linux x86-64 with an
Xvfb.

## The short answer

serf has no GUI subsystem. It has a foreign-call layer, and the *image* has a
GUI that calls Xlib through it. The X11 dependency is a contract between two
things that are both replaceable, and the Self side of that contract was
already designed to be replaced — the Self 4 world ships four canvas
implementations, only one of which is X.

That means "target SDL" is not a port of a graphics subsystem. It is either
(a) making the ~147 Xlib entry points the image mentions resolve to something
other than libX11, or (b) filing a new canvas into the world. (a) is far less
work and is the only option that gets the *existing* `morphic.snap` desktop
onto a new backend without editing a 16.5 MB binary snapshot.

## The layers, measured

```
      ui2 morphs            morph.self, worldMorph.self, ...    0 references to X
    ------------------------------------------------------ canvas protocol (67 selectors)
      traits canvas         abstract
        xWindowCanvas  16   xPixmapCanvas  3      <- the X backend, 19 methods
        quartzBufferCanvas                        <- macOS backend (ui2/quartzCanvas.self)
        nullCanvas     35   colorRecordingCanvas 37  <- two non-graphical backends
    ------------------------------------------------------ drawable protocol
      traits xlib drawable  graphics/xlib.self, generated from glue/xlib.primMaker.hh
    ------------------------------------------------------ _X... primitives  (the serf boundary)
      src/ffi.rs            dlsym + machine-word ABI, no libffi
      src/glue_table.rs     477 entries: 148 X, 199 Quartz/ATSU, 130 libc/libm
      src/struct_table.rs   XEvent/XColor/... field offsets
      src/prims.rs          the xlib_glue.cpp wrappers, hand-written
    ------------------------------------------------------
      libX11.so
```

Facts worth having in front of you:

| | |
|---|---|
| serf source that mentions a display, window or pixel outside the FFI layer | none — `src/main.rs`, `interp.rs`, `value.rs`, `image*.rs` have zero graphics |
| X-ish primitives `morphic.snap` mentions | 366 |
| …of those, struct field accessors serf already answers itself | 212 (`XButtonEvent_xx`, `XColor_pixelpixel`, …) |
| distinct Xlib C functions behind the rest | **147** |
| Xlib calls made booting the desktop to a mapped window | **17** |
| `gc` references in `ui2/morph.self` | 0 |
| `gc` references in `ui2/canvas.self` | 45 |

The last two lines are the whole argument. X stops at the canvas. Above it,
morphs ask for `fillRectangle: r Color: c` and `drawString: s At: pt`; below
it, `xWindowCanvas` turns that into a drawable and a GC. The one leak is
`paintManager` (12 references to `xColormap`, `xAllocColor`, `xStoreColor`),
and it is gated on `aWin depth = 8` — on TrueColor the colormap machinery is
skipped.

Two more things that are already backend-agnostic and easy to miss:

* **Struct access never reaches X.** serf allocates the `XEvent` itself
  (`vec![0u8; 192]`, `src/prims.rs`) and reads fields at offsets from
  `struct_table.rs`. 212 of the 366 X primitives are those reads. Anything
  that writes plausible bytes at those offsets is indistinguishable from
  Xlib — an emulated backend needs no ABI compatibility with a real libX11,
  only agreement with serf's own table.
* **Morphic polls, it does not block.** `worldMorph` guards every
  `nextEvent` with `eventsPending > 0` (`ui2/worldMorph.self:1112, 1152,
  1962`). A backend that can only answer "here is what is queued" is enough;
  nothing needs a blocking `XNextEvent`.

## Strategy A — resolve X to something that is not X

Put a backend behind the FFI boundary instead of libX11. Nothing in the image
changes, the snapshot is untouched, and every backend (SDL, winit, wasm
canvas, headless PNG) shares one rasterizer.

serf funnels every X call through three chokepoints, so the seam is one file:

* `glue_call` — typed calls from `glue_table.rs`
* `untyped_glue` — the fallback for Xlib macros (`DefaultScreen`, `RootWindow`)
* `x_wrap` / `native_wrap` — the `xlib_glue.cpp` wrappers serf reimplements

What you would be writing is not "an X server". It is a 2D rasterizer with an
X-shaped API: rectangles, lines, arcs, polygons, text, `XImage` blits,
pixmaps, a GC holding foreground/background/function/clip/stipple, and region
algebra. That is a well-understood ~3–4 kLOC of Rust, and the realistic
working set is 60–80 of the 147 entry points, not all of them.

The parts that need thought, in descending order of nuisance:

1. **`GXxor`.** Morphic drags outlines with `XSetFunction`. A software
   framebuffer gets this exactly right; a canvas2d backend does not
   (`globalCompositeOperation: 'difference'` is not XOR on premultiplied
   RGBA). This alone argues for rasterizing in Rust and treating every
   platform as a dumb blitter.
2. **Core fonts.** `XLoadQueryFont` / `XTextWidth` / `XDrawString` and an
   `XFontStruct` with per-character metrics. Bake one or two bitmap fonts
   (6x13 "fixed" is what the world falls back to) and synthesize the struct
   in serf's own memory. The world already conjures missing font families
   through `doesNotUnderstand`, so a small set survives.
3. **Regions.** `XCreateRegion`/`XUnionRegion`/`XIntersectRegion`/… — rect-list
   algebra, a couple hundred lines.
4. **Stipples, tiles, plane masks, clip masks.** Used by the paint layer;
   straightforward once you own the pixel loop.

Then a platform shell needs exactly three things: make a window, blit a 32-bit
ARGB framebuffer with a dirty rect, deliver key/mouse/resize events. That is
~300 lines per platform and makes SDL, winit+softbuffer, a Linux framebuffer
and a browser canvas interchangeable.

**On SDL specifically:** you do not need a new dependency. `src/ffi.rs`
already resolves and calls arbitrary C functions by name, so an SDL backend is
`dlopen("libSDL2-2.0.so.0")` plus a list of symbol names — `SDL_Init`,
`SDL_CreateWindow`, `SDL_CreateTexture`, `SDL_UpdateTexture`,
`SDL_RenderCopy`, `SDL_PollEvent`. serf keeps its zero-dependency Cargo.toml
and gains a backend at runtime. If you would rather have a pure-Rust window,
`winit` + `softbuffer` is the same shell with two crates instead.

Validation is already sitting in the repo: `shots/` holds eighteen golden
images and `shot.py` renders an `xwd` dump to PNG with the stdlib. A software
backend can render those same scenes headlessly, with no X server, and diff
against them — a better test than the current one, which needs an Xvfb.

## Strategy B — a new canvas in Self

Write `sdlCanvas.self` the way `ui2/quartzCanvas.self` (455 lines) was written,
plus a glue primitive set. This is what the Self authors would do and it is
architecturally correct: the world would then have X, Quartz and SDL backends
selected the way `traits unixFile osVariants` selects a platform's fcntl
constants.

The cost is not the canvas — `nullCanvas` shows a complete backend is ~35
methods. The cost is getting new code into a snapshot. Self module fileins go
through `bootstrap addSlotsTo:` and `{ }` slot annotations, and serf's parser
handles neither (README, "Deliberately not here"). You would need annotation
support in the parser, or a hand-written bootstrap-free module, and you would
still have to rewire `worldMorph`, `x11Globals` and the event translation.

Worth doing eventually. Not the way to get a picture on screen this month.

## Strategy C — a graphics story for serf's own world

`self/init.self` has no GUI at all; `self/x11-demo.self` pokes Xlib directly.
Once Strategy A exists there is a portable rasterizer in the VM, so a dozen
`_Gfx…` primitives and a small Self canvas give serf's own world a GUI that
was never X-shaped to begin with — and that is the substrate Strategy B would
file its module against.

## Recommended order

0. **Build on Linux.** `src/prims.rs` linked `__error`, which is the
   BSD/macOS errno symbol; the Linux build failed to link. Fixed in this
   branch (`__errno_location` under `cfg`), after which
   `DISPLAY=:99 serf self/x11-demo.self` draws under Xvfb on x86-64.
1. **One seam, no behaviour change.** Route the three chokepoints through a
   `Backend` trait whose only implementation is `X11Passthrough` (today's
   dlsym path). Prove it with `run-tests.sh` and the shots.
2. **`SoftX`**, the software rasterizer, validated headlessly against
   `shots/` with no X server in the loop.
3. **Platform shells**: dlopen'd SDL2 or winit+softbuffer.
4. **wasm.**

## Morphic in a browser

serf is unusually close to this. It is safe Rust with zero crates, its
interpreter keeps activations in a `Vec<Frame>` rather than on the Rust stack,
and `Snapshot::parse` takes a `&[u8]`. The blockers are specific and countable.

### What breaks, and what fixes it

**1. `dlopen`/`dlsym` do not exist.** `src/ffi.rs`'s entire mechanism is gone,
and its replacement cannot be a mechanical one: `call_arity!` transmutes a
symbol to a `fn` of the right arity, and wasm type-checks every
`call_indirect` at runtime, so a mismatched signature traps rather than
working by ABI accident. The registry must be a compile-time table of
`fn(&mut Vm, &[u64]) -> u64` — one uniform signature, arity handled inside.
This is the largest mechanical change and it is confined to `ffi.rs` plus the
`glue_call` dispatch.

**2. Nothing may block.** Three places do: `_Sleep:`, the 10 ms sleep in the
TWAINS idle path (`_TWAINSResultVector:SingleStep:StopAt:` when the scheduler
has an empty readyQ), and the REPL's `read_line`. The fix is already
half-written: `P::Yield` parks a Self stack, pushes the resume value onto the
frame, and returns `Outcome::Yielded` out of `run_stack` (`src/interp.rs:469`).
Add `P::Pause` / `Outcome::Paused` alongside it — the same ~20 lines — and the
host resumes the identical `Vec<Frame>` on the next `requestAnimationFrame`.
The idle path is the shallow case (`vm.current_proc.is_none()`, Rust stack is
`main → run_stack → prims`); the nested case, where the scheduler is running
another process, has to propagate the pause through
`_TWAINSResultVector:` the way it already propagates `Yielded`.

**3. Host services.** `SystemTime::now`, `localtime`/`gmtime`, `select`,
`read`/`write`/`fcntl` on descriptors, `std::fs::read`, and the
`Command::new(filter)` used to decompress gzipped snapshots. Import `Date.now`
from JS, implement civil-time conversion in Rust (~40 lines), give the world a
virtual tty backed by a JS terminal, hand the snapshot in as bytes, and refuse
compressed images (or bundle an inflate).

**4. Drawing.** The emulated X writes into a framebuffer inside wasm linear
memory. Export the pointer and a dirty rect; JS wraps it without copying —
`new ImageData(new Uint8ClampedArray(memory.buffer, ptr, len))` — and
`putImageData`s it. XOR, stipples and plane masks all work because the
rasterizer is ours.

**5. Input.** JS pushes DOM events into a ring buffer in linear memory;
emulated `XPending`/`XNextEvent` drain it and write `XEvent` bytes at the
offsets `struct_table.rs` names. Keysyms are the one table to get right —
ASCII keysyms are ASCII, and the named ones (`_Alt_L`, `_KP_Add`, `_Page_Up`,
…) are a fixed list that `graphics/xlib.self` already enumerates.

**6. Weight.** Loading `morphic.snap` costs 2.0 s and 224 MB peak RSS on
x86-64; on wasm32, with `Rc` pointers halved, expect roughly 130–160 MB, plus
a 16.5 MB download (≈5 MB gzipped). Both are survivable. The real limit is
that serf has no GC — `Rc` everywhere and the world is full of cycles — so a
live Morphic session in a tab leaks until it dies. A browser Morphic makes the
collector the next thing worth building, not the JIT.

### Toolchain

Target `wasm32-unknown-unknown` with raw `#[no_mangle] extern "C"` exports and
`extern "C"` imports, and a ~100-line JS shim. No `wasm-bindgen`, no crates —
the zero-dependency property survives, which would be a shame to lose over a
`<canvas>`.

### The routes not taken

* **X11 wire protocol over a WebSocket** to a JS-side X server (the Broadway /
  xpra shape). Keeps libX11 semantics perfectly, but writing an X server in JS
  is strictly more work than emulating the ~80-call client API in Rust, and it
  puts a socket in the middle of every repaint.
* **`wasm32-wasip1` under a runtime with a virtual X.** Fine for headless
  rendering and CI golden-image tests; does not get you into a tab.

## A portability note found on the way

serf built and ran the X demo on Linux after the `__error` fix, but
`morphic.snap` boots to a mapped window and then stalls. The trace shows why,
and it is not a serf bug:

```
ffi int_or_errno fcntl([3, 4, 40]) -> 0x0          F_SETFL, O_ASYNC = 0x40
ffi int_or_errno fcntl([3, 6, 3926]) -> 0xffffffff F_SETOWN = 6, pid
```

Those are the BSD constants. On Linux `F_SETOWN` is 8 and `O_ASYNC` is
0x2000, so `fcntl(fd, 6, pid)` is `F_SETLK` with a garbage lock pointer and
the console never goes async. The world has the right numbers —
`core/unix.self:4478` defines `f_setown = 8` under
`traits unixFile osVariants linux` — but the saved image picked the BSD
variant, presumably from where it was built.

Which is the same lesson as the graphics: the platform coupling that looks
like it belongs to the VM is mostly in the image, chosen at runtime by an
object the world can be told to pick differently.
