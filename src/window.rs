//! A real window for the native canvas: winit for the platform, softbuffer for
//! the pixels, and a translation from what those report into the X events the
//! world already knows how to read.
//!
//! The VM keeps its own loop. Self's scheduler, its processes and the world's
//! own console all run inside `interp`, and the world asks for input by calling
//! `XPending` when it feels like it -- so winit is *pumped*
//! (`EventLoopExtPumpEvents`) rather than handed the thread with `run_app`.
//! What comes back goes into `events::Events`, and the world drains it exactly
//! as it drains a real X connection.
//!
//! ponytail: one window. X's drawable id for it is a constant, because the
//! world opens exactly one and every event names it. Give `Drawables` a map
//! from window id to surface when a world opens two.

use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::platform::pump_events::{EventLoopExtPumpEvents, PumpStatus};
use winit::window::WindowId;

use crate::canvas::Canvas;
use crate::events::{self, Event, Events, Pointer};

/// The one window's X id. Every event the world reads names it.
pub const WINDOW: u64 = 1;

/// `WM_DELETE_WINDOW`, which is the only client message this sends. The world
/// interns the real atom; what matters is that it is the one it asked for.
pub const WM_DELETE_WINDOW: u64 = 1;

type Surface = softbuffer::Surface<Arc<winit::window::Window>, Arc<winit::window::Window>>;

/// winit's name for a key, as X's. A key that produced a character is that
/// character -- X keysyms for printable keys are their code points, shift
/// already applied, which is exactly what winit's *logical* key reports.
///
/// ponytail: the named keys are the ones an editor steers by. A key with no
/// character and no entry here is dropped rather than guessed at, because a
/// wrong keysym is worse for the world than a missing one.
fn keysym_of(key: &Key) -> Option<u32> {
    Some(match key {
        Key::Character(s) => s.chars().next()? as u32,
        Key::Named(NamedKey::Space) => ' ' as u32,
        Key::Named(NamedKey::Enter) => events::XK_RETURN,
        Key::Named(NamedKey::Tab) => events::XK_TAB,
        Key::Named(NamedKey::Escape) => events::XK_ESCAPE,
        Key::Named(NamedKey::Backspace) => events::XK_BACKSPACE,
        Key::Named(NamedKey::Delete) => events::XK_DELETE,
        Key::Named(NamedKey::ArrowLeft) => events::XK_LEFT,
        Key::Named(NamedKey::ArrowUp) => events::XK_UP,
        Key::Named(NamedKey::ArrowRight) => events::XK_RIGHT,
        Key::Named(NamedKey::ArrowDown) => events::XK_DOWN,
        _ => return None,
    })
}

/// X numbers its buttons 1 left, 2 *middle*, 3 right -- not in the order a
/// modern toolkit lists them. Getting those two the wrong way round sends every
/// menu click somewhere else, and it looks like it works until you try it.
fn x_button(b: MouseButton) -> Option<u32> {
    Some(match b {
        MouseButton::Left => 1,
        MouseButton::Middle => 2,
        MouseButton::Right => 3,
        _ => return None,
    })
}

/// The bit a held button contributes to an event's `state`. Buttons 1..5 sit in
/// bits 8..12, so a wheel "button" has one too.
pub fn button_bit(button: u32) -> u32 {
    if (1..=5).contains(&button) {
        1 << (7 + button)
    } else {
        0
    }
}

fn modifier_bits(m: ModifiersState) -> u32 {
    let mut s = 0;
    if m.shift_key() {
        s |= events::SHIFT_MASK;
    }
    if m.control_key() {
        s |= events::CONTROL_MASK;
    }
    // X has no Command; the world's meta key is Mod1, which is where a Mac's
    // Option already sits and where Command is most useful to it
    if m.alt_key() || m.super_key() {
        s |= events::MOD1_MASK;
    }
    s
}

struct App {
    title: String,
    want: (u32, u32),
    win: Option<Arc<winit::window::Window>>,
    surface: Option<Surface>,
    canvas: Canvas,
    q: Events,
    at: Pointer,
    /// bits for the buttons currently held, which ride along in every `state`
    held: u32,
    mods: u32,
    /// set once the window is gone, so the VM can stop
    closed: bool,
}

