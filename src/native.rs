//! The world's X calls, answered here instead of by an X server.
//!
//! The default on macOS, and `SERF_BACKEND=native` anywhere else, puts this in
//! front of the FFI path in `prims.rs`. The world's own code does not change
//! and neither does the image: the glue primitives it calls are the same, the
//! C structs it pokes at are the same bytes, and only what sits behind
//! `XFillRectangle` is different.
//!
//! Two things carry most of the weight:
//!
//! * **The struct paths already work.** `XEvent_new` mallocs into `vm.c_heap`
//!   and the field accessors in `prims.rs` read at `struct_table` offsets, so
//!   an event filled in by `events::encode` needs no special case at all. The
//!   same trick answers `XLoadQueryFont`: hand back a real `XFontStruct` and
//!   the world's `XFontStruct_ascentascent` finds what it expects.
//! * **Everything else is a handle.** A `Display`, `Screen`, `GC`, `Colormap`
//!   or drawable is an integer the world only ever hands back, so it is tagged
//!   and indexed rather than allocated. Drawables are one store, because a
//!   window, a pixmap and an `XImage` are the same rectangle of pixels.
//!
//! An X call this does not implement is an error rather than a fall-through to
//! `dlsym`: on a host that has libX11, passing it one of these handles would
//! not fail, it would crash. The world's `IfFail:` routes around the error.

use std::collections::HashMap;
use std::io::{PipeReader, PipeWriter, Write};
use std::time::Duration;

use crate::canvas::{alloc_color, Canvas, Drawables, Func, Gc, Rect};
use crate::events;
use crate::text::Fonts;
use crate::value::Vm;
use crate::window::Window;

const TAG: u64 = 0x5E1F_0000_0000_0000;
const DISPLAY: u64 = TAG | 1 << 32;
const SCREEN: u64 = TAG | 2 << 32;
const DRAWABLE: u64 = TAG | 3 << 32;
const GC: u64 = TAG | 4 << 32;
const COLORMAP: u64 = TAG | 5 << 32;
const VISUAL: u64 = TAG | 6 << 32;
const CURSOR: u64 = TAG | 7 << 32;
const ATOM: u64 = TAG | 8 << 32;
const FONT: u64 = TAG | 9 << 32;

fn handle(kind: u64, i: usize) -> u64 {
    kind | (i as u64 + 1)
}

fn index(h: u64) -> usize {
    (h & 0xFFFF_FFFF).saturating_sub(1) as usize
}

/// X's `GXcopy` is 3 and `GXxor` is 6 (`X.h`); everything else this draws as a
/// copy, which is what a server without the raster ops would do too.
fn func_of(f: u64) -> Func {
    if f == 6 {
        Func::Xor
    } else {
        Func::Copy
    }
}

pub struct Native {
    pub window: Option<Window>,
    fonts: Fonts,
    d: Drawables,
    /// the window's drawable id, once it has one
    win: Option<usize>,
    /// real pixels per logical one, learned from the window. Every drawable is
    /// made at it, so a blit between any two of them is a straight copy.
    scale: i32,
    /// what `XSelectInput` last asked for, so `XGetWindowAttributes` can say
    selected: u64,
    /// `SERF_CLICK=x,y[@seconds]`: clicks to make once the world is up, so a
    /// misbehaving one can be reproduced without a hand on the mouse. Only the
    /// world can be clicked, and only a person can click it -- which makes a
    /// bug in what a click does the one kind this cannot chase on its own.
    ///
    /// Spread over time on purpose. A real click is a press, a pause, and a
    /// release, and the world sees each in a different turn of its own loop;
    /// firing them into the queue together is a different gesture.
    clicks: Vec<(std::time::Instant, crate::events::Event)>,
    gcs: Vec<Gc>,
    atoms: Vec<String>,
    /// Font handles are indices into `fonts`, kept so `XSetFont` can find one
    /// from the `fid` it read out of an `XFontStruct`.
    font_of: HashMap<u64, usize>,
    /// The world selects on the display's connection to wait for input. There
    /// is no socket here, so this is a pipe: a byte goes in whenever an event
    /// arrives, which is exactly the wakeup a real connection would give.
    pipe: Option<(PipeReader, PipeWriter)>,
    /// how many bytes are in the pipe, so it is drained rather than filled for
    /// ever by a world that never reads it
    armed: bool,
}

impl Native {
    pub fn new() -> Native {
        Native {
            window: None,
            fonts: Fonts::new(),
            d: Drawables::default(),
            win: None,
            scale: 1,
            selected: 0,
            clicks: scripted_clicks(),
            gcs: vec![],
            atoms: vec![],
            font_of: HashMap::new(),
            pipe: std::io::pipe().ok(),
            armed: false,
        }
    }

