//! Native window (minifb: Windows / macOS / Linux): shows the VGA picture scaled to
//! the window and turns host input into PC input (scancodes, mouse driver state).

#[cfg(feature = "native")]
use minifb::{Key, KeyRepeat, MouseButton, MouseMode, ScaleMode, Window, WindowOptions};

#[derive(Clone, Debug)]
pub enum Event {
    Close,
    /// Mouse in driver coordinates (x 0-639, y = screen row) and button bits.
    Mouse { x: i32, y: i32, buttons: u16 },
    /// Set-1 scancode; 0x1xx = E0-prefixed (arrows, Home/End …).
    Key { sc: u16, down: bool },
}

#[cfg(feature = "native")]
pub struct Host {
    win: Window,
    last_mouse: (i32, i32, u16),
    events: Vec<Event>,
    frame: (usize, usize),
    buf: Vec<u32>,
}

#[cfg(feature = "native")]
fn scancode(k: Key) -> Option<u16> {
    use Key::*;
    Some(match k {
        Escape => 0x01, Key1 => 0x02, Key2 => 0x03, Key3 => 0x04, Key4 => 0x05, Key5 => 0x06,
        Key6 => 0x07, Key7 => 0x08, Key8 => 0x09, Key9 => 0x0a, Key0 => 0x0b, Minus => 0x0c,
        Equal => 0x0d, Backspace => 0x0e, Tab => 0x0f,
        Q => 0x10, W => 0x11, E => 0x12, R => 0x13, T => 0x14, Y => 0x15, U => 0x16, I => 0x17,
        O => 0x18, P => 0x19, LeftBracket => 0x1a, RightBracket => 0x1b, Enter => 0x1c,
        LeftCtrl => 0x1d, A => 0x1e, S => 0x1f, D => 0x20, F => 0x21, G => 0x22, H => 0x23,
        J => 0x24, K => 0x25, L => 0x26, Semicolon => 0x27, Apostrophe => 0x28, Backquote => 0x29,
        LeftShift => 0x2a, Backslash => 0x2b, Z => 0x2c, X => 0x2d, C => 0x2e, V => 0x2f,
        B => 0x30, N => 0x31, M => 0x32, Comma => 0x33, Period => 0x34, Slash => 0x35,
        RightShift => 0x36, LeftAlt => 0x38, Space => 0x39,
        F1 => 0x3b, F2 => 0x3c, F3 => 0x3d, F4 => 0x3e, F5 => 0x3f, F6 => 0x40, F7 => 0x41,
        F8 => 0x42, F9 => 0x43, F10 => 0x44, F11 => 0x57, F12 => 0x58,
        NumPad7 => 0x47, NumPad8 => 0x48, NumPad9 => 0x49, NumPad4 => 0x4b, NumPad5 => 0x4c,
        NumPad6 => 0x4d, NumPad1 => 0x4f, NumPad2 => 0x50, NumPad3 => 0x51, NumPad0 => 0x52,
        NumPadEnter => 0x11c, RightCtrl => 0x11d, RightAlt => 0x138,
        Home => 0x147, Up => 0x148, PageUp => 0x149, Left => 0x14b, Right => 0x14d,
        End => 0x14f, Down => 0x150, PageDown => 0x151, Insert => 0x152, Delete => 0x153,
        _ => return None,
    })
}

/// Window mouse position → mouse driver coordinates (x 0-639 whatever the mode,
/// y = screen row), through the aspect-fit rectangle the frame is drawn in.
/// None while the window has no area (minimized).
pub fn mouse_to_driver(win: (usize, usize), mouse: (f32, f32), buf: (usize, usize), sy: usize) -> Option<(i32, i32)> {
    let ((ww, wh), (bw, bh)) = (win, buf);
    if ww == 0 || wh == 0 || bw == 0 || bh == 0 {
        return None;
    }
    let s = (ww as f32 / bw as f32).min(wh as f32 / bh as f32);
    let (ox, oy) = ((ww as f32 - bw as f32 * s) / 2.0, (wh as f32 - bh as f32 * s) / 2.0);
    let bx = ((mouse.0 - ox) / s).clamp(0.0, bw as f32 - 1.0) as i32;
    let by = ((mouse.1 - oy) / s).clamp(0.0, bh as f32 - 1.0) as i32;
    Some((bx * 640 / bw as i32, by / sy as i32))
}

/// Built without the native window (the web build): no window can open.
#[cfg(not(feature = "native"))]
pub struct Host;