impl App {
    /// How many real pixels one of the world's should be. The window knows,
    /// once there is one; `SERF_SCALE` overrides it, which is the only way a
    /// headless run or a test has of saying.
    ///
    /// ponytail: rounded to a whole number. A fractional scale would stop the
    /// logical grid being a grid, and the displays that raised this question
    /// are all exactly 2.
    fn display_scale(&self) -> i32 {
        if let Some(v) = std::env::var_os("SERF_SCALE") {
            return v.to_string_lossy().trim().parse().unwrap_or(1);
        }
        self.win.as_ref().map_or(1, |w| w.scale_factor().round() as i32).max(1)
    }

    /// Hand an event to the queue, and say so when asked. Only the world can
    /// click, so `SERF_TRACE_INPUT=1` is how a mis-aimed one gets diagnosed.
    fn emit(&mut self, e: Event) {
        if std::env::var_os("SERF_TRACE_INPUT").is_some() {
            let taken = e.mask() & self.q_selected() != 0;
            eprintln!("input {:?} taken={}", e, taken);
        }
        self.q.push(e);
    }

    fn q_selected(&self) -> u64 {
        self.q.selected()
    }

    /// Move the pointer, and say whether X would have reported it.
    ///
    /// X reports motion when the pointer *moves*. winit reports a cursor
    /// position on other occasions too -- on either side of a click, and when a
    /// modifier changes -- and a motion that did not move is not nothing to the
    /// world: motion with a button held is a *drag*. Passing those on turns
    /// every plain click into press, drag, release, and the world handles it as
    /// a drag: the morph under the pointer gets picked up rather than clicked.
    fn point_at(&mut self, x: i32, y: i32) -> bool {
        let moved = (x, y) != (self.at.x, self.at.y);
        self.at.x = x;
        self.at.y = y;
        // no window manager to ask, so the root is the window
        self.at.x_root = x;
        self.at.y_root = y;
        moved
    }

    fn pointer(&self) -> Pointer {
        Pointer { state: self.held | self.mods, ..self.at }
    }

    /// A press and a release together, which is how X reports a wheel.
    fn wheel(&mut self, up: bool) {
        let button = if up { 4 } else { 5 };
        let at = self.pointer();
        self.q.push(Event::Button { press: true, window: WINDOW, at, button });
        self.q.push(Event::Button { press: false, window: WINDOW, at, button });
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.win.is_some() {
            return;
        }
        let attrs = winit::window::Window::default_attributes()
            .with_title(self.title.clone())
            .with_inner_size(winit::dpi::LogicalSize::new(self.want.0, self.want.1));
        let win = match el.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                eprintln!("serf: no window: {}", e);
                self.closed = true;
                return;
            }
        };
        match softbuffer::Context::new(win.clone())
            .and_then(|c| softbuffer::Surface::new(&c, win.clone()))
        {
            Ok(s) => self.surface = Some(s),
            Err(e) => {
                eprintln!("serf: no surface: {}", e);
                self.closed = true;
            }
        }
        self.win = Some(win);
    }

    fn window_event(&mut self, _el: &ActiveEventLoop, _id: WindowId, ev: WindowEvent) {
        match ev {
            WindowEvent::CloseRequested => {
                // what a close box is: the world decides whether to go
                self.emit(Event::ClientMessage {
                    window: WINDOW,
                    message_type: WM_DELETE_WINDOW,
                    format: 32,
                });
                self.closed = true;
            }
            WindowEvent::Resized(size) => {
                // winit reports real pixels. The world is an X client and thinks
                // in its own, so the canvas keeps its coordinates logical and
                // carries the scale -- which is where a retina display stops
                // rendering everything at half size.
                let k = self.display_scale();
                let (w, h) = ((size.width as i32 / k).max(1), (size.height as i32 / k).max(1));
                // X leaves a resized window's contents undefined and follows
                // with an Expose, so start clean rather than stretch anything
                self.canvas = Canvas::scaled(w, h, self.canvas.bg, k);
                self.emit(Event::Configure { window: WINDOW, x: 0, y: 0, width: w, height: h });
                self.emit(Event::Expose {
                    window: WINDOW,
                    x: 0,
                    y: 0,
                    width: w,
                    height: h,
                    count: 0,
                });
            }
            WindowEvent::RedrawRequested => {
                let (w, h) = (self.canvas.w, self.canvas.h);
                self.emit(Event::Expose {
                    window: WINDOW,
                    x: 0,
                    y: 0,
                    width: w,
                    height: h,
                    count: 0,
                });
            }
            WindowEvent::Focused(entering) => {
                self.emit(Event::Focus { entering, window: WINDOW });
            }
            WindowEvent::CursorEntered { .. } => {
                let (x, y) = (self.at.x, self.at.y);
                self.emit(Event::Crossing { enter: true, window: WINDOW, x, y });
            }
            WindowEvent::CursorLeft { .. } => {
                let (x, y) = (self.at.x, self.at.y);
                self.emit(Event::Crossing { enter: false, window: WINDOW, x, y });
            }
            WindowEvent::ModifiersChanged(m) => self.mods = modifier_bits(m.state()),
            WindowEvent::CursorMoved { position, .. } => {
                // a pointer position is in real pixels too
                let k = self.canvas.scale.max(1);
                if self.point_at(position.x as i32 / k, position.y as i32 / k) {
                    let at = self.pointer();
                    self.emit(Event::Motion { window: WINDOW, at });
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let Some(button) = x_button(button) else { return };
                let press = state == ElementState::Pressed;
                // X reports the buttons held *before* the event, so the one
                // being pressed is not in its own state and the one being
                // released still is
                let at = self.pointer();
                if press {
                    self.held |= button_bit(button);
                } else {
                    self.held &= !button_bit(button);
                }
                self.emit(Event::Button { press, window: WINDOW, at, button });
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    MouseScrollDelta::PixelDelta(p) => p.y as f32,
                };
                if dy != 0.0 {
                    self.wheel(dy > 0.0);
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let Some(keysym) = keysym_of(&event.logical_key) else { return };
                let press = event.state == ElementState::Pressed;
                let at = self.pointer();
                self.emit(Event::Key { press, window: WINDOW, at, keysym });
            }
            _ => {}
        }
    }
}