    fn gc(&self, h: u64) -> Gc {
        self.gcs.get(index(h)).cloned().unwrap_or_default()
    }

    /// Let the platform deliver, then match the pipe to what is waiting.
    fn pump(&mut self) {
        let Some(w) = self.window.as_mut() else { return };
        w.pump(Duration::from_millis(0));
        while self.clicks.first().is_some_and(|(t, _)| std::time::Instant::now() >= *t) {
            let (_, e) = self.clicks.remove(0);
            eprintln!("serf: scripted {:?}", e);
            self.window.as_mut().unwrap().events().push(e);
        }
        let w = self.window.as_mut().unwrap();
        let n = w.events().pending();
        let Some((_, tx)) = self.pipe.as_mut() else { return };
        if n > 0 && !self.armed {
            let _ = tx.write(b"x");
            self.armed = true;
        }
    }

    fn queue_empty(&mut self) -> bool {
        self.window.as_mut().is_none_or(|w| w.events().pending() == 0)
    }
}

/// Names this answers. Everything X-shaped is claimed even when it is not
/// implemented, because the alternative is handing a synthetic handle to a real
/// Xlib that happens to be installed -- which does not fail, it crashes.
pub fn claims(cname: &str) -> bool {
    cname.starts_with('X')
        || cname.ends_with("OfScreen")
        || cname.ends_with("OfDisplay")
        || matches!(
            cname,
            "ConnectionNumber" | "DefaultScreen" | "RootWindow" | "DefaultRootWindow"
        )
}

/// One field of a C struct the world allocated, at the offset and width
/// `struct_table` records. The same table the field-accessor primitives read
/// through, so what this writes is exactly what the world's own read finds.
fn field(strukt: &str, name: &str) -> Option<(usize, usize)> {
    crate::struct_table::FIELD.iter().find(|e| e.1 == strukt && e.2 == name).map(|e| (e.3, e.4))
}

fn get_at(p: u64, strukt: &str, name: &str) -> u64 {
    let Some((at, n)) = field(strukt, name) else { return 0 };
    if p == 0 {
        return 0;
    }
    let mut w = [0u8; 8];
    unsafe { std::ptr::copy_nonoverlapping((p as *const u8).add(at), w.as_mut_ptr(), n) };
    u64::from_le_bytes(w)
}

fn put_at(p: u64, strukt: &str, name: &str, v: u64) {
    let Some((at, n)) = field(strukt, name) else { return };
    if p == 0 {
        return;
    }
    unsafe { std::ptr::copy_nonoverlapping(v.to_le_bytes().as_ptr(), (p as *mut u8).add(at), n) };
}

fn cstr(p: u64) -> String {
    if p == 0 {
        return String::new();
    }
    let mut b = vec![];
    unsafe {
        let mut q = p as *const u8;
        while *q != 0 && b.len() < 4096 {
            b.push(*q);
            q = q.add(1);
        }
    }
    String::from_utf8_lossy(&b).into_owned()
}

fn bytes(p: u64, n: u64) -> Vec<u8> {
    if p == 0 {
        return vec![];
    }
    unsafe { std::slice::from_raw_parts(p as *const u8, n as usize).to_vec() }
}

/// Answer one X call. `words` are the marshalled arguments, in the order the
/// glue table declares them -- and words are all of it: every glue call that
/// deals in Self objects goes through `native_wrap` instead, so nothing here
/// has to know what an oop is.
pub fn call(vm: &mut Vm, cname: &str, words: &[u64]) -> Result<u64, String> {
    // lifted out and put back, so the body can reach `vm.c_heap` as well --
    // XLoadQueryFont hands the world a struct that has to outlive the call
    let mut n = vm.native.take().ok_or("no native backend")?;
    let r = answer(vm, &mut n, cname, words);
    vm.native = Some(n);
    if std::env::var_os("SERF_TRACE_FFI").is_some() {
        match &r {
            Ok(v) => eprintln!("native {}({:x?}) -> {:#x}", cname, words, v),
            Err(e) => eprintln!("native {}({:x?}) -> {}", cname, words, e),
        }
    }
    r
}

