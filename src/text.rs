//! Text for the native canvas, from the fonts installed on the host.
//!
//! The world asks for text through X's core font calls -- `XLoadQueryFont` with
//! an XLFD, then `XTextWidth` and `XDrawString` -- so those are what this
//! answers. Nothing says the thing behind that interface has to be a font
//! server's bitmaps.
//!
//! What the image can actually read back is small. `src/struct_table.rs` gives
//! it `XFontStruct` ascent, descent, fid, min/max_char_or_byte2 and `per_char`
//! -- and `per_char` has no indexing accessor, only a test for whether it is
//! there, so answering NULL forces every width through `XTextWidth`. Three
//! numbers and a width function is the whole contract.

use std::collections::HashMap;

use cosmic_text::{
    Attrs, Buffer, Family, FontSystem, Metrics, Shaping, Style, SwashCache, SwashContent, Weight,
    Wrap,
};

use crate::canvas::{Canvas, Gc};

/// A font size with no XLFD to say otherwise. X's own `fixed` is 13 pixels.
const DEFAULT_PX: f32 = 13.0;

#[derive(Clone, Debug, PartialEq)]
enum Face {
    /// A family the host really has, spelled the way the host spells it
    Named(String),
    Mono,
    Serif,
    Sans,
}

#[derive(Clone)]
struct Spec {
    face: Face,
    weight: Weight,
    style: Style,
    px: f32,
}

impl Spec {
    fn family(&self) -> Family<'_> {
        match &self.face {
            Face::Named(s) => Family::Name(s),
            Face::Mono => Family::Monospace,
            Face::Serif => Family::Serif,
            Face::Sans => Family::SansSerif,
        }
    }
}

/// X families that are gone from a modern host but whose *shape* still matters:
/// falling a typewriter face back to something proportional would wreck every
/// console and every method body the world draws.
///
/// ponytail: a substring test over two lists, not a table of all 88 names in
/// `x11Globals fontFamily`. Add a name here when a world visibly picks wrong.
const MONO: &[&str] = &["typewriter", "mono", "courier", "terminus", "fixed", "console"];
const SERIF: &[&str] =
    &["times", "palatino", "palladio", "bembo", "schoolbook", "rockwell", "roman", "serif"];

/// fontdb compares family names exactly (`Database::query` is a `==` over the
/// face's own names), and the world spells them the way X did -- `helvetica`,
/// `lucidaTypewriter`. So resolve to the host's own spelling first, and only
/// fall back to a generic when the host has nothing by that name.
fn resolve(db: &cosmic_text::fontdb::Database, want: &str) -> Face {
    if want.is_empty() {
        return Face::Sans;
    }
    for f in db.faces() {
        for (name, _) in &f.families {
            if name.eq_ignore_ascii_case(want) {
                return Face::Named(name.clone());
            }
        }
    }
    let lower = want.to_ascii_lowercase();
    let has = |set: &[&str]| set.iter().any(|k| lower.contains(k));
    match () {
        _ if has(MONO) => Face::Mono,
        _ if has(SERIF) => Face::Serif,
        _ => Face::Sans,
    }
}

/// `-*-helvetica-medium-r-normal--12-*`. The world keeps a prefix per family and
/// appends the pixel size to it: `x11Globals fontFamily helvetica` is
/// `'-*-helvetica-medium-r-normal--'`. Fields are foundry, family, weight,
/// slant, setwidth, addstyle, pixelsize, so the four that matter sit at 2, 3, 4
/// and 7 once the leading `-` has produced an empty field 0. Anything that is
/// not an XLFD is taken as a bare family name.
struct Xlfd {
    family: String,
    weight: Weight,
    style: Style,
    px: f32,
}

