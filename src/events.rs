//! Input for the native canvas: the queue behind `XPending` and `XNextEvent`,
//! and the bytes the world reads out of an `XEvent`.
//!
//! The image does not read an event through a function. It allocates 192 bytes
//! with `XEvent_new`, has the server fill them, and then loads fields straight
//! out of that buffer -- `XButtonEvent_xx` is a four-byte read at offset 64.
//! So an event here is not a struct the world is handed, it is a *layout* that
//! has to match the one `src/struct_table.rs` records, and the encoder below
//! writes through that same table. Regenerate the table and both sides move
//! together.
//!
//! Codes and masks are X's own, checked against `X11/X.h` and `keysymdef.h`.

use crate::struct_table::FIELD;

/// Event type codes, `X.h`.
pub const KEY_PRESS: i64 = 2;
pub const KEY_RELEASE: i64 = 3;
pub const BUTTON_PRESS: i64 = 4;
pub const BUTTON_RELEASE: i64 = 5;
pub const MOTION_NOTIFY: i64 = 6;
pub const ENTER_NOTIFY: i64 = 7;
pub const LEAVE_NOTIFY: i64 = 8;
pub const FOCUS_IN: i64 = 9;
pub const FOCUS_OUT: i64 = 10;
pub const EXPOSE: i64 = 12;
pub const CONFIGURE_NOTIFY: i64 = 22;
pub const CLIENT_MESSAGE: i64 = 33;

/// The masks `XSelectInput` takes, `X.h`.
pub const KEY_PRESS_MASK: u64 = 1 << 0;
pub const KEY_RELEASE_MASK: u64 = 1 << 1;
pub const BUTTON_PRESS_MASK: u64 = 1 << 2;
pub const BUTTON_RELEASE_MASK: u64 = 1 << 3;
pub const ENTER_WINDOW_MASK: u64 = 1 << 4;
pub const LEAVE_WINDOW_MASK: u64 = 1 << 5;
pub const POINTER_MOTION_MASK: u64 = 1 << 6;
pub const EXPOSURE_MASK: u64 = 1 << 15;
pub const STRUCTURE_NOTIFY_MASK: u64 = 1 << 17;
pub const FOCUS_CHANGE_MASK: u64 = 1 << 21;

/// Modifier and button bits in an event's `state`, `X.h`.
pub const SHIFT_MASK: u32 = 1 << 0;
pub const CONTROL_MASK: u32 = 1 << 2;
pub const MOD1_MASK: u32 = 1 << 3;
pub const BUTTON1_MASK: u32 = 1 << 8;

/// Keysyms for the keys that are not simply their own character,
/// `keysymdef.h`. A printable Latin-1 key's keysym *is* its code point, which
/// is why there is no table for those. The rest arrive when the window backend
/// has a key of its own to translate.
pub const XK_BACKSPACE: u32 = 0xff08;
pub const XK_TAB: u32 = 0xff09;
pub const XK_RETURN: u32 = 0xff0d;
pub const XK_ESCAPE: u32 = 0xff1b;
pub const XK_LEFT: u32 = 0xff51;
pub const XK_UP: u32 = 0xff52;
pub const XK_RIGHT: u32 = 0xff53;
pub const XK_DOWN: u32 = 0xff54;
pub const XK_DELETE: u32 = 0xffff;

/// An `XEvent` is 192 bytes, whatever is in it (`struct_table::ALLOC`).
pub const EVENT_BYTES: usize = 192;

/// Where `time` sits in every core event struct -- window at 32, root at 40,
/// subwindow at 48, then the timestamp. `struct_table` has no entry for it
/// because the world does not read it through an accessor: `prims.rs` answers
/// `xButtonEvent_time` and friends by loading this offset directly.
const TIME_OFFSET: usize = 56;