fn answer(vm: &mut Vm, n: &mut Native, cname: &str, words: &[u64]) -> Result<u64, String> {
    let w = |i: usize| words.get(i).copied().unwrap_or(0);
    let iw = |i: usize| w(i) as u32 as i32;

    // XFree, and the per-struct `XFree_XSizeHints_wrap` shims beside it. What
    // was handed to the world lives in `vm.c_heap` for the life of the VM, so
    // there is nothing here to free and nothing that can dangle.
    if cname.starts_with("XFree") {
        return Ok(0);
    }

    Ok(match cname {
        // --- display and screen ------------------------------------------
        "XOpenDisplay" => DISPLAY,
        "XCloseDisplay" | "XSetErrorHandler" | "XSetIOErrorHandler" | "XSynchronize" => 0,
        "DefaultScreenOfDisplay" | "ScreenOfDisplay" => SCREEN,
        "DefaultScreen" => 0,
        "DefaultDepthOfScreen" | "DefaultDepth" => 24,
        "DefaultVisualOfScreen" | "DefaultVisual" => VISUAL,
        "DefaultColormapOfScreen" | "DefaultColormap" => COLORMAP,
        "DefaultGCOfScreen" | "DefaultGC" => {
            if n.gcs.is_empty() {
                n.gcs.push(Gc::default());
            }
            handle(GC, 0)
        }
        "BlackPixelOfScreen" | "BlackPixel" => 0x00_0000,
        "WhitePixelOfScreen" | "WhitePixel" => 0xFF_FFFF,
        "RootWindowOfScreen" | "RootWindow" | "DefaultRootWindow" => {
            handle(DRAWABLE, n.win.unwrap_or(0))
        }
        "ConnectionNumber" => {
            use std::os::fd::AsRawFd;
            n.pipe.as_ref().map_or(0, |(r, _)| r.as_raw_fd() as u64)
        }
        "XFlush" | "XSync" => {
            if let Some(win) = n.window.as_mut() {
                win.present();
                // SERF_SHOT writes what was just put on the screen, so a run
                // that draws can be checked without anything to look at it
                if let Some(path) = std::env::var_os("SERF_SHOT") {
                    let _ = crate::canvas::write_png(&path.to_string_lossy(), win.canvas());
                }
            }
            n.pump();
            0
        }

        // --- the window --------------------------------------------------
        "XCreateSimpleWindow" => {
            // winit allows one event loop per process, so a world that asks for
            // a second window -- which Morphic does the moment it opens a
            // debugger -- gets the one that already exists rather than an
            // error. Two real windows need the loop lifted out of `Window`.
            if let Some(id) = n.win {
                handle(DRAWABLE, id)
            } else {
                let (width, height) = (w(4).max(1) as i32, w(5).max(1) as i32);
                let bg = w(8) as u32;
                let win = Window::open("Self", width, height, bg)
                    .map_err(|e| format!("primitiveFailedError: {}", e))?;
                n.window = Some(win);
                let id = n.d.add(Canvas::new(0, 0, bg)); // a slot, not the pixels
                n.win = Some(id);
                n.pump(); // the window exists only once winit has been pumped
                          // and only then does it know the display's scale. Fonts have to
                          // hear before any is loaded, since it is baked into the size
                          // each one is asked for.
                n.scale = n.window.as_ref().map_or(1, |w| w.scale());
                n.fonts.set_scale(n.scale as f32);
                handle(DRAWABLE, id)
            }
        }
        "XMapWindow"
        | "XMapRaised"
        | "XUnmapWindow"
        | "XDestroyWindow"
        | "XRaiseWindow"
        | "XLowerWindow"
        | "XSetWMName"
        | "XSetWMNormalHints"
        | "XStoreName"
        | "XSetWMHints"
        | "XSetWMProtocol_wrap"
        | "XChangeWindowAttributes_wrap"
        | "XDefineCursor"
        | "XUndefineCursor"
        | "XFreeCursor"
        | "XBell"
        | "XWarpPointer"
        | "XGrabPointer"
        | "XUngrabPointer"
        | "XUngrabButton"
        | "XGrabButton"
        | "XSetInputFocus"
        | "XCirculateSubwindows"
        | "XCirculateSubwindowsUp"
        | "XCirculateSubwindowsDown" => {
            n.pump();
            0
        }
        "XSelectInput" => {
            n.selected = w(2);
            if let Some(win) = n.window.as_mut() {
                win.events().select(w(2));
            }
            0
        }
        "XInternAtom" => {
            let name = cstr(w(1));
            let at = n.atoms.iter().position(|a| *a == name).unwrap_or_else(|| {
                n.atoms.push(name);
                n.atoms.len() - 1
            });
            handle(ATOM, at)
        }
        "XCreateFontCursor" | "XCreatePixmapCursor" => handle(CURSOR, 0),
        // The window-manager hints. There is no window manager behind this, so
        // what the world fills in is never read -- but it must have somewhere
        // to fill in, and a `proxy` return that is null is a failure.
        "XAllocSizeHints"
        | "XAllocWMHints"
        | "XAllocClassHint"
        | "XAllocIconSize"
        | "XAllocStandardColormap"
        | "XMatchVisualInfo_wrap" => {
            let mem = vec![0u8; 256];
            let p = mem.as_ptr() as u64;
            vm.c_heap.push(mem);
            p
        }
        // Not a blank to be filled in: the world reads the window's own
        // geometry back out of this, and a zeroed struct tells it the window is
        // nothing by nothing -- which it then lays morphs out against.
        "XGetWindowAttributes_wrap" => {
            let mem = vec![0u8; 256];
            let p = mem.as_ptr() as u64;
            vm.c_heap.push(mem);
            let (width, height) = n.window.as_ref().map_or((0, 0), |w| w.size());
            let put = |f: &str, v: u64| put_at(p, "XWindowAttributes", f, v);
            // the window sits at the root's origin: there is one of it and no
            // window manager to have moved it, which is the same fiction the
            // event coordinates are reported under
            put("x", 0);
            put("y", 0);
            put("width", width as u64);
            put("height", height as u64);
            put("border_width", 0);
            put("depth", 24);
            put("visual", VISUAL);
            put("root", handle(DRAWABLE, n.win.unwrap_or(0)));
            put("colormap", COLORMAP);
            put("screen", SCREEN);
            put("c_class", 1); // InputOutput
            put("map_state", 2); // IsViewable
            put("all_event_masks", n.selected);
            put("your_event_mask", n.selected);
            p
        }
        // The world frees the property's `value` afterwards, so it has to point
        // at something: a copy of the string, kept alive with the rest of what
        // has been handed to foreign code.
        "XStringToTextProperty_wrap" => {
            let mut text = cstr(w(1)).into_bytes();
            text.push(0);
            let p = text.as_ptr() as u64;
            vm.c_heap.push(text);
            put_at(w(0), "XTextProperty", "value", p);
            1
        }
        "XShapeQueryExtension_wrap" => 0,

        // --- graphics contexts -------------------------------------------
        "XCreateGC" => {
            n.gcs.push(Gc::default());
            handle(GC, n.gcs.len() - 1)
        }
        "XFreeGC"
        | "XSetGraphicsExposures"
        | "XSetFillStyle"
        | "XSetFillRule"
        | "XSetTile"
        | "XSetStipple"
        | "XSetDashes"
        | "XSetSubwindowMode"
        | "XSetBackground"
        | "XChangeGC"
        | "XGetGCValues_wrap"
        | "XCopyGC" => 0,
        "XSetForeground" => {
            let g = index(w(1));
            if let Some(g) = n.gcs.get_mut(g) {
                g.fg = w(2) as u32 & 0xFF_FFFF;
            }
            0
        }
        "XSetFunction" => {
            let g = index(w(1));
            let f = func_of(w(2));
            if let Some(g) = n.gcs.get_mut(g) {
                g.func = f;
            }
            0
        }
        "XSetLineAttributes" => {
            let g = index(w(1));
            if let Some(g) = n.gcs.get_mut(g) {
                g.line_width = (w(2) as i32).max(1);
            }
            0
        }
        "XSetFont" => {
            let (g, f) = (index(w(1)), n.font_of.get(&w(2)).copied());
            if let Some(g) = n.gcs.get_mut(g) {
                g.font = f;
            }
            0
        }
        "XSetClipMask" => {
            let g = index(w(1));
            if let Some(g) = n.gcs.get_mut(g) {
                g.clear_clip();
            }
            0
        }
        "XSetClipOrigin" => {
            let g = index(w(1));
            let o = (iw(2), iw(3));
            if let Some(g) = n.gcs.get_mut(g) {
                g.clip_origin = o;
            }
            0
        }
        "XSetClipRectangle_wrap" => {
            let g = index(w(1));
            let r = Rect { x: iw(2), y: iw(3), w: w(4) as i32, h: w(5) as i32 };
            if let Some(g) = n.gcs.get_mut(g) {
                g.clip = Some(r);
                g.clip_origin = (0, 0);
            }
            0
        }

        // --- colour: a truecolor visual's colormap is the identity --------
        "XAllocColor" => {
            let c = w(2);
            let (r, g, b) = (
                get_at(c, "XColor", "red"),
                get_at(c, "XColor", "green"),
                get_at(c, "XColor", "blue"),
            );
            let px = alloc_color(r as u16, g as u16, b as u16);
            put_at(c, "XColor", "pixel", px as u64);
            1
        }
        "XFreeColors" | "XStoreColor" | "XInstallColormap" => 0,

        // --- drawables ----------------------------------------------------
        "XCreatePixmap" => {
            let (width, height) = (w(2).max(1) as i32, w(3).max(1) as i32);
            handle(DRAWABLE, n.d.add(Canvas::scaled(width, height, 0, n.scale)))
        }
        "XCreateImage_wrap" => {
            let (width, height) = (w(4).max(1) as i32, w(5).max(1) as i32);
            handle(DRAWABLE, n.d.add(Canvas::scaled(width, height, 0, n.scale)))
        }
        "XCreateBitmapFromData" => {
            let (width, height) = (w(3).max(1) as i32, w(4).max(1) as i32);
            let src = bytes(w(2), (width as u64 + 7) / 8 * height as u64);
            let mut c = Canvas::scaled(width, height, 0, n.scale);
            for y in 0..height {
                for x in 0..width {
                    let bit = src
                        .get((y * ((width + 7) / 8) + x / 8) as usize)
                        .is_some_and(|b| b >> (x % 8) & 1 == 1);
                    c.put(x, y, if bit { 0xFF_FFFF } else { 0 });
                }
            }
            handle(DRAWABLE, n.d.add(c))
        }
        "XFreePixmap" | "XDestroyImage" | "XFree" => 0,
        "XPutPixel" => {
            let (x, y, v) = (iw(1), iw(2), w(3) as u32);
            if let Some(c) = canvas_of(n, w(0)) {
                c.put(x, y, v & 0xFF_FFFF);
            }
            0
        }
        "XGetPixel" => {
            let (x, y) = (iw(1), iw(2));
            canvas_of(n, w(0)).map_or(0, |c| c.get(x, y) as u64)
        }
        "XGetImage" | "XGetSubImage" => {
            let (x, y, width, height) = (iw(2), iw(3), w(4) as i32, w(5) as i32);
            let mut c = Canvas::scaled(width, height, 0, n.scale);
            if let Some(src) = canvas_of(n, w(1)) {
                // real pixels, so a grab of a region that holds text keeps it
                let k = src.scale.min(c.scale);
                for j in 0..height * k {
                    for i in 0..width * k {
                        c.set_at(i, j, src.at(x * k + i, y * k + j));
                    }
                }
            }
            handle(DRAWABLE, n.d.add(c))
        }

        // --- drawing ------------------------------------------------------
        "XDrawPoint" => draw(n, w(1), w(2), |c, g| c.point(g, iw(3), iw(4))),
        "XDrawLine" => draw(n, w(1), w(2), |c, g| c.line(g, iw(3), iw(4), iw(5), iw(6))),
        "XDrawRectangle" => {
            draw(n, w(1), w(2), |c, g| c.draw_rect(g, iw(3), iw(4), w(5) as i32, w(6) as i32))
        }
        "XFillRectangle" => {
            draw(n, w(1), w(2), |c, g| c.fill_rect(g, iw(3), iw(4), w(5) as i32, w(6) as i32))
        }
        "XDrawArc" => draw(n, w(1), w(2), |c, g| {
            c.arc(g, iw(3), iw(4), w(5) as i32, w(6) as i32, iw(7), iw(8), false)
        }),
        "XFillArc" => draw(n, w(1), w(2), |c, g| {
            c.arc(g, iw(3), iw(4), w(5) as i32, w(6) as i32, iw(7), iw(8), true)
        }),
        "XClearArea" => {
            let (x, y, width, height) = (iw(2), iw(3), w(4) as i32, w(5) as i32);
            if let Some(c) = canvas_of(n, w(1)) {
                c.clear_area(x, y, width, height);
            }
            0
        }
        "XCopyArea" | "XCopyPlane" => {
            let g = n.gc(w(3));
            let (sx, sy, width, height, dx, dy) =
                (iw(4), iw(5), w(6) as i32, w(7) as i32, iw(8), iw(9));
            blit(n, w(1), sx, sy, width, height, w(2), dx, dy, &g);
            0
        }
        // XPutImage names its image where XCopyArea names its source
        "XPutImage" => {
            let g = n.gc(w(2));
            let (sx, sy, dx, dy, width, height) =
                (iw(4), iw(5), iw(6), iw(7), w(8) as i32, w(9) as i32);
            blit(n, w(3), sx, sy, width, height, w(1), dx, dy, &g);
            0
        }

        // --- fonts --------------------------------------------------------
        "XLoadQueryFont" | "XLoadQueryFont_wrap" | "XQueryFont" => {
            let name = cstr(w(1));
            let f = n.fonts.load(&name);
            let (a, d) = n.fonts.metrics(f);
            // a real XFontStruct, because the world reads its fields itself
            let mem = vec![0u8; 128];
            let p = mem.as_ptr() as u64;
            let fid = handle(FONT, f);
            n.font_of.insert(fid, f);
            vm.c_heap.push(mem);
            put_at(p, "XFontStruct", "fid", fid);
            put_at(p, "XFontStruct", "min_char_or_byte2", 0);
            put_at(p, "XFontStruct", "max_char_or_byte2", 255);
            // NULL per_char is what forces every width through XTextWidth,
            // which is the only one of these that is measured rather than
            // guessed -- see the note in src/text.rs
            put_at(p, "XFontStruct", "per_char", 0);
            put_at(p, "XFontStruct", "ascent", a as u64);
            put_at(p, "XFontStruct", "descent", d as u64);
            // max_bounds is an XCharStruct at offset 68, and `maxCharWidth`,
            // `maxAscent`, `maxDescent` and `perCharWidth` in prims.rs read it
            // directly. With per_char null it is every character's width, so a
            // proportional face has to answer its widest rather than its mean.
            let widest = b"MW@_".iter().map(|c| n.fonts.width(f, &[*c])).max().unwrap_or(0);
            unsafe {
                let cs = (p + 68) as *mut i16;
                *cs.add(2) = widest as i16; // width
                *cs.add(3) = a as i16; // ascent
                *cs.add(4) = d as i16; // descent
            }
            p
        }
        "XFreeFont" | "XFreeFontInfo" | "XFreeFontNames" => 0,
        "XTextWidth" => {
            let fid = get_at(w(0), "XFontStruct", "fid");
            let s = bytes(w(1), w(2));
            match n.font_of.get(&fid).copied() {
                Some(f) => n.fonts.width(f, &s) as u64,
                None => 0,
            }
        }
        "XDrawString" | "XDrawImageString" => {
            let g = n.gc(w(2));
            let (x, y, s) = (iw(3), iw(4), bytes(w(5), w(6)));
            let id = index(w(1));
            // the fonts and the canvas are different fields of `n`, so they can
            // be borrowed at once -- `canvas_of` would take the whole of it
            let fonts = &mut n.fonts;
            if n.win == Some(id) {
                if let Some(win) = n.window.as_mut() {
                    fonts.draw(win.canvas(), &g, x, y, &s);
                }
            } else if n.d.has(id) {
                fonts.draw(n.d.get_mut(id), &g, x, y, &s);
            }
            0
        }

        // --- events -------------------------------------------------------
        "XPending" | "XEventsQueued" | "XQLength" => {
            n.pump();
            let q = n.window.as_mut().map_or(0, |w| w.events().pending());
            if q == 0 {
                drain(n);
            }
            q as u64
        }
        "XNextEvent" | "XPeekEvent" => {
            let peek = cname == "XPeekEvent";
            if !wait_for_event(n) {
                return Err("primitiveFailedError: no event".into());
            }
            let ev = w(1);
            let buf = unsafe { std::slice::from_raw_parts_mut(ev as *mut u8, events::EVENT_BYTES) };
            let win = n.window.as_mut().unwrap();
            if peek {
                win.events().peek_into(buf);
            } else {
                win.events().next_into(buf);
            }
            drain(n);
            0
        }
        "XCheckMaskEvent" | "XCheckTypedEvent" => {
            n.pump();
            let ev = w(2);
            let buf = unsafe { std::slice::from_raw_parts_mut(ev as *mut u8, events::EVENT_BYTES) };
            let got = match n.window.as_mut() {
                Some(win) if cname == "XCheckMaskEvent" => win.events().check_mask(w(1), buf),
                Some(win) => win.events().check_typed(w(1) as i64, buf),
                None => false,
            };
            drain(n);
            u64::from(got)
        }
        "XPutBackEvent" => {
            let buf = bytes(w(1), events::EVENT_BYTES as u64);
            if let Some(win) = n.window.as_mut() {
                win.events().put_back(&buf);
            }
            0
        }
        _ => {
            return Err(format!(
                "primitiveNotDefinedError: '{}' is not in the native backend",
                cname
            ))
        }
    })
}

