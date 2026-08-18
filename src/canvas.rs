//! Pixels for the native canvas: X's drawables, its graphics context, and the
//! drawing calls the world makes through them.
//!
//! A window, a pixmap and an `XImage` are all the same thing here -- a
//! rectangle of `0x00RRGGBB` -- so `XCopyArea`, `XPutImage` and `XPutPixel` all
//! reach the same store and there is one blit rather than three. That holds
//! because the world runs truecolor: `x11Globals platformColormap` allocates
//! through `XAllocColor`, whose answer on a truecolor visual is the packed
//! pixel it was asked for.
//!
//! The set is what `morphic.snap` actually mentions, which is smaller than
//! Xlib's: every drawing call it names is singular, so there is no
//! `XFillRectangles`, no `XDrawPoints` and no `XDrawSegments` to answer.

/// A drawable. `bg` is what `XClearArea` puts back.
///
/// `w` and `h` are the size the *world* thinks this is, and `scale` is how many
/// real pixels each of those is worth. The world is an X client: it draws in
/// device pixels and has no idea a display might have two of them where it
/// expects one, so on a retina screen drawing 1:1 comes out half size. Keeping
/// its coordinates logical and widening them here is what fixes that, and it
/// is one place rather than every call -- every drawing op funnels through
/// `plot`.
///
/// Text is the exception, and deliberately: glyphs rasterise at `scale` and
/// blend at real pixels, so they stay sharp rather than being drawn small and
/// then doubled. Lines are the other one: the pen walks real pixels, so a
/// diagonal steps at the display's resolution instead of in blocks. Both cover
/// the same footprint a 1:1 draw would have, so the world sees no difference.
pub struct Canvas {
    pub w: i32,
    pub h: i32,
    pub scale: i32,
    pub bg: u32,
    pub px: Vec<u32>,
}

/// X's `GXcopy` and `GXxor`. The world uses xor for drag feedback, where the
/// point is that drawing the same thing twice puts the screen back.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub enum Func {
    #[default]
    Copy,
    Xor,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// A graphics context: the state X keeps on the side of every drawing call.
///
/// ponytail: `XSetTile`, `XSetStipple`, `XSetDashes`, `XSetFillStyle` and the
/// line join/cap styles are accepted and ignored -- everything draws solid.
/// Morphic builds its grey patterns as a pixmap it copies (the 256 `XDrawPoint`
/// calls in a boot trace are one being filled in), so solid covers the desktop;
/// wire the tile through `fill_rect` if a world turns up that stipples directly.
/// `XSetBackground` has nowhere to go yet and so is not here: the background
/// pixel is only read by tiles, stipples and `XDrawImageString`, none of which
/// this draws. `XClearArea` uses the *drawable's* background, which is X's rule.
#[derive(Clone, Debug)]
pub struct Gc {
    pub fg: u32,
    pub func: Func,
    pub line_width: i32,
    /// `XSetClipRectangle`, in coordinates that `clip_origin` shifts
    pub clip: Option<Rect>,
    pub clip_origin: (i32, i32),
    /// index into `text::Fonts`, set by `XSetFont`
    pub font: Option<usize>,
}

impl Default for Gc {
    fn default() -> Gc {
        Gc {
            fg: 0x000000,
            func: Func::Copy,
            line_width: 1,
            clip: None,
            clip_origin: (0, 0),
            font: None,
        }
    }
}

impl Gc {
    /// `XSetClipMask(gc, None)` -- draw everywhere again.
    pub fn clear_clip(&mut self) {
        self.clip = None;
    }

    /// Whether the clip lets a *logical* pixel through.
    pub fn allows(&self, x: i32, y: i32) -> bool {
        match self.clip {
            None => true,
            Some(r) => {
                let (ox, oy) = self.clip_origin;
                x >= r.x + ox && y >= r.y + oy && x < r.x + ox + r.w && y < r.y + oy + r.h
            }
        }
    }
}

impl Canvas {
    pub fn new(w: i32, h: i32, fill: u32) -> Canvas {
        Canvas::scaled(w, h, fill, 1)
    }