fn parse_xlfd(name: &str) -> Xlfd {
    if !name.starts_with('-') {
        return Xlfd {
            family: name.to_string(),
            weight: Weight::NORMAL,
            style: Style::Normal,
            px: DEFAULT_PX,
        };
    }
    let f: Vec<&str> = name.split('-').collect();
    let at = |i: usize| f.get(i).copied().unwrap_or("*");
    Xlfd {
        family: match at(2) {
            "" | "*" => String::new(),
            s => s.to_string(),
        },
        weight: if at(3).eq_ignore_ascii_case("bold") { Weight::BOLD } else { Weight::NORMAL },
        style: match at(4) {
            "i" => Style::Italic,
            "o" => Style::Oblique,
            _ => Style::Normal,
        },
        px: at(7).parse().ok().filter(|p: &f32| *p > 0.0).unwrap_or(DEFAULT_PX),
    }
}

/// One string laid out once. `XTextWidth` reads its width and `XDrawString`
/// walks its glyphs, so the two cannot disagree -- which they must not, or a
/// text morph's cursor and its selection drift off the glyphs they belong to.
struct Laid {
    buffer: Buffer,
    width: i32,
    ascent: i32,
    descent: i32,
}

pub struct Fonts {
    /// real pixels per logical one. Everything is laid out and rasterised at
    /// that scale and divided back down on the way out, so the world keeps
    /// asking and answering in its own coordinates while the glyphs it gets are
    /// at the display's resolution.
    scale: f32,
    system: FontSystem,
    cache: SwashCache,
    specs: Vec<Spec>,
    // ponytail: the memo is unbounded. Morphic re-renders whole text morphs on
    // every damage event, so caching the layout is the whole win; add an LRU if
    // a world turns out to churn distinct strings.
    laid: HashMap<(usize, Vec<u8>), Laid>,
}

impl Fonts {
    /// Loads the host's installed fonts, which is the point of the exercise.
    pub fn new() -> Fonts {
        Fonts {
            scale: 1.0,
            system: FontSystem::new(),
            cache: SwashCache::new(),
            specs: vec![],
            laid: HashMap::new(),
        }
    }

    /// Tell it how many real pixels a logical one is worth.
    ///
    /// The scale is baked into the size every font was asked for, so a font
    /// loaded before this was known would be the wrong size for ever after --
    /// and silently, since nothing downstream can tell. Rescaling what is
    /// already loaded and dropping the memo makes the order not matter.
    pub fn set_scale(&mut self, scale: f32) {
        let scale = scale.max(1.0);
        if scale == self.scale {
            return;
        }
        let k = scale / self.scale;
        for s in &mut self.specs {
            s.px *= k;
        }
        self.laid.clear();
        self.scale = scale;
    }

    /// True when the host has no fonts at all, as a bare CI container does.
    pub fn is_empty(&self) -> bool {
        self.system.db().is_empty()
    }

    /// `XLoadQueryFont`. The id stands in for the `XFontStruct *`. A font the
    /// host does not have is not an error: it resolves to a generic and the
    /// world gets text either way, which is what X's own font server did.
    pub fn load(&mut self, xlfd: &str) -> usize {
        let x = parse_xlfd(xlfd);
        let face = resolve(self.system.db(), &x.family);
        // the size the world asked for is in its pixels, not the display's
        let px = x.px * self.scale;
        self.specs.push(Spec { face, weight: x.weight, style: x.style, px });
        self.specs.len() - 1
    }

    /// `XFontStruct` ascent and descent, so one probe glyph is enough to ask:
    /// cosmic-text takes both from the resolved face, not from the ink of the
    /// glyphs in the run. (A run that falls back to a second face for some
    /// character takes the larger of the two, which Latin-1 text will not do.)
    pub fn metrics(&mut self, f: usize) -> (i32, i32) {
        let (scale, l) = (self.scale, self.lay(f, b"M"));
        let down = |v: i32| (v as f32 / scale).ceil() as i32;
        (down(l.ascent), down(l.descent))
    }