/// Wait a little for an event, and answer whether one turned up.
///
/// X's `XNextEvent` blocks until there is one. This cannot: the interpreter is
/// the same thread that runs Self's scheduler, its processes and the world's
/// own console, so blocking in here stops the world rather than just the
/// process that asked. Waiting briefly and then failing hands the world's
/// `IfFail:` the decision, which is what it does with every other primitive
/// serf cannot answer.
///
/// ponytail: the wait is one frame. Long enough that an event already on its
/// way is not missed, short enough that a world polling an empty queue keeps
/// running; a real X connection would sleep in poll() until data arrived.
fn wait_for_event(n: &mut Native) -> bool {
    if n.window.is_none() {
        return false;
    }
    let deadline = std::time::Instant::now() + Duration::from_millis(16);
    while n.queue_empty() {
        if std::time::Instant::now() >= deadline {
            return false;
        }
        n.window.as_mut().unwrap().pump(Duration::from_millis(2));
    }
    true
}

/// An image's size, and one of its pixels, for `XImagePutData_wrap` in
/// `prims.rs` -- which walks an image a pixel at a time and would otherwise
/// dispatch through a function pointer read out of an Xlib `XImage`.
///
/// ponytail: `image_get` answers the low byte of the pixel, because the caller
/// is filling a byte vector of colour indices. That is what the Xlib path does
/// with a depth-24 image too; a world that wanted the whole pixel back would
/// need a wider call than the one it is making.
pub fn image_size(vm: &mut Vm, img: u64) -> Option<(i32, i32)> {
    let n = vm.native.as_mut()?;
    canvas_of(n, img).map(|c| (c.w, c.h))
}