    /// `scale` real pixels per logical one. Anything but 1 makes this a canvas
    /// whose buffer is bigger than the size the world was told.
    pub fn scaled(w: i32, h: i32, fill: u32, scale: i32) -> Canvas {
        let (w, h, scale) = (w.max(0), h.max(0), scale.max(1));
        Canvas { w, h, scale, bg: fill, px: vec![fill; (w * scale * h * scale) as usize] }
    }

    /// The buffer's own size, which is what a surface or a PNG wants.
    pub fn pw(&self) -> i32 {
        self.w * self.scale
    }

    pub fn ph(&self) -> i32 {
        self.h * self.scale
    }

    /// One real pixel, for the paths that must not lose resolution: blits
    /// between drawables, and glyph coverage.
    pub fn at(&self, x: i32, y: i32) -> u32 {
        if x < 0 || y < 0 || x >= self.pw() || y >= self.ph() {
            0
        } else {
            self.px[(y * self.pw() + x) as usize]
        }
    }

    pub fn set_at(&mut self, x: i32, y: i32, v: u32) {
        if x >= 0 && y >= 0 && x < self.pw() && y < self.ph() {
            let i = (y * self.pw() + x) as usize;
            self.px[i] = v;
        }
    }

    /// One logical pixel: the top-left of the block it stands for.
    pub fn get(&self, x: i32, y: i32) -> u32 {
        if x < 0 || y < 0 || x >= self.w || y >= self.h {
            0
        } else {
            self.at(x * self.scale, y * self.scale)
        }
    }

    /// `XPutPixel`: no clip and no function, as X means it -- an image is
    /// client-side memory, not something the server draws on.
    pub fn put(&mut self, x: i32, y: i32, v: u32) {
        if x < 0 || y < 0 || x >= self.w || y >= self.h {
            return;
        }
        let s = self.scale;
        for dy in 0..s {
            for dx in 0..s {
                self.set_at(x * s + dx, y * s + dy, v);
            }
        }
    }

    /// One pixel through a graphics context: clipped, and combined the way the
    /// context's function says.
    pub fn plot(&mut self, g: &Gc, x: i32, y: i32, rgb: u32) {
        if !g.allows(x, y) || x < 0 || y < 0 || x >= self.w || y >= self.h {
            return;
        }
        let (s, w) = (self.scale, self.pw());
        for dy in 0..s {
            for dx in 0..s {
                let i = ((y * s + dy) * w + x * s + dx) as usize;
                self.px[i] = match g.func {
                    Func::Copy => rgb,
                    Func::Xor => (self.px[i] ^ rgb) & 0xFF_FFFF,
                };
            }
        }
    }

    /// The same, one *real* pixel at a time -- the pen's path on a scaled
    /// canvas. The clip is the world's, so it is still asked in logical
    /// coordinates; the bounds test comes first, since a negative real pixel
    /// would divide towards zero and land on the wrong logical one.
    fn plot_at(&mut self, g: &Gc, x: i32, y: i32, rgb: u32) {
        if x < 0 || y < 0 || x >= self.pw() || y >= self.ph() {
            return;
        }
        if !g.allows(x / self.scale, y / self.scale) {
            return;
        }
        let i = (y * self.pw() + x) as usize;
        self.px[i] = match g.func {
            Func::Copy => rgb,
            Func::Xor => (self.px[i] ^ rgb) & 0xFF_FFFF,
        };
    }

    /// One coverage sample, for antialiased glyphs. Grayscale only, and never
    /// subpixel: Morphic blits its own pixels around constantly, and
    /// subpixel-filtered text refringes as soon as it is moved.
    ///
    /// ponytail: blending is always a Copy, whatever the context's function
    /// says -- xor-ing a fractional coverage is not a thing X can do either.
    /// `x` and `y` are *real* pixels here, not logical ones -- this is the one
    /// path that works at the display's own resolution, so that text drawn on a
    /// scaled canvas is sharp rather than drawn small and blown up. The clip is
    /// still the world's, so it is tested in logical coordinates.
    pub fn blend(&mut self, g: &Gc, x: i32, y: i32, rgb: u32, cov: u8) {
        if cov == 0 || !g.allows(x / self.scale, y / self.scale) {
            return;
        }
        if x < 0 || y < 0 || x >= self.pw() || y >= self.ph() {
            return;
        }
        let i = (y * self.pw() + x) as usize;
        let (d, a) = (self.px[i], cov as u32);
        let mix = |s: u32, d: u32| (s * a + d * (255 - a) + 127) / 255;
        self.px[i] = (mix(rgb >> 16 & 255, d >> 16 & 255) << 16)
            | (mix(rgb >> 8 & 255, d >> 8 & 255) << 8)
            | mix(rgb & 255, d & 255);
    }