#[cfg(not(feature = "native"))]
impl Host {
    pub fn open(_w: usize, _h: usize, _place: Option<(i32, i32, usize, usize)>) -> Host {
        panic!("eiketsu-rt was built without its native window (feature \"native\"); run headless")
    }
    pub fn present(&mut self, _px: &[u32], _w: usize, _h: usize) -> bool {
        false
    }
    pub fn events(&mut self) -> Vec<Event> {
        Vec::new()
    }
    pub fn set_title(&mut self, _title: &str) {}
}

#[cfg(feature = "native")]
impl Host {
    /// `place` = (x, y, width, height) for a borderless window at a fixed spot.
    pub fn open(w: usize, h: usize, place: Option<(i32, i32, usize, usize)>) -> Host {
        let mut opts = WindowOptions { resize: true, topmost: true, scale_mode: ScaleMode::AspectRatioStretch, ..WindowOptions::default() };
        let (ww, wh) = match place {
            Some((_, _, pw, ph)) => {
                opts.borderless = true;
                opts.title = false;
                opts.resize = false;
                (pw, ph)
            }
            None => (w * 2, h * 2),
        };
        let mut win = Window::new("三國志英傑傳", ww, wh, opts).expect("cannot open window");
        if let Some((x, y, _, _)) = place {
            win.set_position(x as isize, y as isize);
        } else {
            win.set_position(60, 40);
        }
        win.set_target_fps(0);
        Host { win, last_mouse: (-1, -1, 0), events: Vec::new(), frame: (w, h), buf: Vec::new() }
    }

    /// Show a frame; returns false once the window was closed.
    pub fn present(&mut self, px: &[u32], w: usize, h: usize) -> bool {
        if !self.win.is_open() {
            return false;
        }
        // 320-wide modes are doubled so the window keeps a 4:3-ish shape.
        let (sx, sy) = (if w <= 320 { 2 } else { 1 }, if h <= 240 { 2 } else { 1 });
        let (bw, bh) = (w * sx, h * sy);
        self.buf.resize(bw * bh, 0);
        for y in 0..bh {
            for x in 0..bw {
                self.buf[y * bw + x] = px[(y / sy) * w + x / sx];
            }
        }
        self.frame = (w, h);
        if self.win.update_with_buffer(&self.buf, bw, bh).is_err() {
            return false;
        }
        self.poll(bw, bh, sy);
        self.win.is_open()
    }

    fn poll(&mut self, bw: usize, bh: usize, sy: usize) {
        for k in self.win.get_keys_pressed(KeyRepeat::Yes) {
            if let Some(sc) = scancode(k) {
                self.events.push(Event::Key { sc, down: true });
            }
        }
        for k in self.win.get_keys_released() {
            if let Some(sc) = scancode(k) {
                self.events.push(Event::Key { sc, down: false });
            }
        }
        // Map window coordinates through the aspect-fit rectangle to the buffer.
        // (Pass, not Clamp: minifb's Clamp panics on a minimized, 0x0 window.)
        let pos = self.win.get_unscaled_mouse_pos(MouseMode::Pass);
        if let Some((x, y)) = pos.and_then(|m| mouse_to_driver(self.win.get_size(), m, (bw, bh), sy)) {
            let b = self.win.get_mouse_down(MouseButton::Left) as u16
                | (self.win.get_mouse_down(MouseButton::Right) as u16) << 1
                | (self.win.get_mouse_down(MouseButton::Middle) as u16) << 2;
            if (x, y, b) != self.last_mouse {
                self.last_mouse = (x, y, b);
                self.events.push(Event::Mouse { x, y, buttons: b });
            }
        }
    }

    pub fn events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    pub fn set_title(&mut self, title: &str) {
        self.win.set_title(title);
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimized_window_gives_no_mouse() {
        // Minimized: the window reports 0x0 (this used to panic in a clamp).
        assert_eq!(mouse_to_driver((0, 0), (12.0, 34.0), (640, 400), 1), None);
        assert_eq!(mouse_to_driver((0, 0), (f32::NAN, f32::NAN), (640, 400), 1), None);
    }

    #[test]
    fn mouse_maps_through_the_letterbox() {
        // 1280x960 window, 640x480 frame: scale 2, no bars.
        assert_eq!(mouse_to_driver((1280, 960), (640.0, 480.0), (640, 480), 1), Some((320, 240)));
        // Outside the window (Pass mode reports it) is clamped to the frame edge.
        assert_eq!(mouse_to_driver((1280, 960), (-50.0, 5000.0), (640, 480), 1), Some((0, 479)));
        // 640x400 frame (320x200 doubled, sy 2) in a 1280x960 window: bars of 80 above and below.
        assert_eq!(mouse_to_driver((1280, 960), (0.0, 80.0), (640, 400), 2), Some((0, 0)));
    }
}