/// A window, its pixels, and the events it has produced.
pub struct Window {
    el: EventLoop<()>,
    app: App,
}

impl Window {
    /// Opens nothing yet: winit creates the window on the first pump, when it
    /// has an event loop to hang it off.
    pub fn open(title: &str, w: i32, h: i32, bg: u32) -> Result<Window, String> {
        let el = EventLoop::new().map_err(|e| format!("no event loop: {}", e))?;
        let mut q = Events::new();
        // what a world that has not called XSelectInput yet should still see;
        // the VM replaces this the moment it does
        q.select(!0);
        Ok(Window {
            el,
            app: App {
                title: title.into(),
                want: (w.max(1) as u32, h.max(1) as u32),
                win: None,
                surface: None,
                canvas: Canvas::new(w, h, bg),
                q,
                at: Pointer { x: 0, y: 0, x_root: 0, y_root: 0, state: 0 },
                held: 0,
                mods: 0,
                closed: false,
            },
        })
    }

    /// Let the platform deliver what it has. `timeout` is how long to wait for
    /// something to arrive; zero returns at once, which is what a VM with work
    /// of its own wants.
    ///
    /// ponytail: on macOS, pumping stops `NSApplication` between calls, so a
    /// live resize can show tearing where `run_app` would not. It is the price
    /// of the VM keeping its own loop, and it is the right price -- Self's
    /// scheduler cannot be a callback.
    pub fn pump(&mut self, timeout: Duration) -> bool {
        if let PumpStatus::Exit(_) = self.el.pump_app_events(Some(timeout), &mut self.app) {
            self.app.closed = true;
        }
        !self.app.closed
    }

    pub fn events(&mut self) -> &mut Events {
        &mut self.app.q
    }

    pub fn canvas(&mut self) -> &mut Canvas {
        &mut self.app.canvas
    }

    pub fn size(&self) -> (i32, i32) {
        (self.app.canvas.w, self.app.canvas.h)
    }

    /// Real pixels per logical one, once the window has told us.
    pub fn scale(&self) -> i32 {
        self.app.canvas.scale
    }

    /// Where the platform says the window actually is. Worth having: a window
    /// that was created but never put on the screen looks exactly like a
    /// working one from in here, and this is what tells them apart.
    pub fn placement(&self) -> Option<String> {
        let w = self.app.win.as_ref()?;
        let s = w.inner_size();
        let at = w.outer_position().map_or("?".into(), |p| format!("{},{}", p.x, p.y));
        Some(format!(
            "{}x{} at {}, scale {}, visible {:?}",
            s.width,
            s.height,
            at,
            w.scale_factor(),
            w.is_visible()
        ))
    }