pub fn image_put(vm: &mut Vm, img: u64, x: i32, y: i32, v: u32) {
    let Some(n) = vm.native.as_mut() else { return };
    if let Some(c) = canvas_of(n, img) {
        c.put(x, y, v & 0xFF_FFFF);
    }
}

pub fn image_get(vm: &mut Vm, img: u64, x: i32, y: i32) -> u32 {
    let Some(n) = vm.native.as_mut() else { return 0 };
    canvas_of(n, img).map_or(0, |c| c.get(x, y))
}

/// The two point-list calls, for `native_wrap` -- their coordinates arrive as
/// Self vectors, so they never reach the word path.
pub fn polygon(vm: &mut Vm, fill: bool, d: u64, gc: u64, xs: &[i64], ys: &[i64]) {
    let Some(n) = vm.native.as_mut() else { return };
    let (xs, ys): (Vec<i32>, Vec<i32>) =
        (xs.iter().map(|v| *v as i32).collect(), ys.iter().map(|v| *v as i32).collect());
    let g = n.gc(gc);
    if let Some(c) = canvas_of(n, d) {
        if fill {
            c.fill_polygon(&g, &xs, &ys);
        } else {
            c.lines(&g, &xs, &ys);
        }
    }
}

/// `XLookupString` for `native_wrap`, which owns the Self side of that call.
pub fn lookup_string_at(evt: u64) -> (Vec<u8>, u32) {
    events::lookup_string(&bytes(evt, events::EVENT_BYTES as u64))
}