/// Milliseconds since the first event was encoded, which is all an X timestamp
/// has to be: the world only ever takes differences.
///
/// Leaving this at zero is not a cosmetic omission. Morphic tells a click from
/// a double click from a press-and-hold by *when* the events arrived, so a
/// frozen clock makes every gesture look the same -- a single click does
/// nothing at all, and two in a row come out as something else entirely.
fn now_ms() -> u64 {
    use std::sync::OnceLock;
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pointer {
    pub x: i32,
    pub y: i32,
    pub x_root: i32,
    pub y_root: i32,
    pub state: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Expose {
        window: u64,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        count: i32,
    },
    Button {
        press: bool,
        window: u64,
        at: Pointer,
        button: u32,
    },
    Motion {
        window: u64,
        at: Pointer,
    },
    /// `keysym` is X's name for the key. For a printable key that name *is*
    /// the character it produced, shift already applied, which is why no text
    /// rides along: `lookup_string` recovers it from the encoded event.
    Key {
        press: bool,
        window: u64,
        at: Pointer,
        keysym: u32,
    },
    Crossing {
        enter: bool,
        window: u64,
        x: i32,
        y: i32,
    },
    Focus {
        entering: bool,
        window: u64,
    },
    Configure {
        window: u64,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    },
    /// What a close box sends, once the world has asked for `WM_DELETE_WINDOW`
    ClientMessage {
        window: u64,
        message_type: u64,
        format: i32,
    },
}

impl Event {
    pub fn kind(&self) -> i64 {
        match self {
            Event::Expose { .. } => EXPOSE,
            Event::Button { press: true, .. } => BUTTON_PRESS,
            Event::Button { .. } => BUTTON_RELEASE,
            Event::Motion { .. } => MOTION_NOTIFY,
            Event::Key { press: true, .. } => KEY_PRESS,
            Event::Key { .. } => KEY_RELEASE,
            Event::Crossing { enter: true, .. } => ENTER_NOTIFY,
            Event::Crossing { .. } => LEAVE_NOTIFY,
            Event::Focus { entering: true, .. } => FOCUS_IN,
            Event::Focus { .. } => FOCUS_OUT,
            Event::Configure { .. } => CONFIGURE_NOTIFY,
            Event::ClientMessage { .. } => CLIENT_MESSAGE,
        }
    }

    pub fn mask(&self) -> u64 {
        mask_of(self.kind())
    }

    /// A keycode for the world to read out of `XKeyEvent`. A native window
    /// system has already turned scan codes into characters, so there is no
    /// host keycode left to pass on -- these are derived from the keysym, which
    /// makes them stable per key but not the host's own numbering. Nothing in
    /// Morphic reads them for anything but telling one key from another.
    ///
    /// Which is the whole difficulty: the two ranges have to stay apart.
    /// Folding both into the low byte puts Return (0xff0d) on the same keycode
    /// as Ctrl-M (0x0d), and every other function key on top of a control
    /// character, so a world that told them apart by keycode would stop being
    /// able to. X keycodes are conventionally 8..255; these run past that
    /// rather than collide, and the field is 32 bits wide.
    pub fn keycode(keysym: u32) -> u32 {
        if keysym >= 0xff00 {
            512 + (keysym & 0xff)
        } else {
            8 + (keysym & 0xff)
        }
    }

    /// ...and back, because `XLookupString` is handed the event and nothing
    /// else. The two bands are disjoint so this is exact.
    pub fn keysym(keycode: u32) -> u32 {
        if keycode >= 512 {
            0xff00 | (keycode - 512)
        } else {
            keycode.wrapping_sub(8)
        }
    }
}

/// Which `XSelectInput` bit asks for an event of this type. Taken from the
/// type code rather than from the event, because by the time the queue is
/// filtering, an event is 192 bytes and nothing else. A `ClientMessage` is
/// delivered whatever the mask says, as X delivers it.
pub fn mask_of(kind: i64) -> u64 {
    match kind {
        EXPOSE => EXPOSURE_MASK,
        BUTTON_PRESS => BUTTON_PRESS_MASK,
        BUTTON_RELEASE => BUTTON_RELEASE_MASK,
        MOTION_NOTIFY => POINTER_MOTION_MASK,
        KEY_PRESS => KEY_PRESS_MASK,
        KEY_RELEASE => KEY_RELEASE_MASK,
        ENTER_NOTIFY => ENTER_WINDOW_MASK,
        LEAVE_NOTIFY => LEAVE_WINDOW_MASK,
        FOCUS_IN | FOCUS_OUT => FOCUS_CHANGE_MASK,
        CONFIGURE_NOTIFY => STRUCTURE_NOTIFY_MASK,
        _ => !0,
    }
}

/// Write `v` into the field the world will read, at the offset and width
/// `struct_table` recorded for it. Silently does nothing for a field the table
/// has no entry for, which means the world could not have read it either.
///
/// ponytail: a linear scan of the table per field. It is ~60 entries and an
/// event is encoded once, against thousands of pixels drawn; index it if a
/// profile ever disagrees.
fn put(buf: &mut [u8], strukt: &str, field: &str, v: u64) {
    let Some(e) = FIELD.iter().find(|e| e.1 == strukt && e.2 == field) else { return };
    let (at, n) = (e.3, e.4);
    if at + n > buf.len() {
        return;
    }
    buf[at..at + n].copy_from_slice(&v.to_le_bytes()[..n]);
}

/// Read one back the way the world does, for the checks below and `--event-demo`.
pub fn get(buf: &[u8], strukt: &str, field: &str) -> u64 {
    let Some(e) = FIELD.iter().find(|e| e.1 == strukt && e.2 == field) else { return 0 };
    let (at, n) = (e.3, e.4);
    if at + n > buf.len() {
        return 0;
    }
    let mut w = [0u8; 8];
    w[..n].copy_from_slice(&buf[at..at + n]);
    u64::from_le_bytes(w)
}

/// Lay an event out as Xlib would, so the image's field reads find what they
/// expect. `buf` is the world's own `XEvent` allocation.
pub fn encode(e: &Event, buf: &mut [u8]) {
    encode_at(e, buf, now_ms())
}

/// The same, with the timestamp supplied, so a check can pin it down.
pub fn encode_at(e: &Event, buf: &mut [u8], time: u64) {
    buf.iter_mut().for_each(|b| *b = 0);
    put(buf, "XEvent", "type", e.kind() as u64);
    // every event the pointer or keyboard produces carries one; Expose,
    // ConfigureNotify and the rest have other fields at that offset
    if matches!(
        e,
        Event::Button { .. } | Event::Motion { .. } | Event::Key { .. } | Event::Crossing { .. }
    ) && buf.len() >= TIME_OFFSET + 8
    {
        buf[TIME_OFFSET..TIME_OFFSET + 8].copy_from_slice(&time.to_le_bytes());
    }
    let pointer = |buf: &mut [u8], s: &str, at: &Pointer| {
        put(buf, s, "x", at.x as u32 as u64);
        put(buf, s, "y", at.y as u32 as u64);
        put(buf, s, "x_root", at.x_root as u32 as u64);
        put(buf, s, "y_root", at.y_root as u32 as u64);
        put(buf, s, "state", at.state as u64);
    };
    match e {
        Event::Expose { window, x, y, width, height, count } => {
            let s = "XExposeEvent";
            put(buf, s, "window", *window);
            put(buf, s, "x", *x as u32 as u64);
            put(buf, s, "y", *y as u32 as u64);
            put(buf, s, "width", *width as u32 as u64);
            put(buf, s, "height", *height as u32 as u64);
            put(buf, s, "count", *count as u32 as u64);
        }
        Event::Button { window, at, button, .. } => {
            put(buf, "XButtonEvent", "window", *window);
            pointer(buf, "XButtonEvent", at);
            put(buf, "XButtonEvent", "button", *button as u64);
        }
        Event::Motion { window, at } => {
            put(buf, "XMotionEvent", "window", *window);
            pointer(buf, "XMotionEvent", at);
            // never a hint: this backend delivers every motion it is given, so
            // the world must not be told to go ask where the pointer really is
            put(buf, "XMotionEvent", "is_hint", 0);
        }
        Event::Key { window, at, keysym, .. } => {
            put(buf, "XKeyEvent", "window", *window);
            pointer(buf, "XKeyEvent", at);
            put(buf, "XKeyEvent", "keycode", Event::keycode(*keysym) as u64);
        }
        Event::Crossing { window, x, y, .. } => {
            put(buf, "XCrossingEvent", "window", *window);
            put(buf, "XCrossingEvent", "x", *x as u32 as u64);
            put(buf, "XCrossingEvent", "y", *y as u32 as u64);
        }
        Event::Focus { window, .. } => {
            put(buf, "XFocusChangeEvent", "window", *window);
            // NotifyNormal / NotifyAncestor, which is what a plain focus change
            // is and what the world's handler tests for
            put(buf, "XFocusChangeEvent", "mode", 0);
            put(buf, "XFocusChangeEvent", "detail", 0);
        }
        Event::Configure { window, x, y, width, height } => {
            let s = "XConfigureEvent";
            put(buf, s, "window", *window);
            put(buf, s, "x", *x as u32 as u64);
            put(buf, s, "y", *y as u32 as u64);
            put(buf, s, "width", *width as u32 as u64);
            put(buf, s, "height", *height as u32 as u64);
        }
        Event::ClientMessage { window, message_type, format } => {
            put(buf, "XClientMessageEvent", "window", *window);
            put(buf, "XClientMessageEvent", "message_type", *message_type);
            put(buf, "XClientMessageEvent", "format", *format as u32 as u64);
        }
    }
}

/// `XLookupString`: the bytes a key produced, and the keysym for the key.
///
/// It takes the encoded event, because that is all the world hands it -- the
/// `XEvent` proxy it was filling in. So the keysym has to come back out of the
/// keycode, which is what makes that mapping's two bands worth keeping apart.
///
/// ponytail: Latin-1 only, because the call is the 8-bit one and the world's
/// strings are bytes. A key with no printable character of its own answers no
/// bytes and its keysym, which is what X does for a function key -- so nothing
/// downstream meets a case it has not already seen.
pub fn lookup_string(buf: &[u8]) -> (Vec<u8>, u32) {
    let t = get(buf, "XEvent", "type") as i64;
    if t != KEY_PRESS && t != KEY_RELEASE {
        return (vec![], 0);
    }
    let keysym = Event::keysym(get(buf, "XKeyEvent", "keycode") as u32);
    let printable = matches!(keysym, 0x20..=0x7e | 0xa0..=0xff);
    (if printable { vec![keysym as u8] } else { vec![] }, keysym)
}

/// The queue behind `XPending`, `XNextEvent` and friends.
///
/// It holds *encoded* events, not Rust ones, because that is the form the world
/// deals in: `XPutBackEvent` hands back the same 192 bytes `XNextEvent` filled,
/// and there is no way to recover a Rust value from them. Filtering reads the
/// type code out of the bytes, which is all `XCheckMaskEvent` needs.
#[derive(Default)]
pub struct Events {
    q: std::collections::VecDeque<[u8; EVENT_BYTES]>,
    /// what `XSelectInput` last asked for
    selected: u64,
}

impl Events {
    pub fn new() -> Events {
        Events::default()
    }

    /// `XSelectInput`. Events the world has not asked for are dropped on the
    /// way in rather than filtered on the way out, so `XPending` answers the
    /// number the world will actually get.
    pub fn select(&mut self, mask: u64) {
        self.selected = mask;
    }

    /// Offer an event from the window system. Answers whether it was taken.
    pub fn push(&mut self, e: Event) -> bool {
        if e.mask() & self.selected == 0 {
            return false;
        }
        let mut buf = [0u8; EVENT_BYTES];
        encode(&e, &mut buf);
        self.q.push_back(buf);
        true
    }

    /// What `XSelectInput` last asked for.
    pub fn selected(&self) -> u64 {
        self.selected
    }

    /// `XPending` and `XEventsQueued`.
    pub fn pending(&self) -> usize {
        self.q.len()
    }

    /// `XNextEvent`, which in X blocks; here it answers `false` on an empty
    /// queue and the caller decides how to wait.
    pub fn next_into(&mut self, buf: &mut [u8]) -> bool {
        match self.q.pop_front() {
            Some(e) => copy_into(&e, buf),
            None => false,
        }
    }

    /// `XPeekEvent`: the same, without taking it.
    pub fn peek_into(&self, buf: &mut [u8]) -> bool {
        match self.q.front() {
            Some(e) => copy_into(e, buf),
            None => false,
        }
    }

    /// `XCheckMaskEvent`: take the first event matching the mask, wherever it
    /// is in the queue, and leave the ones it skipped in the order they came.
    pub fn check_mask(&mut self, mask: u64, buf: &mut [u8]) -> bool {
        self.take(buf, |k| mask_of(k) & mask != 0)
    }

    /// `XCheckTypedEvent`.
    pub fn check_typed(&mut self, kind: i64, buf: &mut [u8]) -> bool {
        self.take(buf, |k| k == kind)
    }

    fn take(&mut self, buf: &mut [u8], want: impl Fn(i64) -> bool) -> bool {
        let at = self.q.iter().position(|e| want(get(e, "XEvent", "type") as i64));
        match at.and_then(|i| self.q.remove(i)) {
            Some(e) => copy_into(&e, buf),
            None => false,
        }
    }

    /// `XPutBackEvent`: to the head, so the next `XNextEvent` sees it again.
    /// It takes the world's own buffer, which is the only thing it has.
    pub fn put_back(&mut self, buf: &[u8]) {
        let mut e = [0u8; EVENT_BYTES];
        let n = buf.len().min(EVENT_BYTES);
        e[..n].copy_from_slice(&buf[..n]);
        self.q.push_front(e);
    }
}

fn copy_into(e: &[u8; EVENT_BYTES], buf: &mut [u8]) -> bool {
    let n = buf.len().min(EVENT_BYTES);
    buf[..n].copy_from_slice(&e[..n]);
    true
}

/// Run a scripted sequence through the queue and print each event the way the
/// image reads it -- through `struct_table`'s own offsets -- so the layout can
/// be checked without a window to click in.
pub fn demo() {
    let win = 0x2a00_0001u64;
    let at = |x, y, state| Pointer { x, y, x_root: x + 100, y_root: y + 40, state };
    let script = [
        Event::Expose { window: win, x: 0, y: 0, width: 640, height: 480, count: 0 },
        Event::Crossing { enter: true, window: win, x: 12, y: 34 },
        Event::Motion { window: win, at: at(120, 200, 0) },
        Event::Button { press: true, window: win, at: at(120, 200, 0), button: 1 },
        Event::Motion { window: win, at: at(126, 204, BUTTON1_MASK) },
        Event::Button { press: false, window: win, at: at(126, 204, BUTTON1_MASK), button: 1 },
        Event::Key { press: true, window: win, at: at(126, 204, SHIFT_MASK), keysym: 'A' as u32 },
        // not selected below, so the queue never sees it
        Event::Key { press: false, window: win, at: at(126, 204, SHIFT_MASK), keysym: 'A' as u32 },
        // the keys with no text of their own, which is what an editor steers by
        Event::Key { press: true, window: win, at: at(126, 204, 0), keysym: XK_RETURN },
        Event::Key { press: true, window: win, at: at(126, 204, 0), keysym: XK_TAB },
        Event::Key { press: true, window: win, at: at(126, 204, 0), keysym: XK_ESCAPE },
        Event::Key { press: true, window: win, at: at(126, 204, 0), keysym: XK_BACKSPACE },
        Event::Key { press: true, window: win, at: at(126, 204, 0), keysym: XK_LEFT },
        Event::Key { press: true, window: win, at: at(126, 204, 0), keysym: XK_UP },
        Event::Key { press: true, window: win, at: at(126, 204, 0), keysym: XK_RIGHT },
        Event::Key { press: true, window: win, at: at(126, 204, 0), keysym: XK_DOWN },
        Event::Key { press: true, window: win, at: at(126, 204, 0), keysym: XK_DELETE },
        Event::Configure { window: win, x: 0, y: 0, width: 800, height: 600 },
        Event::Focus { entering: false, window: win },
        Event::ClientMessage { window: win, message_type: 0x111, format: 32 },
    ];

    let mut q = Events::new();
    // what the world selects: everything but key release, which Morphic does
    // not ask for -- so the queue drops those on the way in
    q.select(
        EXPOSURE_MASK
            | STRUCTURE_NOTIFY_MASK
            | FOCUS_CHANGE_MASK
            | ENTER_WINDOW_MASK
            | LEAVE_WINDOW_MASK
            | POINTER_MOTION_MASK
            | BUTTON_PRESS_MASK
            | BUTTON_RELEASE_MASK
            | KEY_PRESS_MASK,
    );
    let offered = script.len();
    let taken = script.into_iter().filter(|e| q.push(e.clone())).count();
    println!("XSelectInput: {} of {} offered events queued", taken, offered);
    println!("XPending -> {}", q.pending());

    let mut buf = [0u8; EVENT_BYTES];

    // a world checks for the close message before it settles into its loop,
    // and XCheckTypedEvent reaches past everything in front of it
    if q.check_typed(CLIENT_MESSAGE, &mut buf) {
        println!(
            "XCheckTypedEvent(ClientMessage) -> window={:#x} message_type={:#x}, {} left",
            get(&buf, "XClientMessageEvent", "window"),
            get(&buf, "XClientMessageEvent", "message_type"),
            q.pending()
        );
    }
    // and handles a resize before repainting to the old size
    if q.check_mask(STRUCTURE_NOTIFY_MASK, &mut buf) {
        println!(
            "XCheckMaskEvent(StructureNotify) -> {}x{}, {} left",
            get(&buf, "XConfigureEvent", "width"),
            get(&buf, "XConfigureEvent", "height"),
            q.pending()
        );
    }
    // XPeekEvent looks without taking; XPutBackEvent undoes an XNextEvent
    q.peek_into(&mut buf);
    let head = get(&buf, "XEvent", "type");
    q.next_into(&mut buf);
    q.put_back(&buf);
    println!("XPeekEvent -> type={}, put back after XNextEvent -> {} pending\n", head, q.pending());

    while q.next_into(&mut buf) {
        let t = get(&buf, "XEvent", "type") as i64;
        let (name, s, fields): (&str, &str, &[&str]) = match t {
            EXPOSE => ("Expose", "XExposeEvent", &["x", "y", "width", "height", "count"]),
            BUTTON_PRESS | BUTTON_RELEASE => {
                ("Button", "XButtonEvent", &["x", "y", "x_root", "y_root", "state", "button"])
            }
            MOTION_NOTIFY => {
                ("Motion", "XMotionEvent", &["x", "y", "x_root", "y_root", "state", "is_hint"])
            }
            KEY_PRESS | KEY_RELEASE => ("Key", "XKeyEvent", &["state", "keycode"]),
            ENTER_NOTIFY | LEAVE_NOTIFY => ("Crossing", "XCrossingEvent", &["x", "y"]),
            FOCUS_IN | FOCUS_OUT => ("Focus", "XFocusChangeEvent", &["mode", "detail"]),
            CONFIGURE_NOTIFY => ("Configure", "XConfigureEvent", &["x", "y", "width", "height"]),
            CLIENT_MESSAGE => ("ClientMessage", "XClientMessageEvent", &["message_type", "format"]),
            _ => ("?", "XEvent", &[]),
        };
        let mut vals: Vec<String> =
            fields.iter().map(|f| format!("{}={}", f, get(&buf, s, f))).collect();
        if matches!(t, KEY_PRESS | KEY_RELEASE) {
            let (text, keysym) = lookup_string(&buf);
            vals.push(format!(
                "XLookupString -> keysym={:#06x} text={:?}",
                keysym,
                String::from_utf8_lossy(&text)
            ));
        }
        println!(
            "  type={:<2} {:<13} window={:#x} {}",
            t,
            name,
            get(&buf, s, "window"),
            vals.join(" ")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win() -> u64 {
        0x2a00_0001
    }

    /// The whole contract: what the encoder writes is what the image's field
    /// reads find. Every value is distinct, so writing `x` where the world
    /// reads `x_root` -- or a signed field that lost its sign -- fails here.
    #[test]
    fn the_world_reads_back_what_was_encoded() {
        let mut b = [0u8; EVENT_BYTES];
        let at = Pointer { x: 11, y: 22, x_root: 33, y_root: 44, state: BUTTON1_MASK };

        encode(&Event::Button { press: true, window: win(), at, button: 3 }, &mut b);
        assert_eq!(get(&b, "XEvent", "type"), BUTTON_PRESS as u64);
        assert_eq!(get(&b, "XButtonEvent", "window"), win());
        assert_eq!(get(&b, "XButtonEvent", "x"), 11);
        assert_eq!(get(&b, "XButtonEvent", "y"), 22);
        assert_eq!(get(&b, "XButtonEvent", "x_root"), 33);
        assert_eq!(get(&b, "XButtonEvent", "y_root"), 44);
        assert_eq!(get(&b, "XButtonEvent", "state"), BUTTON1_MASK as u64);
        assert_eq!(get(&b, "XButtonEvent", "button"), 3);

        encode(&Event::Button { press: false, window: win(), at, button: 3 }, &mut b);
        assert_eq!(get(&b, "XEvent", "type"), BUTTON_RELEASE as u64);

        encode(&Event::Expose { window: win(), x: 1, y: 2, width: 3, height: 4, count: 5 }, &mut b);
        assert_eq!(get(&b, "XEvent", "type"), EXPOSE as u64);
        for (f, v) in [("x", 1), ("y", 2), ("width", 3), ("height", 4), ("count", 5)] {
            assert_eq!(get(&b, "XExposeEvent", f), v, "XExposeEvent {}", f);
        }

        encode(&Event::Configure { window: win(), x: 6, y: 7, width: 8, height: 9 }, &mut b);
        assert_eq!(get(&b, "XEvent", "type"), CONFIGURE_NOTIFY as u64);
        for (f, v) in [("x", 6), ("y", 7), ("width", 8), ("height", 9)] {
            assert_eq!(get(&b, "XConfigureEvent", f), v, "XConfigureEvent {}", f);
        }

        // a window at a negative position, which is what a partly offscreen
        // morph configures to -- the field is four bytes and signed
        encode(&Event::Configure { window: win(), x: -5, y: -6, width: 8, height: 9 }, &mut b);
        assert_eq!(get(&b, "XConfigureEvent", "x") as u32 as i32, -5);
        assert_eq!(get(&b, "XConfigureEvent", "y") as u32 as i32, -6);

        // one byte, and only one: is_hint must not spill into what follows
        encode(&Event::Motion { window: win(), at }, &mut b);
        assert_eq!(get(&b, "XMotionEvent", "is_hint"), 0);
        assert_eq!(get(&b, "XMotionEvent", "state"), BUTTON1_MASK as u64);

        // a stale buffer must not show through: encode a fat event, then a
        // thin one over it, and the fields it does not set must read zero
        encode(&Event::Button { press: true, window: win(), at, button: 3 }, &mut b);
        encode(&Event::Focus { entering: true, window: win() }, &mut b);
        assert_eq!(get(&b, "XEvent", "type"), FOCUS_IN as u64);
        assert_eq!(get(&b, "XButtonEvent", "button"), 0, "an old event showed through");
    }

    /// `XSelectInput` is what stops a world that never asked for motion from
    /// being drowned in it, and `XPending` has to agree with what it will get.
    #[test]
    fn only_selected_events_are_queued() {
        let at = Pointer { x: 1, y: 2, x_root: 3, y_root: 4, state: 0 };
        let mut q = Events::new();
        q.select(BUTTON_PRESS_MASK | EXPOSURE_MASK);
        assert!(q.push(Event::Button { press: true, window: win(), at, button: 1 }));
        assert!(!q.push(Event::Motion { window: win(), at }), "motion was not asked for");
        assert!(!q.push(Event::Button { press: false, window: win(), at, button: 1 }));
        // a ClientMessage arrives whatever the mask says, as X delivers it
        assert!(q.push(Event::ClientMessage { window: win(), message_type: 1, format: 32 }));
        assert_eq!(q.pending(), 2);
    }

    /// `XCheckMaskEvent` reaches past events it does not want, and must leave
    /// the ones it skipped in the order they arrived.
    #[test]
    fn a_checked_event_is_taken_out_of_the_middle() {
        let at = Pointer { x: 1, y: 2, x_root: 3, y_root: 4, state: 0 };
        let mut q = Events::new();
        q.select(!0);
        q.push(Event::Motion { window: win(), at });
        q.push(Event::Expose { window: 7, x: 0, y: 0, width: 1, height: 1, count: 0 });
        q.push(Event::Motion { window: 9, at });

        let mut b = [0u8; EVENT_BYTES];
        assert!(q.check_mask(EXPOSURE_MASK, &mut b));
        assert_eq!(get(&b, "XExposeEvent", "window"), 7);
        assert_eq!(q.pending(), 2);

        assert!(q.next_into(&mut b));
        assert_eq!(get(&b, "XEvent", "type"), MOTION_NOTIFY as u64);
        assert_eq!(get(&b, "XMotionEvent", "window"), win(), "the queue lost its order");

        assert!(!q.check_typed(EXPOSE, &mut b), "there was no second Expose to find");
        assert!(q.check_typed(MOTION_NOTIFY, &mut b));
        assert_eq!(q.pending(), 0);
        assert!(!q.next_into(&mut b), "an empty queue answered an event");

        // XPutBackEvent hands the world's own buffer back to the head, and
        // XPeekEvent has to see it there without taking it
        q.put_back(&b);
        assert_eq!(q.pending(), 1);
        let mut peeked = [0u8; EVENT_BYTES];
        assert!(q.peek_into(&mut peeked));
        assert_eq!(peeked, b, "peek did not answer the event that was put back");
        assert_eq!(q.pending(), 1, "peek consumed the event");
        assert!(q.next_into(&mut peeked) && q.pending() == 0);
    }

    /// Morphic tells a click from a double click from a press-and-hold by when
    /// the events arrived. A frozen clock makes every gesture identical: a
    /// single click does nothing, and two in a row come out as something else.
    #[test]
    fn pointer_events_carry_the_time_the_world_reads() {
        let at = Pointer { x: 1, y: 2, x_root: 1, y_root: 2, state: 0 };
        let mut b = [0u8; EVENT_BYTES];
        let time_in =
            |b: &[u8]| u64::from_le_bytes(b[TIME_OFFSET..TIME_OFFSET + 8].try_into().unwrap());

        encode_at(&Event::Button { press: true, window: win(), at, button: 1 }, &mut b, 1234);
        assert_eq!(time_in(&b), 1234, "a button press lost its timestamp");
        encode_at(&Event::Motion { window: win(), at }, &mut b, 5678);
        assert_eq!(time_in(&b), 5678);
        encode_at(&Event::Key { press: true, window: win(), at, keysym: 65 }, &mut b, 9);
        assert_eq!(time_in(&b), 9);
        encode_at(&Event::Crossing { enter: true, window: win(), x: 1, y: 2 }, &mut b, 11);
        assert_eq!(time_in(&b), 11);

        // an Expose has other fields at that offset, so it must not be stamped
        encode_at(
            &Event::Expose { window: win(), x: 0, y: 0, width: 4, height: 5, count: 7 },
            &mut b,
            9999,
        );
        assert_eq!(get(&b, "XExposeEvent", "count"), 7, "the stamp landed on a real field");

        // and the clock actually moves
        let mut c = [0u8; EVENT_BYTES];
        encode(&Event::Button { press: true, window: win(), at, button: 1 }, &mut c);
        let first = time_in(&c);
        std::thread::sleep(std::time::Duration::from_millis(12));
        encode(&Event::Button { press: false, window: win(), at, button: 1 }, &mut c);
        assert!(time_in(&c) > first, "the clock did not advance between two events");
    }

    /// A key that types a character and one that does not, which is the split
    /// every text editor in the world cares about -- and `XLookupString` has to
    /// make it from the encoded event alone, because that is all it is given.
    #[test]
    fn lookup_string_works_from_the_encoded_event() {
        let at = Pointer { x: 0, y: 0, x_root: 0, y_root: 0, state: SHIFT_MASK };
        let mut b = [0u8; EVENT_BYTES];
        let mut round = |keysym| {
            encode(&Event::Key { press: true, window: win(), at, keysym }, &mut b);
            lookup_string(&b)
        };
        assert_eq!(round('A' as u32), (b"A".to_vec(), 'A' as u32));
        assert_eq!(round('a' as u32), (b"a".to_vec(), 'a' as u32));
        assert_eq!(round(0xe9), (vec![0xe9], 0xe9), "Latin-1 above 0x7f is still text");
        assert_eq!(round(XK_RETURN), (vec![], XK_RETURN));
        assert_eq!(round(XK_DELETE), (vec![], XK_DELETE));
        // a control character has a keysym but no printable byte, as in X
        assert_eq!(round(0x03), (vec![], 0x03));

        // and nothing but a key answers anything
        encode(&Event::Motion { window: win(), at }, &mut b);
        assert_eq!(lookup_string(&b), (vec![], 0));
    }

    /// The world tells keys apart by keycode, so Return must not land on the
    /// same one as Ctrl-M -- which is what folding both into the low byte does,
    /// and it buries every other function key under a control character too.
    #[test]
    fn function_keys_and_control_characters_keep_different_keycodes() {
        let specials = [
            XK_BACKSPACE,
            XK_TAB,
            XK_RETURN,
            XK_ESCAPE,
            XK_LEFT,
            XK_UP,
            XK_RIGHT,
            XK_DOWN,
            XK_DELETE,
        ];
        let mut seen = std::collections::HashMap::new();
        // every special key against every Latin-1 character, which includes the
        // control character sharing its low byte
        for k in specials.into_iter().chain(0..0x100) {
            let c = Event::keycode(k);
            if let Some(prev) = seen.insert(c, k) {
                panic!("keysyms {:#06x} and {:#06x} share keycode {}", prev, k, c);
            }
            assert_eq!(Event::keysym(c), k, "keycode {} did not map back", c);
        }
    }
}