    /// `XTextWidth`, in whole pixels because the caller is a 32-bit Self world.
    /// ponytail: rounded up to a whole logical pixel, so a run can be up to
    /// `scale - 1` real pixels wider than the box the world reserved for it.
    /// That is less than one of the world's own pixels, and rounding the other
    /// way clips the last glyph -- which is the visible failure.
    pub fn width(&mut self, f: usize, s: &[u8]) -> i32 {
        let (scale, l) = (self.scale, self.lay(f, s));
        (l.width as f32 / scale).ceil() as i32
    }

    /// `XDrawString(dpy, drawable, gc, x, y, s, n)`: `y` is the baseline, as X
    /// means it, and the colour and the font both come off the context.
    pub fn draw(&mut self, c: &mut Canvas, gc: &Gc, x: i32, y: i32, s: &[u8]) {
        let Some(f) = gc.font else { return };
        self.lay(f, s);
        let Some(l) = self.laid.get(&(f, s.to_vec())) else { return };
        // the layout is already at the display's resolution, so the pen starts
        // there too -- `Canvas::blend` takes real pixels for exactly this
        let scale = self.scale;
        let (x, y) = ((x as f32 * scale) as i32, (y as f32 * scale) as i32);
        let (system, cache) = (&mut self.system, &mut self.cache);
        for run in l.buffer.layout_runs() {
            for g in run.glyphs {
                // offset (0, 0) puts the glyphs where X wants them: relative to
                // the baseline, with no line box in the way
                let p = g.physical((0.0, 0.0), 1.0);
                let Some(img) = cache.get_image(system, p.cache_key).as_ref() else { continue };
                let (gw, gh) = (img.placement.width as i32, img.placement.height as i32);
                // ponytail: a colour bitmap is an emoji strike, and this draws
                // Latin-1 with one foreground colour. Skipped, not blended.
                if img.content != SwashContent::Mask || gw == 0 || gh == 0 {
                    continue;
                }
                let (ox, oy) = (x + p.x + img.placement.left, y + p.y - img.placement.top);
                for (i, &cov) in img.data.iter().enumerate() {
                    let i = i as i32;
                    c.blend(gc, ox + i % gw, oy + i / gw, gc.fg, cov);
                }
            }
        }
    }

    fn lay(&mut self, f: usize, s: &[u8]) -> &Laid {
        let key = (f, s.to_vec());
        if !self.laid.contains_key(&key) {
            let spec = self.specs[f].clone();
            // XDrawString is 8-bit and the world's strings are bytes. Latin-1 to
            // Unicode is the identity on code points, so this is the whole of
            // the conversion -- no table, and no door opened to UTF-8.
            let text: String = s.iter().map(|&b| b as char).collect();
            let mut buf = Buffer::new(&mut self.system, Metrics::new(spec.px, spec.px));
            buf.set_wrap(Wrap::None);
            buf.set_size(None, None);
            let attrs = Attrs::new().family(spec.family()).weight(spec.weight).style(spec.style);
            buf.set_text(&text, &attrs, Shaping::Advanced, None);
            buf.shape_until_scroll(&mut self.system, false);
            let (width, ascent, descent) =
                match buf.line_layout(&mut self.system, 0).and_then(|l| l.first()) {
                    Some(l) => {
                        (l.w.ceil() as i32, l.max_ascent.ceil() as i32, l.max_descent.ceil() as i32)
                    }
                    None => (0, spec.px.ceil() as i32, 0),
                };
            self.laid.insert(key.clone(), Laid { buffer: buf, width, ascent, descent });
        }
        &self.laid[&key]
    }
}

impl Default for Fonts {
    fn default() -> Fonts {
        Fonts::new()
    }
}