/// Fill the world's `XEvent` from the queue, for `XNextEvent_wrap` -- which
/// answers a Self object and so cannot come through `call`. Blocks the way X
/// does, pumping rather than spinning.
pub fn next_event_into(vm: &mut Vm, buf: u64, peek: bool) -> Result<(), String> {
    let mut n = vm.native.take().ok_or("no native backend")?;
    let r = (|| {
        if !wait_for_event(&mut n) {
            return Err("primitiveFailedError: no event".into());
        }
        let out = unsafe { std::slice::from_raw_parts_mut(buf as *mut u8, events::EVENT_BYTES) };
        let win = n.window.as_mut().unwrap();
        if peek {
            win.events().peek_into(out);
        } else {
            win.events().next_into(out);
        }
        Ok(())
    })();
    drain(&mut n);
    vm.native = Some(n);
    r
}

/// The window's pixels are the surface's, everything else is in the store.
fn canvas_of(n: &mut Native, h: u64) -> Option<&mut Canvas> {
    if n.win == Some(index(h)) {
        return n.window.as_mut().map(|w| w.canvas());
    }
    if n.d.has(index(h)) {
        Some(n.d.get_mut(index(h)))
    } else {
        None
    }
}

fn draw(n: &mut Native, d: u64, gc: u64, f: impl FnOnce(&mut Canvas, &Gc)) -> u64 {
    let g = n.gc(gc);
    if let Some(c) = canvas_of(n, d) {
        f(c, &g);
    }
    0
}

