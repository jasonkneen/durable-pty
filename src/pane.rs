//! What makes the browser's side panel a working terminal rather than a picture of one: sorting what arrives from the
//! keyboard into bytes for the program, mouse events and the leave key; re-addressing mouse events to the program's own
//! coordinates in the form it asked for; and keeping the real terminal in the input modes the program set.

use vt100::{MouseProtocolEncoding as Encoding, MouseProtocolMode as Mode};

/// A mouse event as terminals report it (SGR form): `code` carries the button, modifiers, motion (32) and wheel (64)
/// bits; `x`/`y` count from 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mouse {
    pub code: u16,
    pub x: u16,
    pub y: u16,
    pub release: bool,
}

impl Mouse {
    /// -1 for a wheel step up, 1 for a step down.
    pub fn wheel(&self) -> Option<i32> {
        (self.code & 64 != 0).then_some(if self.code & 1 == 0 { -1 } else { 1 })
    }

    pub fn motion(&self) -> bool {
        self.code & 32 != 0 && self.code & 64 == 0
    }

    pub fn left_press(&self) -> bool {
        !self.release && self.code & (64 | 32) == 0 && self.code & 3 == 0
    }

    pub fn within(&self, x: u16, y: u16, width: u16, height: u16) -> bool {
        self.x >= x && self.x < x + width && self.y >= y && self.y < y + height
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Input {
    /// Typed or pasted bytes, to pass on untouched.
    Bytes(Vec<u8>),
    Mouse(Mouse),
    /// Shift-PageUp (-1) or Shift-PageDown (1): scroll back through history.
    Page(i32),
    /// The terminal's answer to "how big is a character cell in pixels": width, height.
    CellSize(u16, u16),
    /// The terminal's answer to a graphics query: whether it draws images.
    Graphics(bool),
    /// Ctrl-] or Ctrl-\: give the keyboard back to the session list. Anything after it in the same read is dropped.
    Leave,
}

/// Sorts raw terminal input. Only an unfinished mouse report is held back for the next read; a lone Esc is never
/// delayed.
#[derive(Default)]
pub struct Splitter {
    pending: Vec<u8>,
}

const MOUSE: &[u8] = b"\x1b[<";
/// Answers from the terminal itself, which arrive mixed in with what is typed.
const GRAPHICS: &[u8] = b"\x1b_G";
const CELL_SIZE: &[u8] = b"\x1b[6;";
/// The keys that leave: Ctrl-] and Ctrl-\, each as one byte and as terminals in an extended keyboard mode send it.
/// Ctrl-] is the one that always works: when the browser itself runs inside a dpty session, the outer `dpty attach`
/// takes Ctrl-\ for itself and detaches the whole window.
const LEAVE: [&[u8]; 6] = [
    b"\x1d",
    b"\x1b[93;5u",
    b"\x1b[27;5;93~",
    b"\x1c",
    b"\x1b[92;5u",
    b"\x1b[27;5;92~",
];
const PAGE_UP: &[u8] = b"\x1b[5;2~";
const PAGE_DOWN: &[u8] = b"\x1b[6;2~";

impl Splitter {
    pub fn feed(&mut self, data: &[u8]) -> Vec<Input> {
        let mut buf = std::mem::take(&mut self.pending);
        buf.extend_from_slice(data);
        let mut out = vec![];
        let mut plain: Vec<u8> = vec![];
        let mut i = 0;
        while i < buf.len() {
            let rest = &buf[i..];
            let special = if LEAVE.iter().any(|f| rest.starts_with(f)) {
                Some((Input::Leave, rest.len()))
            } else if rest.starts_with(PAGE_UP) {
                Some((Input::Page(-1), PAGE_UP.len()))
            } else if rest.starts_with(PAGE_DOWN) {
                Some((Input::Page(1), PAGE_DOWN.len()))
            } else if rest.starts_with(GRAPHICS) {
                match rest.windows(2).position(|w| w == b"\x1b\\") {
                    Some(end) => Some((Input::Graphics(rest[..end].ends_with(b";OK")), end + 2)),
                    None if rest.len() < 256 => {
                        self.pending = rest.to_vec();
                        break;
                    }
                    None => None,
                }
            } else if rest.starts_with(CELL_SIZE) {
                match parse_cell_size(&rest[CELL_SIZE.len()..]) {
                    Parsed::Done(size, used) => {
                        Some((Input::CellSize(size.x, size.y), CELL_SIZE.len() + used))
                    }
                    Parsed::Incomplete => {
                        self.pending = rest.to_vec();
                        break;
                    }
                    Parsed::Invalid => None,
                }
            } else if rest.starts_with(MOUSE) {
                match parse_mouse(&rest[MOUSE.len()..]) {
                    Parsed::Done(m, used) => Some((Input::Mouse(m), MOUSE.len() + used)),
                    Parsed::Incomplete => {
                        self.pending = rest.to_vec();
                        break;
                    }
                    Parsed::Invalid => None,
                }
            } else {
                None
            };
            match special {
                Some((input, used)) => {
                    if !plain.is_empty() {
                        out.push(Input::Bytes(std::mem::take(&mut plain)));
                    }
                    let leave = input == Input::Leave;
                    out.push(input);
                    if leave {
                        return out;
                    }
                    i += used;
                }
                None => {
                    plain.push(buf[i]);
                    i += 1;
                }
            }
        }
        if !plain.is_empty() {
            out.push(Input::Bytes(plain));
        }
        out
    }
}

enum Parsed {
    Done(Mouse, usize),
    Incomplete,
    Invalid,
}

/// Parses `height;width` followed by `t`, the rest of a cell-size report. Returned as a `Mouse` for its x/y pair.
fn parse_cell_size(d: &[u8]) -> Parsed {
    let mut nums = [0u32; 2];
    let mut n = 0;
    for (i, &b) in d.iter().enumerate() {
        match b {
            b'0'..=b'9' => nums[n] = nums[n].saturating_mul(10).saturating_add((b - b'0') as u32),
            b';' if n == 0 => n = 1,
            b't' if n == 1 && nums[0] > 0 && nums[1] > 0 => {
                let px = |v: u32| v.min(u16::MAX as u32) as u16;
                return Parsed::Done(
                    Mouse {
                        code: 0,
                        x: px(nums[1]),
                        y: px(nums[0]),
                        release: false,
                    },
                    i + 1,
                );
            }
            _ => return Parsed::Invalid,
        }
        if i > 12 {
            return Parsed::Invalid;
        }
    }
    Parsed::Incomplete
}

/// Parses `code;x;y` followed by `M` (press, motion) or `m` (release).
fn parse_mouse(d: &[u8]) -> Parsed {
    let mut nums = [0u32; 3];
    let mut n = 0;
    let mut digits = 0;
    for (i, &b) in d.iter().enumerate() {
        match b {
            b'0'..=b'9' => {
                nums[n] = nums[n].saturating_mul(10).saturating_add((b - b'0') as u32);
                digits += 1;
            }
            b';' if n < 2 && digits > 0 => {
                n += 1;
                digits = 0;
            }
            b'M' | b'm' if n == 2 && digits > 0 => {
                let cell = |v: u32| v.clamp(1, u16::MAX as u32) as u16 - 1;
                return Parsed::Done(
                    Mouse {
                        code: nums[0].min(u16::MAX as u32) as u16,
                        x: cell(nums[1]),
                        y: cell(nums[2]),
                        release: b == b'm',
                    },
                    i + 1,
                );
            }
            _ => return Parsed::Invalid,
        }
        if i > 24 {
            return Parsed::Invalid;
        }
    }
    Parsed::Incomplete
}

/// Whether a program that asked for `mode` wants to hear about this event.
pub fn wants(mode: Mode, m: &Mouse) -> bool {
    let held = m.code & 3 != 3;
    match mode {
        Mode::None => false,
        Mode::Press => !m.release && !m.motion(),
        Mode::PressRelease => !m.motion(),
        Mode::ButtonMotion => !m.motion() || held,
        Mode::AnyMotion => true,
    }
}

/// The event as the program expects to read it. `m` is already in the program's own coordinates. The older forms
/// cannot address cells past column or row 223; such events are dropped.
pub fn encode(m: &Mouse, encoding: Encoding) -> Option<Vec<u8>> {
    match encoding {
        Encoding::Sgr => Some(
            format!(
                "\x1b[<{};{};{}{}",
                m.code,
                m.x + 1,
                m.y + 1,
                if m.release { 'm' } else { 'M' }
            )
            .into_bytes(),
        ),
        Encoding::Default | Encoding::Utf8 => {
            // These forms have no release-of-which-button: every release is reported as button 3.
            let code = if m.release { (m.code & !3) | 3 } else { m.code };
            let vals = [32 + code as u32, 33 + m.x as u32, 33 + m.y as u32];
            let mut out = b"\x1b[M".to_vec();
            for v in vals {
                if encoding == Encoding::Utf8 {
                    let mut tmp = [0u8; 4];
                    out.extend_from_slice(char::from_u32(v)?.encode_utf8(&mut tmp).as_bytes());
                } else {
                    out.push(u8::try_from(v).ok()?);
                }
            }
            Some(out)
        }
    }
}

/// The input modes a program can ask a terminal for that change what the keyboard sends. While the pane has the
/// keyboard, the real terminal is kept in the same modes as the program's screen, so arrow keys, keypad keys and pasted
/// text reach the program in the form it expects.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Modes {
    pub paste: bool,
    pub cursor_keys: bool,
    pub keypad: bool,
}

impl Modes {
    pub fn of(screen: &vt100::Screen) -> Self {
        Modes {
            paste: screen.bracketed_paste(),
            cursor_keys: screen.application_cursor(),
            keypad: screen.application_keypad(),
        }
    }