/// Render a sheet of the fonts the world actually asks for, with each line's
/// ascent/descent box drawn around it, so the metrics can be checked by eye
/// before there is any window to put them in.
pub fn demo(path: &str) -> std::io::Result<()> {
    let mut fonts = Fonts::new();
    if fonts.is_empty() {
        eprintln!("serf: no fonts installed on this host");
        return Ok(());
    }
    let names = [
        "-*-helvetica-medium-r-normal--12-*",
        "-*-helvetica-bold-r-normal--12-*",
        "-*-helvetica-medium-i-normal--12-*",
        "-*-times-medium-r-normal--14-*",
        "-*-courier-medium-r-normal--12-*",
        "-*-lucidaTypewriter-medium-r-normal--12-*",
        "fixed",
        "-*-helvetica-medium-r-normal--24-*",
    ];
    let sample = b"Handgloves 0123 The quick brown fox; jump!";

    let mut c = Canvas::new(760, 40 * names.len() as i32 + 20, 0xFFFFFF);
    let mut gc = Gc::default();
    for (i, name) in names.iter().enumerate() {
        let f = fonts.load(name);
        let (a, d) = fonts.metrics(f);
        let base = 40 * i as i32 + 28;
        let w = fonts.width(f, sample);
        // the box the world will clip this line to, and the baseline in it
        for x in 10..10 + w {
            c.blend(&gc, x, base - a, 0x000000, 40);
            c.blend(&gc, x, base + d, 0x000000, 40);
            c.blend(&gc, x, base, 0xFF0000, 60);
        }
        gc.font = Some(f);
        fonts.draw(&mut c, &gc, 10, base, sample);
        let label = format!("{}  w={} a={} d={}", name, w, a, d);
        gc.font = Some(fonts.load("-*-courier-medium-r-normal--9-*"));
        gc.fg = 0x0066AA;
        fonts.draw(&mut c, &gc, 10, base - a - 4, label.as_bytes());
        gc.fg = 0x000000;
    }
    crate::canvas::write_png(path, &c)?;
    eprintln!("wrote {} ({}x{})", path, c.w, c.h);
    Ok(())
}

/// Every drawing call the world makes, on one sheet, so the raster ops can be
/// checked by eye before there is any window to put them in. Each panel is
/// labelled with the X call it stands for.
pub fn draw_demo(path: &str) -> std::io::Result<()> {
    let c = draw_sheet();
    crate::canvas::write_png(path, &c)?;
    eprintln!("wrote {} ({}x{})", path, c.w, c.h);
    Ok(())
}