#[allow(clippy::too_many_arguments)]
fn blit(
    n: &mut Native,
    src: u64,
    sx: i32,
    sy: i32,
    w: i32,
    h: i32,
    dst: u64,
    dx: i32,
    dy: i32,
    g: &Gc,
) {
    // One of the two may be the window, whose pixels are not in the store, so
    // the band is lifted out before anything is written -- and in real pixels,
    // for the reason `canvas::lift` gives.
    let k = scale_of(n, src).min(scale_of(n, dst));
    let band = match canvas_of(n, src) {
        Some(c) => crate::canvas::lift(c, sx, sy, w, h, k),
        None => return,
    };
    if let Some(d) = canvas_of(n, dst) {
        crate::canvas::drop_in(d, &band, (w * k).max(0), (h * k).max(0), dx, dy, k, g);
    }
}

/// `SERF_CLICK=x,y[@seconds][xN]` -- N clicks at that point, starting then.
/// A press, a pause, a release, and a gap before the next, because that is
/// what a hand does and what the world is written to recognise.
fn scripted_clicks() -> Vec<(std::time::Instant, crate::events::Event)> {
    use crate::events::{Event, Pointer, BUTTON1_MASK};
    let Ok(v) = std::env::var("SERF_CLICK") else { return vec![] };
    let (v, times) = v.split_once('x').unwrap_or((v.as_str(), "1"));
    let (at, after) = v.split_once('@').unwrap_or((v, "20"));
    let Some((x, y)) = at.split_once(',') else { return vec![] };
    let (Ok(x), Ok(y)) = (x.trim().parse::<i32>(), y.trim().parse::<i32>()) else { return vec![] };
    let start =
        std::time::Instant::now() + Duration::from_secs_f64(after.trim().parse().unwrap_or(20.0));
    let at = Pointer { x, y, x_root: x, y_root: y, state: 0 };
    let down = Pointer { state: BUTTON1_MASK, ..at };
    let mut out = vec![(start, Event::Motion { window: 1, at })];
    for i in 0..times.trim().parse().unwrap_or(1) {
        let base = start + Duration::from_millis(120 + i * 400);
        out.push((base, Event::Button { press: true, window: 1, at, button: 1 }));
        out.push((
            base + Duration::from_millis(90),
            Event::Button { press: false, window: 1, at: down, button: 1 },
        ));
    }
    out
}