    /// `XDrawPoint`.
    pub fn point(&mut self, g: &Gc, x: i32, y: i32) {
        self.plot(g, x, y, g.fg);
    }

    /// `XFillRectangle`.
    pub fn fill_rect(&mut self, g: &Gc, x: i32, y: i32, w: i32, h: i32) {
        for yy in y..y + h {
            for xx in x..x + w {
                self.plot(g, xx, yy, g.fg);
            }
        }
    }

    /// `XDrawRectangle`, which in X is w+1 by h+1 pixels around the outside.
    ///
    /// The four edges are walked half-open, each corner falling to exactly one
    /// of them. Four whole lines would paint every corner twice, and under
    /// GXxor -- which is what drag feedback uses -- a twice-painted pixel is a
    /// missing one, so the corners of the rubber band would drop out.
    pub fn draw_rect(&mut self, g: &Gc, x: i32, y: i32, w: i32, h: i32) {
        for i in 0..w {
            self.pen(g, x + i, y);
            self.pen(g, x + w - i, y + h);
        }
        for i in 0..h {
            self.pen(g, x + w, y + i);
            self.pen(g, x, y + h - i);
        }
        if w == 0 && h == 0 {
            self.pen(g, x, y);
        }
    }

    /// `XClearArea`: back to the drawable's background. A zero width or height
    /// means "to the edge", as X specifies.
    pub fn clear_area(&mut self, x: i32, y: i32, w: i32, h: i32) {
        let (w, h) = (if w == 0 { self.w - x } else { w }, if h == 0 { self.h - y } else { h });
        let (bg, s) = (self.bg, self.scale);
        for yy in (y.max(0) * s)..((y + h).min(self.h) * s) {
            for xx in (x.max(0) * s)..((x + w).min(self.w) * s) {
                self.set_at(xx, yy, bg);
            }
        }
    }

    /// `XDrawLine`, Bresenham -- stepped in *real* pixels rather than the
    /// world's. Run at the world's resolution and then blown up, a diagonal on
    /// a retina display staircases in `scale`-sized blocks; run at the
    /// display's, it staircases in its own pixels. A logical endpoint stands
    /// for the centre of the block it names and the pen is `scale` real pixels
    /// per logical one, so the footprint is the one an unscaled draw would
    /// leave -- same width, same ends, finer steps in between.
    ///
    /// ponytail: a line wider than one pixel is a square pen dragged along the
    /// run, so joins and caps are whatever that leaves. X's join and cap styles
    /// need the pen to be a polygon; nothing in Morphic has asked yet.
    pub fn line(&mut self, g: &Gc, x0: i32, y0: i32, x1: i32, y1: i32) {
        let (h, s) = (self.scale / 2, self.scale);
        let (x0, y0, x1, y1) = (x0 * s + h, y0 * s + h, x1 * s + h, y1 * s + h);
        let (dx, dy) = ((x1 - x0).abs(), -(y1 - y0).abs());
        let (sx, sy) = (if x0 < x1 { 1 } else { -1 }, if y0 < y1 { 1 } else { -1 });
        let (mut x, mut y, mut err) = (x0, y0, dx + dy);
        loop {
            self.nib(g, x, y);
            if x == x1 && y == y1 {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x += sx;
            }
            if e2 <= dx {
                err += dx;
                y += sy;
            }
        }
    }

    /// The square pen at one logical pixel, for the calls that are still drawn
    /// in the world's own grid.
    fn pen(&mut self, g: &Gc, x: i32, y: i32) {
        let (h, s) = (self.scale / 2, self.scale);
        self.nib(g, x * s + h, y * s + h);
    }

    /// The pen where it actually lands: `line_width` *logical* pixels wide,
    /// centred on a real one. On an unscaled canvas this is X's own pen.
    fn nib(&mut self, g: &Gc, x: i32, y: i32) {
        let n = g.line_width.max(1) * self.scale;
        let half = n / 2;
        for yy in y - half..y - half + n {
            for xx in x - half..x - half + n {
                self.plot_at(g, xx, yy, g.fg);
            }
        }
    }