    /// What to write to the real terminal to move it from these modes to `to`.
    pub fn switch(self, to: Modes) -> Vec<u8> {
        let mut out = vec![];
        if self.paste != to.paste {
            out.extend_from_slice(if to.paste {
                b"\x1b[?2004h"
            } else {
                b"\x1b[?2004l"
            });
        }
        if self.cursor_keys != to.cursor_keys {
            out.extend_from_slice(if to.cursor_keys {
                b"\x1b[?1h"
            } else {
                b"\x1b[?1l"
            });
        }
        if self.keypad != to.keypad {
            out.extend_from_slice(if to.keypad { b"\x1b=" } else { b"\x1b>" });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_bytes_pass_through_untouched() {
        let mut s = Splitter::default();
        assert_eq!(
            s.feed(b"ls -la\r"),
            vec![Input::Bytes(b"ls -la\r".to_vec())]
        );
        assert_eq!(
            s.feed(b"\x1b"),
            vec![Input::Bytes(b"\x1b".to_vec())],
            "a lone Esc is not held back"
        );
        assert_eq!(
            s.feed(b"\x1b[A\x1b[200~pasted\x1b[201~"),
            vec![Input::Bytes(b"\x1b[A\x1b[200~pasted\x1b[201~".to_vec())]
        );
    }

    #[test]
    fn mouse_reports_are_separated_from_typing() {
        let mut s = Splitter::default();
        assert_eq!(
            s.feed(b"a\x1b[<0;10;5Mb\x1b[<0;10;5m"),
            vec![
                Input::Bytes(b"a".to_vec()),
                Input::Mouse(Mouse {
                    code: 0,
                    x: 9,
                    y: 4,
                    release: false
                }),
                Input::Bytes(b"b".to_vec()),
                Input::Mouse(Mouse {
                    code: 0,
                    x: 9,
                    y: 4,
                    release: true
                }),
            ]
        );
    }

    #[test]
    fn a_mouse_report_split_across_reads_is_joined() {
        let mut s = Splitter::default();
        assert_eq!(s.feed(b"x\x1b[<64;3"), vec![Input::Bytes(b"x".to_vec())]);
        assert_eq!(
            s.feed(b";7M"),
            vec![Input::Mouse(Mouse {
                code: 64,
                x: 2,
                y: 6,
                release: false
            })]
        );
    }

    #[test]
    fn the_leave_key_ends_input_in_every_form() {
        for form in LEAVE {
            let mut s = Splitter::default();
            let mut d = b"ab".to_vec();
            d.extend_from_slice(form);
            d.extend_from_slice(b"dropped");
            assert_eq!(s.feed(&d), vec![Input::Bytes(b"ab".to_vec()), Input::Leave]);
        }
    }

    #[test]
    fn shift_page_keys_scroll_instead_of_reaching_the_program() {
        let mut s = Splitter::default();
        assert_eq!(
            s.feed(b"\x1b[5;2~\x1b[6;2~\x1b[5~"),
            vec![
                Input::Page(-1),
                Input::Page(1),
                Input::Bytes(b"\x1b[5~".to_vec())
            ]
        );
    }

    #[test]
    fn answers_from_the_terminal_are_not_typed_into_the_program() {
        let mut s = Splitter::default();
        assert_eq!(
            s.feed(b"a\x1b[6;36;17tb"),
            vec![
                Input::Bytes(b"a".to_vec()),
                Input::CellSize(17, 36),
                Input::Bytes(b"b".to_vec())
            ]
        );
        assert_eq!(s.feed(b"\x1b_Gi=31;OK\x1b\\"), vec![Input::Graphics(true)]);
        assert_eq!(s.feed(b"\x1b_Gi=31;ENOTSUP"), vec![]);
        assert_eq!(
            s.feed(b"PORTED\x1b\\x"),
            vec![Input::Graphics(false), Input::Bytes(b"x".to_vec())]
        );
        assert_eq!(
            s.feed(b"\x1b[6;5~\x1b[6~"),
            vec![Input::Bytes(b"\x1b[6;5~\x1b[6~".to_vec())],
            "Ctrl-PageDown and PageDown are keys"
        );
        assert_eq!(s.feed(b"\x1b[6;2~"), vec![Input::Page(1)]);
    }

    #[test]
    fn a_program_only_hears_the_mouse_events_it_asked_for() {
        let press = Mouse {
            code: 0,
            x: 1,
            y: 1,
            release: false,
        };
        let release = Mouse {
            release: true,
            ..press
        };
        let drag = Mouse { code: 32, ..press };
        let hover = Mouse { code: 35, ..press };
        let wheel = Mouse { code: 65, ..press };
        assert!(!wants(Mode::None, &press));
        assert!(
            wants(Mode::Press, &press)
                && !wants(Mode::Press, &release)
                && wants(Mode::Press, &wheel)
        );
        assert!(wants(Mode::PressRelease, &release) && !wants(Mode::PressRelease, &drag));
        assert!(wants(Mode::ButtonMotion, &drag) && !wants(Mode::ButtonMotion, &hover));
        assert!(wants(Mode::AnyMotion, &hover));
        assert_eq!(wheel.wheel(), Some(1));
        assert_eq!(Mouse { code: 64, ..press }.wheel(), Some(-1));
    }

    #[test]
    fn mouse_events_are_written_in_the_form_the_program_asked_for() {
        let m = Mouse {
            code: 0,
            x: 4,
            y: 2,
            release: false,
        };
        assert_eq!(encode(&m, Encoding::Sgr).unwrap(), b"\x1b[<0;5;3M");
        assert_eq!(
            encode(&Mouse { release: true, ..m }, Encoding::Sgr).unwrap(),
            b"\x1b[<0;5;3m"
        );
        assert_eq!(
            encode(&m, Encoding::Default).unwrap(),
            [0x1b, b'[', b'M', 32, 37, 35]
        );
        assert_eq!(
            encode(&Mouse { release: true, ..m }, Encoding::Default).unwrap(),
            [0x1b, b'[', b'M', 35, 37, 35]
        );
        assert_eq!(
            encode(&Mouse { x: 300, ..m }, Encoding::Default),
            None,
            "too far right for the old form"
        );
        assert!(encode(&Mouse { x: 300, ..m }, Encoding::Utf8).is_some());
    }

    #[test]
    fn the_real_terminal_follows_the_programs_input_modes() {
        let mut p = vt100::Parser::new(5, 20, 0);
        assert_eq!(Modes::of(p.screen()), Modes::default());
        p.process(b"\x1b[?2004h\x1b[?1h\x1b=");
        let on = Modes::of(p.screen());
        assert_eq!(
            on,
            Modes {
                paste: true,
                cursor_keys: true,
                keypad: true
            }
        );
        assert_eq!(Modes::default().switch(on), b"\x1b[?2004h\x1b[?1h\x1b=");
        assert_eq!(on.switch(Modes::default()), b"\x1b[?2004l\x1b[?1l\x1b>");
        assert!(on.switch(on).is_empty());
    }
}