/// A drawable's scale, for the blits that have to work in real pixels.
fn scale_of(n: &mut Native, h: u64) -> i32 {
    canvas_of(n, h).map_or(1, |c| c.scale)
}

/// Take the wakeup byte back out once the queue is empty, so the world's
/// `select` blocks again instead of spinning on a connection that stays ready.
fn drain(n: &mut Native) {
    if !n.armed || !n.queue_empty() {
        return;
    }
    if let Some((r, _)) = n.pipe.as_mut() {
        use std::io::Read;
        let mut b = [0u8; 1];
        let _ = r.read(&mut b);
    }
    n.armed = false;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Handles are what the world hands back, so a drawable must never be
    /// mistaken for a GC and index 0 must not be a null pointer -- a `proxy`
    /// argument that is zero is a failure before the call is even made.
    #[test]
    fn handles_are_distinct_and_never_null() {
        let mut seen = std::collections::HashSet::new();
        for kind in [DISPLAY, SCREEN, DRAWABLE, GC, COLORMAP, VISUAL, CURSOR, ATOM, FONT] {
            for i in 0..4 {
                let h = handle(kind, i);
                assert_ne!(h, 0, "a handle must not be null");
                assert!(seen.insert(h), "two handles collided");
                assert_eq!(index(h), i, "a handle did not survive the round trip");
            }
        }
        assert_ne!(DISPLAY, SCREEN);
    }

    /// The whole point of the backend: an X call it does not implement fails
    /// loudly rather than reaching a real Xlib with a handle that is not a
    /// pointer, which would not fail at all.
    #[test]
    fn every_x_shaped_name_is_claimed() {
        for c in ["XFillRectangle", "XPending", "BlackPixelOfScreen", "ConnectionNumber"] {
            assert!(claims(c), "{} was not claimed", c);
        }
        // and the rest of the glue -- libc, libm -- is left alone
        for c in ["fcntl", "getpid", "sqrt", "socket", "MYSELF"] {
            assert!(!claims(c), "{} should go to the FFI", c);
        }
        // an X name with no implementation must be claimed all the same
        assert!(claims("XkbSetDetectableAutoRepeat"));
    }

    #[test]
    fn the_graphics_function_follows_x_numbering() {
        assert_eq!(func_of(3), Func::Copy, "GXcopy");
        assert_eq!(func_of(6), Func::Xor, "GXxor");
        assert_eq!(func_of(0), Func::Copy, "GXclear draws as a copy here");
    }
}