    /// `XDrawLines`: a polyline, as the world passes it -- one x vector and one
    /// y vector, in `CoordModeOrigin`.
    pub fn lines(&mut self, g: &Gc, xs: &[i32], ys: &[i32]) {
        for i in 1..xs.len().min(ys.len()) {
            self.line(g, xs[i - 1], ys[i - 1], xs[i], ys[i]);
        }
    }

    /// `XFillPolygon`, scanline with the even-odd rule.
    ///
    /// ponytail: X's `Shape` hint (Convex/Nonconvex/Complex) only ever lets a
    /// server pick a faster path, so it is ignored; even-odd is the answer that
    /// is right for all three.
    pub fn fill_polygon(&mut self, g: &Gc, xs: &[i32], ys: &[i32]) {
        let n = xs.len().min(ys.len());
        if n < 3 {
            return;
        }
        // Scanned in real pixels, so an edge staircases at the display's own
        // resolution rather than in logical blocks: the world fills a disc and
        // draws its outline over it, and a coarser fill sticks out past the edge
        // meant to cover it. A vertex is the *corner* of its block though, not
        // the centre `line` steps between -- half-open on both axes, that covers
        // exactly what `fill_rect` would. Centred, the shape lands half a
        // logical pixel down and right, and the half hanging off the bottom is
        // outside the rectangle the world thinks it painted, so the world's own
        // repair never takes it back: one row of a window left on the desktop
        // per frame of the animation that closes it.
        let s = self.scale;
        let to_real = |v: &[i32]| v[..n].iter().map(|c| c * s).collect::<Vec<i32>>();
        let (xs, ys) = (to_real(xs), to_real(ys));
        let (lo, hi) = (*ys.iter().min().unwrap(), *ys.iter().max().unwrap());
        let mut cross: Vec<i32> = vec![];
        for y in lo..=hi {
            cross.clear();
            for i in 0..n {
                let j = (i + 1) % n;
                let (y0, y1) = (ys[i], ys[j]);
                // a half-open edge test, so a vertex is not counted twice
                if (y0 <= y) != (y1 <= y) {
                    let t = (y - y0) as f64 / (y1 - y0) as f64;
                    cross.push(xs[i] + ((xs[j] - xs[i]) as f64 * t).round() as i32);
                }
            }
            cross.sort_unstable();
            for pair in cross.chunks(2) {
                // half-open on the right, as X paints a pixel only when its
                // centre is inside. Two polygons that share an edge must not
                // both claim it -- under GXxor a doubly-painted seam shows up
                // as a line that should not be there.
                if let [a, b] = pair {
                    for x in *a..*b {
                        self.plot_at(g, x, y, g.fg);
                    }
                }
            }
        }
    }

    /// `XDrawArc` and `XFillArc`: the ellipse inscribed in `w` by `h` at
    /// `x`, `y`, with angles in 64ths of a degree counterclockwise from three
    /// o'clock, which is how X states them.
    pub fn arc(&mut self, g: &Gc, x: i32, y: i32, w: i32, h: i32, a1: i32, a2: i32, fill: bool) {
        let (cx, cy) = (x as f64 + w as f64 / 2.0, y as f64 + h as f64 / 2.0);
        let (rx, ry) = (w as f64 / 2.0, h as f64 / 2.0);
        let steps = (w.abs() + h.abs()).clamp(8, 720) as usize;
        let at = |i: usize| {
            let deg = (a1 as f64 + a2 as f64 * i as f64 / steps as f64) / 64.0;
            let t = deg.to_radians();
            // X's y grows downward, so a counterclockwise angle turns the other
            // way on the screen
            ((cx + rx * t.cos()).round() as i32, (cy - ry * t.sin()).round() as i32)
        };
        let (xs, ys): (Vec<i32>, Vec<i32>) = (0..=steps).map(at).unzip();
        if fill {
            // a pie slice, unless it closes on itself
            let full = a2.abs() >= 360 * 64;
            let (mut xs, mut ys) = (xs, ys);
            if !full {
                xs.push(cx.round() as i32);
                ys.push(cy.round() as i32);
            }
            self.fill_polygon(g, &xs, &ys);
        } else {
            self.lines(g, &xs, &ys);
        }
    }
}