/// The same sheet as a canvas, for a window to put on the screen.
pub fn draw_sheet() -> Canvas {
    use crate::canvas::{alloc_color, Drawables, Func, Rect};

    let mut fonts = Fonts::new();
    let label_font =
        if fonts.is_empty() { None } else { Some(fonts.load("-*-courier-medium-r-normal--9-*")) };
    let mut d = Drawables::default();
    let win = d.add(Canvas::new(780, 430, 0xF2F2F2));
    let mut g = Gc::default();

    // XAllocColor on a truecolor visual is the identity, which is the whole of
    // x11Globals platformColormap here
    let blue = alloc_color(0x2200, 0x5500, 0xAA00);
    let red = alloc_color(0xCC00, 0x2200, 0x2200);

    let say = |d: &mut Drawables, fonts: &mut Fonts, x: i32, y: i32, s: &str| {
        if let Some(f) = label_font {
            let g = Gc { fg: 0x666666, font: Some(f), ..Gc::default() };
            fonts.draw(d.get_mut(win), &g, x, y, s.as_bytes());
        }
    };

    // --- points, lines, polyline
    say(&mut d, &mut fonts, 20, 26, "XDrawPoint  XDrawLine  XDrawLines");
    g.fg = 0x000000;
    for i in 0..40 {
        d.get_mut(win).point(&g, 20 + i * 3, 40 + (i % 5) * 3);
    }
    g.fg = blue;
    for i in 0..6 {
        d.get_mut(win).line(&g, 20, 60, 20 + i * 30, 110);
    }
    g.fg = red;
    g.line_width = 3;
    let (xs, ys): (Vec<i32>, Vec<i32>) =
        (0..12).map(|i| (200 + i * 12, 60 + if i % 2 == 0 { 0 } else { 40 })).unzip();
    d.get_mut(win).lines(&g, &xs, &ys);
    g.line_width = 1;

    // --- rectangles, filled and outlined, and a polygon
    say(&mut d, &mut fonts, 370, 26, "XFillRectangle  XDrawRectangle  XFillPolygon");
    g.fg = blue;
    d.get_mut(win).fill_rect(&g, 370, 40, 70, 50);
    g.fg = 0x000000;
    d.get_mut(win).draw_rect(&g, 455, 40, 70, 50);
    g.fg = red;
    d.get_mut(win).fill_polygon(&g, &[545, 615, 580, 545], &[90, 90, 40, 65]);

    // --- arcs, drawn and filled
    say(&mut d, &mut fonts, 20, 146, "XDrawArc  XFillArc");
    g.fg = 0x000000;
    d.get_mut(win).arc(&g, 20, 155, 70, 70, 0, 360 * 64, false);
    g.fg = blue;
    d.get_mut(win).arc(&g, 100, 155, 70, 70, 45 * 64, 270 * 64, false);
    g.fg = red;
    d.get_mut(win).arc(&g, 180, 155, 70, 70, 30 * 64, 300 * 64, true);
    g.fg = 0x338833;
    d.get_mut(win).arc(&g, 260, 155, 90, 70, 0, 360 * 64, true);

    // --- clipping: the same fill twice, once with a clip rectangle
    say(&mut d, &mut fonts, 370, 146, "XSetClipRectangle");
    g.fg = 0xBBBBBB;
    d.get_mut(win).fill_rect(&g, 370, 155, 120, 70);
    g.fg = red;
    g.clip = Some(Rect { x: 0, y: 0, w: 60, h: 35 });
    g.clip_origin = (390, 170);
    d.get_mut(win).fill_rect(&g, 370, 155, 120, 70);
    g.clear_clip();

    // --- xor, drawn twice on the right half so half of it disappears
    say(&mut d, &mut fonts, 520, 146, "GXxor, twice");
    g.fg = 0x000000;
    d.get_mut(win).fill_rect(&g, 520, 155, 120, 70);
    g.fg = 0x00FFFF;
    g.func = Func::Xor;
    d.get_mut(win).fill_rect(&g, 520, 155, 120, 70);
    d.get_mut(win).fill_rect(&g, 580, 155, 60, 70);
    g.func = Func::Copy;

    // --- a pixmap drawn into and copied back, which is how Morphic paints
    say(&mut d, &mut fonts, 20, 266, "XCreatePixmap + XCopyArea, tiled");
    let pat = d.add(Canvas::new(16, 16, 0xFFFFFF));
    g.fg = 0x2255AA;
    for y in 0..16 {
        for x in 0..16 {
            if (x + y) % 4 == 0 {
                d.get_mut(pat).point(&g, x, y);
            }
        }
    }
    for ty in 0..4 {
        for tx in 0..14 {
            d.copy_area(&g, pat, 0, 0, 16, 16, win, 20 + tx * 16, 275 + ty * 16);
        }
    }

    // --- an XImage filled pixel by pixel and put back
    say(&mut d, &mut fonts, 270, 266, "XCreateImage  XPutPixel");
    let img = d.add(Canvas::new(120, 64, 0));
    for y in 0..64 {
        for x in 0..120 {
            let v = alloc_color((x * 546) as u16, (y * 1024) as u16, 0x8000);
            d.get_mut(img).put(x, y, v);
        }
    }
    d.copy_area(&g, img, 0, 0, 120, 64, win, 270, 275);

    // --- a self-overlapping blit, the case a naive one gets wrong
    say(&mut d, &mut fonts, 420, 266, "XCopyArea onto itself; XClearArea");
    d.copy_area(&g, win, 270, 275, 120, 64, win, 420, 275);
    d.copy_area(&g, win, 420, 275, 100, 64, win, 440, 275);
    d.get_mut(win).clear_area(560, 275, 100, 64);

    // --- text, on the same surface, through the same context
    say(&mut d, &mut fonts, 20, 380, "XSetFont  XDrawString");
    if !fonts.is_empty() {
        let f = fonts.load("-*-helvetica-medium-r-normal--18-*");
        g.font = Some(f);
        g.fg = 0x000000;
        fonts.draw(d.get_mut(win), &g, 20, 410, b"Handgloves 0123 -- the same canvas, one context");
    }

    d.take(win)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xlfd_fields_the_world_sends() {
        // x11Globals fontFamily helveticaBold, with a size appended
        let s = parse_xlfd("-*-helvetica-bold-r-normal--12-*");
        assert_eq!(s.family, "helvetica");
        assert_eq!(s.weight, Weight::BOLD);
        assert_eq!(s.style, Style::Normal);
        assert_eq!(s.px, 12.0);
        let s = parse_xlfd("-*-times-medium-i-normal--14-*");
        assert_eq!((s.family.as_str(), s.style, s.px), ("times", Style::Italic, 14.0));
        // a bare name, and a malformed one, both have to answer something
        assert_eq!(parse_xlfd("fixed").px, DEFAULT_PX);
        assert_eq!(parse_xlfd("-*-*-*-*-*--*-*").px, DEFAULT_PX);
    }

    /// A family the host does not have must still keep its shape, or the
    /// world's consoles come back proportional.
    #[test]
    fn missing_families_fall_back_by_shape() {
        let db = cosmic_text::fontdb::Database::new(); // empty: nothing resolves
        assert_eq!(resolve(&db, "lucidaTypewriter"), Face::Mono);
        assert_eq!(resolve(&db, "fixed"), Face::Mono);
        assert_eq!(resolve(&db, "newCenturySchoolbook"), Face::Serif);
        assert_eq!(resolve(&db, "helveticaNarrow"), Face::Sans);
        assert_eq!(resolve(&db, ""), Face::Sans);
    }

    /// The host's own spelling wins over any of that, case aside -- fontdb
    /// matches family names with `==`, and the world shouts them in X's casing.
    #[test]
    fn host_families_resolve_case_insensitively() {
        let fonts = Fonts::new();
        if fonts.is_empty() {
            return;
        }
        let db = fonts.system.db();
        let first = db.faces().next().unwrap().families[0].0.clone();
        assert_eq!(resolve(db, &first.to_ascii_uppercase()), Face::Named(first));
    }

    /// The contract the world is owed: what `XTextWidth` promises is where
    /// `XDrawString` actually puts ink, inside the ascent/descent box. A drift
    /// here is what walks a text morph's cursor off its own glyphs.
    #[test]
    fn drawn_ink_stays_inside_the_measured_box() {
        let mut fonts = Fonts::new();
        if fonts.is_empty() {
            return; // no fonts installed; nothing to check
        }
        let f = fonts.load("-*-helvetica-medium-r-normal--12-*");
        let (s, x, base) = (b"Handgloves jgq".as_slice(), 20, 40);
        let (w, (a, d)) = (fonts.width(f, s), fonts.metrics(f));
        assert!(w > 0 && a > 0 && d > 0, "empty metrics: w={} a={} d={}", w, a, d);
        assert!(fonts.width(f, b"Handgloves jgq!") > w, "width is not monotonic");

        let mut c = Canvas::new(200, 80, 0xFFFFFF);
        fonts.draw(&mut c, &Gc { fg: 0x000000, font: Some(f), ..Gc::default() }, x, base, s);
        let (mut ink, mut escaped) = (0, 0);
        for y in 0..c.h {
            for px in 0..c.w {
                if c.px[(y * c.w + px) as usize] != 0xFFFFFF {
                    ink += 1;
                    // one pixel of slack each way: antialiasing and a glyph's
                    // side bearing may both reach just past the advance
                    if px < x - 1 || px > x + w + 1 || y < base - a - 1 || y > base + d + 1 {
                        escaped += 1;
                    }
                }
            }
        }
        assert!(ink > 50, "text drew {} pixels of ink", ink);
        assert_eq!(escaped, 0, "{} of {} ink pixels fell outside the box", escaped, ink);
    }
}