    /// `XFlush`: put the canvas on the screen.
    pub fn present(&mut self) {
        let (Some(surface), Some(win)) = (&mut self.app.surface, &self.app.win) else { return };
        let c = &self.app.canvas;
        let (Some(w), Some(h)) = (NonZeroU32::new(c.pw() as u32), NonZeroU32::new(c.ph() as u32))
        else {
            return;
        };
        if let Err(e) = surface.resize(w, h) {
            eprintln!("serf: surface resize: {}", e);
            return;
        }
        match surface.buffer_mut() {
            Ok(mut b) => {
                // softbuffer's pixels are 0RGB, which is the canvas's own
                // format, so this is a copy and not a conversion
                let n = b.len().min(c.px.len());
                b[..n].copy_from_slice(&c.px[..n]);
                if let Err(e) = b.present() {
                    eprintln!("serf: present: {}", e);
                }
                win.pre_present_notify();
            }
            Err(e) => eprintln!("serf: buffer: {}", e),
        }
    }
}

/// Open a window with the drawing sheet in it and report what comes back, so
/// the whole native path -- pixels, fonts and input -- can be seen at once.
/// Leaves on its own after `secs`, on Escape, or when the window is closed.
pub fn demo(secs: u64) {
    let sheet = crate::text::draw_sheet();
    let mut w = match Window::open("serf", sheet.w, sheet.h, 0xF2F2F2) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("serf: {}", e);
            return;
        }
    };
    println!(
        "serf: window open, {}x{}; Escape or close to leave, {}s otherwise",
        sheet.w, sheet.h, secs
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    let mut buf = [0u8; events::EVENT_BYTES];
    let (mut n, mut frames, mut told) = (0usize, 0usize, false);
    while w.pump(Duration::from_millis(16)) {
        if !told {
            if let Some(p) = w.placement() {
                println!("serf: the platform says {}", p);
                told = true;
            }
        }
        if std::time::Instant::now() >= deadline {
            println!("serf: {}s up", secs);
            break;
        }
        while w.events().next_into(&mut buf) {
            let t = events::get(&buf, "XEvent", "type") as i64;
            n += 1;
            match t {
                events::EXPOSE => {
                    // the world's answer to an Expose: draw, then flush
                    let (cw, ch) = w.size();
                    let c = w.canvas();
                    for y in 0..ch.min(sheet.h) {
                        for x in 0..cw.min(sheet.w) {
                            c.put(x, y, sheet.get(x, y));
                        }
                    }
                    w.present();
                    frames += 1;
                    println!(
                        "Expose {}x{} -> drew and flushed",
                        events::get(&buf, "XExposeEvent", "width"),
                        events::get(&buf, "XExposeEvent", "height")
                    );
                }
                events::BUTTON_PRESS | events::BUTTON_RELEASE => println!(
                    "Button{} {} at {},{} state={:#x}",
                    events::get(&buf, "XButtonEvent", "button"),
                    if t == events::BUTTON_PRESS { "press  " } else { "release" },
                    events::get(&buf, "XButtonEvent", "x"),
                    events::get(&buf, "XButtonEvent", "y"),
                    events::get(&buf, "XButtonEvent", "state")
                ),
                events::KEY_PRESS | events::KEY_RELEASE => {
                    let (text, keysym) = events::lookup_string(&buf);
                    println!(
                        "Key{} keysym={:#06x} text={:?} state={:#x}",
                        if t == events::KEY_PRESS { "press  " } else { "release" },
                        keysym,
                        String::from_utf8_lossy(&text),
                        events::get(&buf, "XKeyEvent", "state")
                    );
                    if keysym == events::XK_ESCAPE && t == events::KEY_PRESS {
                        println!("serf: escape");
                        return;
                    }
                }
                events::CONFIGURE_NOTIFY => println!(
                    "ConfigureNotify {}x{}",
                    events::get(&buf, "XConfigureEvent", "width"),
                    events::get(&buf, "XConfigureEvent", "height")
                ),
                events::CLIENT_MESSAGE => {
                    println!("ClientMessage: the close box");
                    return;
                }
                // motion and crossing are the noisy ones; count them only
                _ => {}
            }
        }
    }
    println!("serf: {} events, {} frames presented", n, frames);
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::keyboard::SmolStr;

    fn app() -> App {
        App {
            title: "test".into(),
            want: (100, 100),
            win: None,
            surface: None,
            canvas: Canvas::new(100, 100, 0),
            q: Events::new(),
            at: Pointer { x: 0, y: 0, x_root: 0, y_root: 0, state: 0 },
            held: 0,
            mods: 0,
            closed: false,
        }
    }

    /// A click is press and release with nothing in between. winit reports a
    /// cursor position on either side of one, at the very place the pointer
    /// already is, and passing those on as `MotionNotify` makes every click a
    /// drag -- which is how a button press ends up picking a morph up instead.
    #[test]
    fn a_pointer_that_did_not_move_reports_no_motion() {
        let mut a = app();
        assert!(a.point_at(40, 30), "the first position is a move");
        assert!(!a.point_at(40, 30), "the same position again is not");
        assert!(!a.point_at(40, 30));
        assert!(a.point_at(41, 30), "one pixel is a move");
        assert!(a.point_at(41, 31));
        assert!(!a.point_at(41, 31));

        // and the position is still tracked, so a button event that follows
        // one of the suppressed reports still carries the right place
        assert_eq!((a.at.x, a.at.y), (41, 31));
        assert_eq!((a.at.x_root, a.at.y_root), (41, 31));
    }

    /// X numbers its buttons 1, 2, 3 as left, *middle*, right. A toolkit that
    /// lists them left, right, middle will swap two of them, and then every
    /// menu click in the world lands on the wrong button -- which looks like it
    /// works right up until someone tries it.
    #[test]
    fn buttons_are_numbered_the_way_x_numbers_them() {
        assert_eq!(x_button(MouseButton::Left), Some(1));
        assert_eq!(x_button(MouseButton::Middle), Some(2));
        assert_eq!(x_button(MouseButton::Right), Some(3));
        assert_eq!(x_button(MouseButton::Other(9)), None);

        // and each held button owns one bit of `state`, wheel buttons included
        for (b, bit) in [(1, 0x100), (2, 0x200), (3, 0x400), (4, 0x800), (5, 0x1000)] {
            assert_eq!(button_bit(b), bit, "button {}", b);
        }
        assert_eq!(button_bit(1), events::BUTTON1_MASK);
        assert_eq!(button_bit(9), 0, "a button X does not name contributes nothing");
    }

    /// A printable key's keysym is its own character, and the rest have to land
    /// on the keysym the world knows them by -- through `XLookupString`, which
    /// is the only way the world ever asks.
    #[test]
    fn keys_translate_to_the_keysyms_the_world_reads() {
        let ch = |s: &str| Key::Character(SmolStr::new(s));
        assert_eq!(keysym_of(&ch("a")), Some('a' as u32));
        // shift is already applied by the time winit reports a logical key,
        // which is exactly X's rule for a keysym
        assert_eq!(keysym_of(&ch("A")), Some('A' as u32));
        assert_eq!(keysym_of(&Key::Named(NamedKey::Space)), Some(' ' as u32));
        assert_eq!(keysym_of(&Key::Named(NamedKey::Enter)), Some(events::XK_RETURN));
        assert_eq!(keysym_of(&Key::Named(NamedKey::Backspace)), Some(events::XK_BACKSPACE));
        assert_eq!(keysym_of(&Key::Named(NamedKey::ArrowUp)), Some(events::XK_UP));
        // a key with no character and no name of its own is dropped, not guessed
        assert_eq!(keysym_of(&Key::Named(NamedKey::F13)), None);

        // and the round trip the world actually performs
        let at = Pointer { x: 0, y: 0, x_root: 0, y_root: 0, state: 0 };
        let mut b = [0u8; events::EVENT_BYTES];
        for (key, text, sym) in [
            (ch("A"), "A", 'A' as u32),
            (Key::Named(NamedKey::Enter), "\r", events::XK_RETURN),
            (Key::Named(NamedKey::Backspace), "\u{8}", events::XK_BACKSPACE),
            (Key::Named(NamedKey::ArrowLeft), "", events::XK_LEFT),
        ] {
            let keysym = keysym_of(&key).unwrap();
            events::encode(&Event::Key { press: true, window: WINDOW, at, keysym }, &mut b);
            let (got, gs) = events::lookup_string(&b);
            assert_eq!((String::from_utf8_lossy(&got).as_ref(), gs), (text, sym));
        }
    }

    /// The modifier bits the world tests for. Command has no X equivalent, so
    /// it joins Option on Mod1 rather than going missing.
    #[test]
    fn modifiers_land_on_the_bits_x_defines() {
        assert_eq!(modifier_bits(ModifiersState::empty()), 0);
        assert_eq!(modifier_bits(ModifiersState::SHIFT), events::SHIFT_MASK);
        assert_eq!(modifier_bits(ModifiersState::CONTROL), events::CONTROL_MASK);
        assert_eq!(modifier_bits(ModifiersState::ALT), events::MOD1_MASK);
        assert_eq!(modifier_bits(ModifiersState::SUPER), events::MOD1_MASK);
        assert_eq!(
            modifier_bits(ModifiersState::SHIFT | ModifiersState::CONTROL),
            events::SHIFT_MASK | events::CONTROL_MASK
        );
    }
}