/// Every drawable the world has made: the window, its pixmaps, and the client
/// side images it fills in and puts back. One store, because to this backend
/// they are the same rectangle of pixels.
#[derive(Default)]
pub struct Drawables(Vec<Canvas>);

impl Drawables {
    pub fn add(&mut self, c: Canvas) -> usize {
        self.0.push(c);
        self.0.len() - 1
    }

    pub fn get(&self, id: usize) -> &Canvas {
        &self.0[id]
    }

    /// Whether an id names one. The world's handles come back from its own
    /// memory, so a stale one has to answer no rather than panic.
    pub fn has(&self, id: usize) -> bool {
        id < self.0.len()
    }

    pub fn get_mut(&mut self, id: usize) -> &mut Canvas {
        &mut self.0[id]
    }

    /// Lift one drawable out, leaving an empty one behind. `XFreePixmap` for
    /// everything else, and how a window's pixels get handed to a surface.
    pub fn take(&mut self, id: usize) -> Canvas {
        std::mem::replace(&mut self.0[id], Canvas::new(0, 0, 0))
    }

    /// `XCopyArea` and `XPutImage`, which are the same operation once an image
    /// and a pixmap are the same thing.
    #[allow(clippy::too_many_arguments)]
    pub fn copy_area(
        &mut self,
        g: &Gc,
        src: usize,
        sx: i32,
        sy: i32,
        w: i32,
        h: i32,
        dst: usize,
        dx: i32,
        dy: i32,
    ) {
        let k = self.get(src).scale.min(self.get(dst).scale);
        let band = lift(self.get(src), sx, sy, w, h, k);
        drop_in(self.get_mut(dst), &band, w * k, h * k, dx, dy, k, g);
    }
}

/// Take a rectangle out of a drawable, in *real* pixels rather than the
/// world's.
///
/// Morphic draws its text into a backing pixmap and copies that to the window,
/// so a blit that worked a logical pixel at a time would throw away every
/// glyph's resolution on the way -- sharp text, flattened by the copy that
/// delivers it.
///
/// Lifting and putting down are two calls because the source and the
/// destination may be the same drawable, which is what a scroll is, and because
/// one of them may be the window -- whose pixels live in its surface rather than
/// in the store. There is one implementation because there were briefly two,
/// and only one of them got fixed.
pub fn lift(src: &Canvas, sx: i32, sy: i32, w: i32, h: i32, k: i32) -> Vec<u32> {
    let (pw, ph) = ((w * k).max(0), (h * k).max(0));
    let mut band = Vec::with_capacity((pw * ph) as usize);
    for y in 0..ph {
        for x in 0..pw {
            band.push(src.at(sx * k + x, sy * k + y));
        }
    }
    band
}

/// ...and put it back down, through the context's clip and function.
#[allow(clippy::too_many_arguments)]
pub fn drop_in(dst: &mut Canvas, band: &[u32], pw: i32, ph: i32, dx: i32, dy: i32, k: i32, g: &Gc) {
    for y in 0..ph {
        for x in 0..pw {
            let (px, py) = (dx * k + x, dy * k + y);
            if !g.allows(px / k.max(1), py / k.max(1)) {
                continue;
            }
            let Some(v) = band.get((y * pw + x) as usize).copied() else { continue };
            let out = match g.func {
                Func::Xor => (dst.at(px, py) ^ v) & 0xFF_FFFF,
                Func::Copy => v,
            };
            dst.set_at(px, py, out);
        }
    }
}

/// `XAllocColor` on a truecolor visual: the pixel value *is* the colour, so the
/// world's colormap is the identity and nothing has to be allocated. X states
/// the components as 16-bit, and the world sends them that way.
pub fn alloc_color(r: u16, gr: u16, b: u16) -> u32 {
    ((r >> 8) as u32) << 16 | ((gr >> 8) as u32) << 8 | (b >> 8) as u32
}

// --- PNG, so the result can be looked at ------------------------------------

fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
    }
    !c
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &x in data {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn chunk(out: &mut Vec<u8>, tag: &[u8; 4], body: &[u8]) {
    out.extend((body.len() as u32).to_be_bytes());
    let mut c = tag.to_vec();
    c.extend_from_slice(body);
    out.extend_from_slice(&c);
    out.extend(crc32(&c).to_be_bytes());
}

/// ponytail: deflate's *stored* blocks, so a debug dump needs no compressor and
/// the VM keeps its one graphics dependency. The file is about as big as the
/// pixels; nothing but eyes ever reads these.
pub fn write_png(path: &str, c: &Canvas) -> std::io::Result<()> {
    let (w, h) = (c.pw(), c.ph());
    let mut raw = Vec::with_capacity((h * (1 + w * 3)) as usize);
    for y in 0..h {
        raw.push(0); // filter: none
        for x in 0..w {
            let p = c.at(x, y);
            raw.extend([(p >> 16) as u8, (p >> 8) as u8, p as u8]);
        }
    }
    let mut z = vec![0x78, 0x01];
    for (i, b) in raw.chunks(0xFFFF).enumerate() {
        let last = u8::from((i + 1) * 0xFFFF >= raw.len());
        z.extend([
            last,
            b.len() as u8,
            (b.len() >> 8) as u8,
            !b.len() as u8,
            !(b.len() >> 8) as u8,
        ]);
        z.extend_from_slice(b);
    }
    z.extend(adler32(&raw).to_be_bytes());

    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend((w as u32).to_be_bytes());
    ihdr.extend((h as u32).to_be_bytes());
    ihdr.extend([8, 2, 0, 0, 0]); // 8-bit, truecolour
    chunk(&mut png, b"IHDR", &ihdr);
    chunk(&mut png, b"IDAT", &z);
    chunk(&mut png, b"IEND", &[]);
    std::fs::write(path, png)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ink(c: &Canvas, v: u32) -> usize {
        c.px.iter().filter(|p| **p == v).count()
    }

    /// Every drawing call goes through one clip test, so one op proves it for
    /// all of them -- but a clip that is off by its origin is exactly the bug
    /// that paints a morph over its neighbours.
    #[test]
    fn the_clip_holds_and_moves_with_its_origin() {
        let mut c = Canvas::new(40, 40, 0xFFFFFF);
        let mut g = Gc { fg: 0xFF0000, ..Gc::default() };
        g.clip = Some(Rect { x: 10, y: 10, w: 5, h: 5 });
        c.fill_rect(&g, 0, 0, 40, 40);
        assert_eq!(ink(&c, 0xFF0000), 25, "clip did not hold");
        assert_eq!(c.get(10, 10), 0xFF0000);
        assert_eq!(c.get(9, 10), 0xFFFFFF);

        // the same rectangle, shifted by the clip origin
        let mut c = Canvas::new(40, 40, 0xFFFFFF);
        g.clip_origin = (3, 4);
        c.fill_rect(&g, 0, 0, 40, 40);
        assert_eq!(ink(&c, 0xFF0000), 25);
        assert_eq!(c.get(13, 14), 0xFF0000);
        assert_eq!(c.get(10, 10), 0xFFFFFF, "clip ignored its origin");

        g.clear_clip();
        c.fill_rect(&g, 0, 0, 40, 40);
        assert_eq!(ink(&c, 0xFF0000), 1600, "XSetClipMask(None) did not open it up");
    }

    /// What drag feedback is for: drawing the same thing twice with GXxor puts
    /// the screen back exactly as it was.
    #[test]
    fn drawing_twice_with_xor_undoes_itself() {
        let mut c = Canvas::new(30, 30, 0x336699);
        let before = c.px.clone();
        let g = Gc { fg: 0xFFFFFF, func: Func::Xor, ..Gc::default() };
        for _ in 0..2 {
            c.fill_rect(&g, 2, 2, 20, 12);
            c.line(&g, 0, 0, 29, 29);
            c.arc(&g, 5, 5, 16, 16, 0, 360 * 64, false);
            // the outline is the one that catches a doubly-painted corner
            c.draw_rect(&g, 3, 3, 22, 22);
        }
        assert_eq!(c.px, before, "xor is not its own inverse");

        // ...and *one* pass has to light every corner exactly once. Four whole
        // lines would paint each corner twice, which under xor is not painting
        // it at all -- a rubber band with its corners missing. Drawing twice
        // cannot see that, because twice-of-twice is back where it started.
        let mut c = Canvas::new(30, 30, 0);
        c.draw_rect(&g, 3, 3, 22, 22);
        for (x, y) in [(3, 3), (25, 3), (3, 25), (25, 25)] {
            assert_eq!(c.get(x, y), 0xFFFFFF, "corner {},{} was painted twice", x, y);
        }
    }

    /// A drawable scrolling over itself is the case a naive blit gets wrong,
    /// and Morphic scrolls constantly.
    #[test]
    fn a_blit_that_overlaps_itself_still_moves_the_pixels() {
        let mut d = Drawables::default();
        let id = d.add(Canvas::new(10, 1, 0));
        for x in 0..10 {
            d.get_mut(id).put(x, 0, x as u32 + 1);
        }
        // shift right by two, over the top of the source
        d.copy_area(&Gc::default(), id, 0, 0, 8, 1, id, 2, 0);
        let got: Vec<u32> = (0..10).map(|x| d.get(id).get(x, 0)).collect();
        assert_eq!(got, vec![1, 2, 1, 2, 3, 4, 5, 6, 7, 8]);
    }

    /// On a retina display one of the world's pixels is four real ones. It must
    /// not find that out: its coordinates stay its own, and only the buffer
    /// underneath gets bigger.
    #[test]
    fn a_scaled_canvas_keeps_the_world_in_its_own_pixels() {
        let mut c = Canvas::scaled(10, 8, 0, 2);
        assert_eq!((c.w, c.h, c.pw(), c.ph()), (10, 8, 20, 16));
        let g = Gc { fg: 0xFF0000, ..Gc::default() };

        c.point(&g, 3, 4);
        for (x, y) in [(6, 8), (7, 8), (6, 9), (7, 9)] {
            assert_eq!(c.at(x, y), 0xFF0000, "real pixel {},{} of the block", x, y);
        }
        assert_eq!(c.at(5, 8), 0, "the block spread sideways");
        assert_eq!(c.get(3, 4), 0xFF0000, "a logical read missed its own block");

        // the world's edge is the world's, not the buffer's
        c.point(&g, 10, 4);
        assert_eq!(c.at(20, 8), 0, "drawing past the logical edge reached the buffer");

        // ...except for glyph coverage, which is the one path that works at the
        // display's resolution, so text is sharp rather than drawn small and
        // blown up. One real pixel, not a block.
        c.blend(&Gc::default(), 1, 1, 0xFFFFFF, 255);
        assert_eq!(c.at(1, 1), 0xFFFFFF);
        assert_eq!(c.at(0, 0), 0, "blend widened a real pixel into a block");
    }

    /// The other path that works at the display's resolution: a diagonal
    /// staircases in real pixels, not in `scale`-sized blocks, while still
    /// covering the same footprint and the same ends as an unscaled draw.
    #[test]
    fn a_diagonal_staircases_at_the_displays_resolution() {
        let g = Gc { fg: 0xFF0000, ..Gc::default() };
        let (a, b) = ((0, 0), (4, 2));

        let mut c = Canvas::scaled(8, 6, 0, 2);
        c.line(&g, a.0, a.1, b.0, b.1);
        // real pixel 2,1 is in logical block 1,0 -- which a doubled 1:1 line
        // never touches, because it steps from block 0,0 straight to 1,1
        assert_eq!(c.at(2, 1), 0xFF0000, "the step is still a whole logical block");
        assert_eq!(c.get(0, 0), 0xFF0000, "an end moved off its own pixel");
        assert_eq!(c.get(4, 2), 0xFF0000, "an end moved off its own pixel");
        assert_eq!(c.get(0, 2), 0, "the line spilled below itself");

        // and it is still the same line: every logical pixel a 1:1 draw inks,
        // this one inks too
        let mut flat = Canvas::new(8, 6, 0);
        flat.line(&g, a.0, a.1, b.0, b.1);
        for y in 0..6 {
            for x in 0..8 {
                if flat.get(x, y) == 0xFF0000 {
                    assert_eq!(c.get(x, y), 0xFF0000, "logical {},{} went missing", x, y);
                }
            }
        }

        // xor feedback still undoes itself: the rubber band Morphic drags is
        // the same line drawn twice
        let x = Gc { fg: 0xFFFFFF, func: Func::Xor, ..Gc::default() };
        let mut c = Canvas::scaled(8, 6, 0, 2);
        c.line(&x, a.0, a.1, b.0, b.1);
        c.line(&x, a.0, a.1, b.0, b.1);
        assert_eq!(c.px.iter().filter(|&&p| p != 0).count(), 0, "xor left the line behind");

        // and a fill steps as finely as the outline the world draws over it,
        // or the fill shows past the edge that is meant to cover it
        let mut c = Canvas::scaled(8, 6, 0, 2);
        c.fill_polygon(&g, &[0, 8, 0], &[0, 4, 4]);
        assert_eq!(c.at(4, 3), 0xFF0000, "the fill stepped a whole logical block");
        assert_eq!(c.at(6, 3), 0, "the fill leaked past its own edge");

        // ...but still on the world's own grid: a polygon around a logical
        // rectangle covers what `fill_rect` covers, not that shifted half a
        // block. The half that hangs off the bottom is outside the rectangle
        // the world repairs, so it stays on the screen -- one row of a window
        // left behind per frame of the animation that closes it.
        let mut poly = Canvas::scaled(8, 6, 0, 2);
        poly.fill_polygon(&g, &[1, 6, 6, 1], &[1, 1, 4, 4]);
        let mut rect = Canvas::scaled(8, 6, 0, 2);
        rect.fill_rect(&g, 1, 1, 5, 3);
        assert_eq!(poly.px, rect.px, "a polygon fill is off its logical block");
    }

    /// Morphic draws into a backing pixmap and copies it to the window, so a
    /// blit that worked a logical pixel at a time would throw away every glyph's
    /// extra resolution on the way -- crisp text, then flattened by the copy.
    #[test]
    fn a_blit_keeps_the_resolution_the_text_was_drawn_at() {
        let mut d = Drawables::default();
        let (src, dst) = (d.add(Canvas::scaled(4, 1, 0, 2)), d.add(Canvas::scaled(4, 1, 0, 2)));
        // sub-logical detail, as a glyph leaves
        d.get_mut(src).blend(&Gc::default(), 1, 0, 0xFFFFFF, 255);
        d.get_mut(src).blend(&Gc::default(), 4, 1, 0xFFFFFF, 255);

        d.copy_area(&Gc::default(), src, 0, 0, 4, 1, dst, 0, 0);
        assert_eq!(d.get(dst).at(1, 0), 0xFFFFFF, "the copy lost a real pixel");
        assert_eq!(d.get(dst).at(4, 1), 0xFFFFFF);
        assert_eq!(d.get(dst).at(0, 0), 0, "the copy smeared one into a block");
    }

    /// Fills have to cover their inside and nothing else: a polygon that leaks
    /// is how a filled morph paints over the one behind it.
    #[test]
    fn fills_cover_the_inside_and_stop() {
        let mut c = Canvas::new(24, 24, 0xFFFFFF);
        let g = Gc { fg: 0x000000, ..Gc::default() };
        // a triangle with a known area: base 20, height 20, so about 200
        c.fill_polygon(&g, &[2, 22, 2], &[2, 22, 22]);
        let n = ink(&c, 0x000000);
        assert!((180..=210).contains(&n), "triangle covered {} pixels", n);
        assert_eq!(c.get(4, 20), 0x000000, "inside was not filled");
        assert_eq!(c.get(20, 4), 0xFFFFFF, "fill leaked outside");

        // two polygons meeting on x = 12 must not both paint it, or an xor
        // seam appears where the world drew none
        let mut c = Canvas::new(24, 24, 0xFFFFFF);
        let x = Gc { fg: 0xFFFFFF, func: Func::Xor, ..Gc::default() };
        c.fill_polygon(&x, &[2, 12, 12, 2], &[2, 2, 20, 20]);
        c.fill_polygon(&x, &[12, 22, 22, 12], &[2, 2, 20, 20]);
        assert_eq!(ink(&c, 0xFFFFFF), 24 * 24 - 20 * 18, "the shared edge was painted twice");

        let mut c = Canvas::new(24, 24, 0xFFFFFF);
        c.fill_rect(&g, 4, 4, 6, 6);
        assert_eq!(ink(&c, 0x000000), 36);
        c.clear_area(0, 0, 0, 0);
        assert_eq!(ink(&c, 0xFFFFFF), 24 * 24, "XClearArea left something behind");
    }
}
