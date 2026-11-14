//! Links and images for the browser's own terminal views.
//!
//! The browser draws a session from a grid of characters, and the grid (the `vt100` crate) keeps neither hyperlinks nor
//! images. This sits between the session's output and that grid and carries both through it as ordinary text, so they
//! scroll, clip and clear with the text around them:
//!
//! - A hyperlink (OSC 8) becomes two invisible marker characters after every character it covers, naming an entry in
//!   a table of addresses. When the grid is drawn, the markers are taken off again and the cells are written to the
//!   real terminal as a link.
//! - An image (kitty graphics protocol) is passed to the real terminal under an id of our own and given a "virtual
//!   placement"; the cells it should cover are filled with the protocol's placeholder character, whose colour names
//!   the image and whose combining marks name the row and column. The terminal draws the image wherever those cells
//!   end up.

use base64::Engine;
use std::collections::HashMap;
use unicode_width::UnicodeWidthChar;

/// The combining marks that number a placeholder cell's row and column, in order (entry 0 means row or column 0).
/// From kitty's `rowcolumn-diacritics.txt`, the list every terminal implementing the protocol uses.
pub const DIACRITICS: [char; 297] = [
    '\u{0305}',
    '\u{030d}',
    '\u{030e}',
    '\u{0310}',
    '\u{0312}',
    '\u{033d}',
    '\u{033e}',
    '\u{033f}',
    '\u{0346}',
    '\u{034a}',
    '\u{034b}',
    '\u{034c}',
    '\u{0350}',
    '\u{0351}',
    '\u{0352}',
    '\u{0357}',
    '\u{035b}',
    '\u{0363}',
    '\u{0364}',
    '\u{0365}',
    '\u{0366}',
    '\u{0367}',
    '\u{0368}',
    '\u{0369}',
    '\u{036a}',
    '\u{036b}',
    '\u{036c}',
    '\u{036d}',
    '\u{036e}',
    '\u{036f}',
    '\u{0483}',
    '\u{0484}',
    '\u{0485}',
    '\u{0486}',
    '\u{0487}',
    '\u{0592}',
    '\u{0593}',
    '\u{0594}',
    '\u{0595}',
    '\u{0597}',
    '\u{0598}',
    '\u{0599}',
    '\u{059c}',
    '\u{059d}',
    '\u{059e}',
    '\u{059f}',
    '\u{05a0}',
    '\u{05a1}',
    '\u{05a8}',
    '\u{05a9}',
    '\u{05ab}',
    '\u{05ac}',
    '\u{05af}',
    '\u{05c4}',
    '\u{0610}',
    '\u{0611}',
    '\u{0612}',
    '\u{0613}',
    '\u{0614}',
    '\u{0615}',
    '\u{0616}',
    '\u{0617}',
    '\u{0657}',
    '\u{0658}',
    '\u{0659}',
    '\u{065a}',
    '\u{065b}',
    '\u{065d}',
    '\u{065e}',
    '\u{06d6}',
    '\u{06d7}',
    '\u{06d8}',
    '\u{06d9}',
    '\u{06da}',
    '\u{06db}',
    '\u{06dc}',
    '\u{06df}',
    '\u{06e0}',
    '\u{06e1}',
    '\u{06e2}',
    '\u{06e4}',
    '\u{06e7}',
    '\u{06e8}',
    '\u{06eb}',
    '\u{06ec}',
    '\u{0730}',
    '\u{0732}',
    '\u{0733}',
    '\u{0735}',
    '\u{0736}',
    '\u{073a}',
    '\u{073d}',
    '\u{073f}',
    '\u{0740}',
    '\u{0741}',
    '\u{0743}',
    '\u{0745}',
    '\u{0747}',
    '\u{0749}',
    '\u{074a}',
    '\u{07eb}',
    '\u{07ec}',
    '\u{07ed}',
    '\u{07ee}',
    '\u{07ef}',
    '\u{07f0}',
    '\u{07f1}',
    '\u{07f3}',
    '\u{0816}',
    '\u{0817}',
    '\u{0818}',
    '\u{0819}',
    '\u{081b}',
    '\u{081c}',
    '\u{081d}',
    '\u{081e}',
    '\u{081f}',
    '\u{0820}',
    '\u{0821}',
    '\u{0822}',
    '\u{0823}',
    '\u{0825}',
    '\u{0826}',
    '\u{0827}',
    '\u{0829}',
    '\u{082a}',
    '\u{082b}',
    '\u{082c}',
    '\u{082d}',
    '\u{0951}',
    '\u{0953}',
    '\u{0954}',
    '\u{0f82}',
    '\u{0f83}',
    '\u{0f86}',
    '\u{0f87}',
    '\u{135d}',
    '\u{135e}',
    '\u{135f}',
    '\u{17dd}',
    '\u{193a}',
    '\u{1a17}',
    '\u{1a75}',
    '\u{1a76}',
    '\u{1a77}',
    '\u{1a78}',
    '\u{1a79}',
    '\u{1a7a}',
    '\u{1a7b}',
    '\u{1a7c}',
    '\u{1b6b}',
    '\u{1b6d}',
    '\u{1b6e}',
    '\u{1b6f}',
    '\u{1b70}',
    '\u{1b71}',
    '\u{1b72}',
    '\u{1b73}',
    '\u{1cd0}',
    '\u{1cd1}',
    '\u{1cd2}',
    '\u{1cda}',
    '\u{1cdb}',
    '\u{1ce0}',
    '\u{1dc0}',
    '\u{1dc1}',
    '\u{1dc3}',
    '\u{1dc4}',
    '\u{1dc5}',
    '\u{1dc6}',
    '\u{1dc7}',
    '\u{1dc8}',
    '\u{1dc9}',
    '\u{1dcb}',
    '\u{1dcc}',
    '\u{1dd1}',
    '\u{1dd2}',
    '\u{1dd3}',
    '\u{1dd4}',
    '\u{1dd5}',
    '\u{1dd6}',
    '\u{1dd7}',
    '\u{1dd8}',
    '\u{1dd9}',
    '\u{1dda}',
    '\u{1ddb}',
    '\u{1ddc}',
    '\u{1ddd}',
    '\u{1dde}',
    '\u{1ddf}',
    '\u{1de0}',
    '\u{1de1}',
    '\u{1de2}',
    '\u{1de3}',
    '\u{1de4}',
    '\u{1de5}',
    '\u{1de6}',
    '\u{1dfe}',
    '\u{20d0}',
    '\u{20d1}',
    '\u{20d4}',
    '\u{20d5}',
    '\u{20d6}',
    '\u{20d7}',
    '\u{20db}',
    '\u{20dc}',
    '\u{20e1}',
    '\u{20e7}',
    '\u{20e9}',
    '\u{20f0}',
    '\u{2cef}',
    '\u{2cf0}',
    '\u{2cf1}',
    '\u{2de0}',
    '\u{2de1}',
    '\u{2de2}',
    '\u{2de3}',
    '\u{2de4}',
    '\u{2de5}',
    '\u{2de6}',
    '\u{2de7}',
    '\u{2de8}',
    '\u{2de9}',
    '\u{2dea}',
    '\u{2deb}',
    '\u{2dec}',
    '\u{2ded}',
    '\u{2dee}',
    '\u{2def}',
    '\u{2df0}',
    '\u{2df1}',
    '\u{2df2}',
    '\u{2df3}',
    '\u{2df4}',
    '\u{2df5}',
    '\u{2df6}',
    '\u{2df7}',
    '\u{2df8}',
    '\u{2df9}',
    '\u{2dfa}',
    '\u{2dfb}',
    '\u{2dfc}',
    '\u{2dfd}',
    '\u{2dfe}',
    '\u{2dff}',
    '\u{a66f}',
    '\u{a67c}',
    '\u{a67d}',
    '\u{a6f0}',
    '\u{a6f1}',
    '\u{a8e0}',
    '\u{a8e1}',
    '\u{a8e2}',
    '\u{a8e3}',
    '\u{a8e4}',
    '\u{a8e5}',
    '\u{a8e6}',
    '\u{a8e7}',
    '\u{a8e8}',
    '\u{a8e9}',
    '\u{a8ea}',
    '\u{a8eb}',
    '\u{a8ec}',
    '\u{a8ed}',
    '\u{a8ee}',
    '\u{a8ef}',
    '\u{a8f0}',
    '\u{a8f1}',
    '\u{aab0}',
    '\u{aab2}',
    '\u{aab3}',
    '\u{aab7}',
    '\u{aab8}',
    '\u{aabe}',
    '\u{aabf}',
    '\u{aac1}',
    '\u{fe20}',
    '\u{fe21}',
    '\u{fe22}',
    '\u{fe23}',
    '\u{fe24}',
    '\u{fe25}',
    '\u{fe26}',
    '\u{10a0f}',
    '\u{10a38}',
    '\u{1d185}',
    '\u{1d186}',
    '\u{1d187}',
    '\u{1d188}',
    '\u{1d189}',
    '\u{1d1aa}',
    '\u{1d1ab}',
    '\u{1d1ac}',
    '\u{1d1ad}',
    '\u{1d242}',
    '\u{1d243}',
    '\u{1d244}',
];

const PLACEHOLDER: char = '\u{10EEEE}';
/// Link markers are two characters from Variation Selectors Supplement: invisible, zero columns wide, and never
/// produced by programs next to ordinary text.
const MARK_BASE: u32 = 0xE0100;
const MARK_RADIX: u32 = 240;
const MAX_LINKS: u32 = MARK_RADIX * MARK_RADIX;
const MAX_SEQUENCE: usize = 8 * 1024 * 1024;

fn is_mark(c: char) -> bool {
    (MARK_BASE..MARK_BASE + MARK_RADIX).contains(&(c as u32))
}

/// Text without link markers: what the program actually printed.
pub fn without_marks(text: &str) -> String {
    text.chars().filter(|c| !is_mark(*c)).collect()
}

/// A cell's text without link markers, and the link it belongs to if it has them.
pub fn link_of(contents: &str) -> (std::borrow::Cow<'_, str>, Option<u32>) {
    // Every marker starts with this byte in UTF-8; almost no cell has one.
    if !contents.as_bytes().contains(&0xF3) || !contents.chars().any(is_mark) {
        return (contents.into(), None);
    }
    let marks: Vec<u32> = contents
        .chars()
        .filter(|c| is_mark(*c))
        .map(|c| c as u32 - MARK_BASE)
        .collect();
    let text: String = contents.chars().filter(|c| !is_mark(*c)).collect();
    let link = (marks.len() >= 2).then(|| marks[0] * MARK_RADIX + marks[1]);
    (text.into(), link)
}

#[derive(Default)]
struct Links {
    uris: Vec<String>,
    index: HashMap<String, u32>,
    next: u32,
    current: Option<u32>,
}

impl Links {
    fn intern(&mut self, uri: &str) -> u32 {
        if let Some(&i) = self.index.get(uri) {
            return i;
        }
        let i = self.next % MAX_LINKS;
        self.next = self.next.wrapping_add(1);
        if (i as usize) < self.uris.len() {
            let old = std::mem::replace(&mut self.uris[i as usize], uri.to_string());
            self.index.remove(&old);
        } else {
            self.uris.push(uri.to_string());
        }
        self.index.insert(uri.to_string(), i);
        i
    }
}

#[derive(Default, Clone, Copy, PartialEq)]
enum State {
    #[default]
    Ground,
    Esc,
    Csi,
    Osc,
    OscEsc,
    Apc,
    ApcEsc,
    /// A DCS, SOS or PM string: passed through untouched.
    Str,
    StrEsc,
}

/// What handling a piece of output asks of the outside world.
#[derive(Default, Debug)]
pub struct Effects {
    /// To write to the real terminal: image data and placements.
    pub terminal: Vec<u8>,
    /// To send to the session as input: answers to the program's image queries.
    pub session: Vec<u8>,
}

/// A chunked image transfer in progress.
struct Transfer {
    id: u32,
    place: Option<Placement>,
    reply: Option<String>,
}

#[derive(Clone, Copy, Default)]
struct Placement {
    cols: u32,
    rows: u32,
    /// Source rectangle in pixels, when the program asked for part of the image.
    rect: [u32; 4],
    /// The program asked for the cursor to stay where it was.
    keep_cursor: bool,
}

#[derive(Default)]
struct Images {
    next: u32,
    /// The program's name for an image (its id, or its image number) to ours.
    by_app: HashMap<(bool, u32), u32>,
    /// Pixel size of each image, where it could be learned.
    size: HashMap<u32, (u32, u32)>,
    transfer: Option<Transfer>,
}

pub struct Rich {
    state: State,
    seq: Vec<u8>,
    utf8: Vec<u8>,
    /// Output waiting to be given to the grid.
    out: Vec<u8>,
    links: Links,
    images: Images,
    /// The real terminal draws images (it answered our query). Until that is known, image commands are dropped.
    pub graphics: bool,
    /// Size of one character cell in pixels, for working out how many cells an image covers.
    pub cell: (u16, u16),
    /// Replaying old output: images and links are restored, but queries in it are not answered a second time.
    pub replaying: bool,
}

impl Default for Rich {
    fn default() -> Self {
        Rich {
            state: State::Ground,
            seq: vec![],
            utf8: vec![],
            out: vec![],
            links: Links::default(),
            images: Images::default(),
            graphics: false,
            cell: (0, 0),
            replaying: false,
        }
    }
}

/// The keys of a graphics command: `a=T,f=100,i=7`.
struct Keys<'a>(Vec<(u8, &'a str)>);

impl<'a> Keys<'a> {
    fn parse(control: &'a str) -> Self {
        Keys(
            control
                .split(',')
                .filter_map(|kv| kv.split_once('='))
                .filter(|(k, _)| k.len() == 1)
                .map(|(k, v)| (k.as_bytes()[0], v))
                .collect(),
        )
    }
    fn get(&self, key: u8) -> Option<&'a str> {
        self.0.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
    }
    fn num(&self, key: u8) -> u32 {
        self.get(key).and_then(|v| v.parse().ok()).unwrap_or(0)
    }
    fn letter(&self, key: u8, default: u8) -> u8 {
        self.get(key)
            .and_then(|v| v.bytes().next())
            .unwrap_or(default)
    }
    /// The given keys as they were written, for passing on.
    fn pass(&self, keys: &[u8]) -> String {
        self.0
            .iter()
            .filter(|(k, _)| keys.contains(k))
            .map(|(k, v)| format!(",{}={v}", *k as char))
            .collect()
    }
}

fn png_size(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() < 24 || &data[..8] != b"\x89PNG\r\n\x1a\n" || &data[12..16] != b"IHDR" {
        return None;
    }
    let be = |at: usize| u32::from_be_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]);
    Some((be(16), be(20)))
}

fn graphics_command(control: &str, payload: &[u8]) -> Vec<u8> {
    let mut v = format!("\x1b_G{control}").into_bytes();
    if !payload.is_empty() {
        v.push(b';');
        v.extend_from_slice(payload);
    }
    v.extend_from_slice(b"\x1b\\");
    v
}

impl Rich {
    /// The address a link index stands for.
    pub fn uri(&self, link: u32) -> Option<&str> {
        self.links.uris.get(link as usize).map(String::as_str)
    }

    /// Gives a piece of the session's output to the grid, with links and images turned into text the grid can hold.
    pub fn feed(&mut self, parser: &mut vt100::Parser, data: &[u8]) -> Effects {
        let mut fx = Effects::default();
        for &b in data {
            match self.state {
                State::Ground => {
                    if b == 0x1b {
                        self.flush_utf8();
                        self.state = State::Esc;
                    } else {
                        self.text(b);
                    }
                }
                State::Esc => match b {
                    b'[' => self.begin(State::Csi),
                    b']' => self.begin(State::Osc),
                    b'_' => self.begin(State::Apc),
                    b'P' | b'X' | b'^' => {
                        self.out.extend_from_slice(&[0x1b, b]);
                        self.state = State::Str;
                    }
                    0x1b => self.out.push(0x1b),
                    _ => {
                        self.out.extend_from_slice(&[0x1b, b]);
                        self.state = State::Ground;
                    }
                },
                State::Csi => {
                    self.seq.push(b);
                    if (0x40..=0x7e).contains(&b) || self.seq.len() > 256 {
                        self.out.extend_from_slice(b"\x1b[");
                        self.out.append(&mut self.seq);
                        self.state = State::Ground;
                    }
                }
                State::Osc => match b {
                    0x07 => self.end_osc(),
                    0x1b => self.state = State::OscEsc,
                    _ => self.collect(b),
                },
                State::OscEsc => {
                    if b == b'\\' {
                        self.end_osc();
                    } else {
                        self.collect(0x1b);
                        self.collect(b);
                        self.state = State::Osc;
                    }
                }
                State::Apc => match b {
                    0x1b => self.state = State::ApcEsc,
                    _ => self.collect(b),
                },
                State::ApcEsc => {
                    if b == b'\\' {
                        let seq = std::mem::take(&mut self.seq);
                        self.state = State::Ground;
                        if seq.first() == Some(&b'G') {
                            // The command acts where the cursor is, so the grid has to be up to date first.
                            parser.process(&std::mem::take(&mut self.out));
                            self.graphics_in(parser, &seq[1..], &mut fx);
                        }
                    } else {
                        self.collect(0x1b);
                        self.collect(b);
                        self.state = State::Apc;
                    }
                }
                State::Str => {
                    self.out.push(b);
                    if b == 0x1b {
                        self.state = State::StrEsc;
                    }
                }
                State::StrEsc => {
                    self.out.push(b);
                    self.state = if b == b'\\' {
                        State::Ground
                    } else {
                        State::Str
                    };
                }
            }
        }
        parser.process(&std::mem::take(&mut self.out));
        if self.replaying {
            fx.session.clear();
        }
        fx
    }

    fn begin(&mut self, state: State) {
        self.seq.clear();
        self.state = state;
    }

    fn collect(&mut self, b: u8) {
        // A runaway sequence (lost terminator) is cut off rather than kept for ever.
        if self.seq.len() < MAX_SEQUENCE {
            self.seq.push(b);
        }
    }

    /// One byte of plain text. Inside a link, every character that takes up a cell gets the link's markers after it.
    fn text(&mut self, b: u8) {
        let Some(link) = self.links.current else {
            self.out.push(b);
            return;
        };
        self.utf8.push(b);
        match std::str::from_utf8(&self.utf8) {
            Ok(s) => {
                for c in s.chars() {
                    let mut tmp = [0u8; 4];
                    self.out
                        .extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
                    if c.width().unwrap_or(0) > 0 {
                        for mark in [link / MARK_RADIX, link % MARK_RADIX] {
                            let m = char::from_u32(MARK_BASE + mark).unwrap_or(' ');
                            self.out
                                .extend_from_slice(m.encode_utf8(&mut tmp).as_bytes());
                        }
                    }
                }
                self.utf8.clear();
            }
            // Part of a character: wait for the rest.
            Err(e) if e.error_len().is_none() && self.utf8.len() < 4 => {}
            Err(_) => self.flush_utf8(),
        }
    }

    fn flush_utf8(&mut self) {
        self.out.append(&mut self.utf8);
    }

    fn end_osc(&mut self) {
        let seq = std::mem::take(&mut self.seq);
        self.state = State::Ground;
        if let Some(rest) = seq.strip_prefix(b"8;") {
            // OSC 8 ; params ; address. An empty address ends the link.
            let uri = rest.splitn(2, |&b| b == b';').nth(1).unwrap_or(b"");
            let uri: String = String::from_utf8_lossy(uri)
                .chars()
                .filter(|c| !c.is_control())
                .take(2083)
                .collect();
            self.links.current = (!uri.is_empty()).then(|| self.links.intern(&uri));
            return;
        }
        self.out.extend_from_slice(b"\x1b]");
        self.out.extend_from_slice(&seq);
        self.out.push(0x07);
    }

    fn new_image(&mut self) -> u32 {
        // Ids are ours alone on this terminal; the high byte keeps two browsers on one terminal apart.
        let base = (std::process::id() % 250 + 1) << 16;
        self.images.next = self.images.next % 0xFFFF + 1;
        base | self.images.next
    }

    /// The id we use for the image a command names, creating one for a new image.
    fn image_for(&mut self, keys: &Keys<'_>, create: bool) -> Option<u32> {
        let name = match (keys.num(b'i'), keys.num(b'I')) {
            (0, 0) => None,
            (0, n) => Some((true, n)),
            (i, _) => Some((false, i)),
        };
        if let Some(id) = name.and_then(|n| self.images.by_app.get(&n).copied()) {
            return Some(id);
        }
        if !create {
            return None;
        }
        let id = self.new_image();
        if let Some(name) = name {
            self.images.by_app.insert(name, id);
        }
        Some(id)
    }

    fn graphics_in(&mut self, parser: &mut vt100::Parser, seq: &[u8], fx: &mut Effects) {
        if !self.graphics {
            return;
        }
        let (control, payload) = match seq.iter().position(|&b| b == b';') {
            Some(at) => (&seq[..at], &seq[at + 1..]),
            None => (seq, &[][..]),
        };
        let control = String::from_utf8_lossy(control);
        let keys = Keys::parse(&control);
        let more = keys.num(b'm') == 1;

        // The later chunks of a transfer carry only the data.
        if let Some(transfer) = self.images.transfer.take() {
            fx.terminal
                .extend(graphics_command(&format!("m={},q=2", more as u8), payload));
            if more {
                self.images.transfer = Some(transfer);
            } else {
                self.finish(parser, transfer, fx);
            }
            return;
        }

        let quiet = keys.num(b'q');
        let reply = |ours: u32| {
            let named = match (keys.num(b'i'), keys.num(b'I')) {
                (0, 0) => return None,
                (0, n) => format!("i={ours},I={n}"),
                (i, _) => format!("i={i}"),
            };
            (quiet == 0).then(|| format!("\x1b_G{named};OK\x1b\\"))
        };
        match keys.letter(b'a', b't') {
            // "Can you draw this?" Yes: the real terminal said so.
            b'q' => fx
                .session
                .extend(format!("\x1b_Gi={};OK\x1b\\", keys.num(b'i')).into_bytes()),
            action @ (b't' | b'T') => {
                let Some(id) = self.image_for(&keys, true) else {
                    return;
                };
                if let Some(size) = self.image_size(&keys, payload) {
                    self.images.size.insert(id, size);
                }
                fx.terminal.extend(graphics_command(
                    &format!("a=t,i={id},q=2{}", keys.pass(b"ftsvoSOm")),
                    payload,
                ));
                let transfer = Transfer {
                    id,
                    place: (action == b'T').then(|| Self::placement(&keys)),
                    reply: reply(id),
                };
                if keys.num(b'I') != 0 && keys.num(b'i') == 0 {
                    // The program learns our id from the reply and may use it from then on.
                    self.images.by_app.insert((false, id), id);
                }
                if more {
                    self.images.transfer = Some(transfer);
                } else {
                    self.finish(parser, transfer, fx);
                }
            }
            b'p' => {
                if let Some(id) = self.image_for(&keys, false) {
                    self.place(parser, id, Self::placement(&keys), fx);
                    fx.session
                        .extend(reply(id).unwrap_or_default().into_bytes());
                }
            }
            b'd' => {
                let what = keys.letter(b'd', b'a');
                let free = what.is_ascii_uppercase();
                let ids: Vec<u32> = match what.to_ascii_lowercase() {
                    b'i' | b'n' => self.image_for(&keys, false).into_iter().collect(),
                    b'a' => self.all_images(),
                    // Deleting by position: the cells themselves are text here and go when the program clears them.
                    _ => vec![],
                };
                for id in ids {
                    fx.terminal.extend(graphics_command(
                        &format!("a=d,d={},i={id},q=2", if free { 'I' } else { 'i' }),
                        &[],
                    ));
                    if free {
                        self.images.by_app.retain(|_, v| *v != id);
                        self.images.size.remove(&id);
                    }
                }
            }
            // Animation frames and controls go to the same image under our id.
            b'f' | b'a' | b'c' => {
                if let Some(id) = self.image_for(&keys, false) {
                    let rest: String = keys
                        .0
                        .iter()
                        .filter(|(k, _)| !b"iIq".contains(k))
                        .map(|(k, v)| format!(",{}={v}", *k as char))
                        .collect();
                    fx.terminal
                        .extend(graphics_command(&format!("i={id},q=2{rest}"), payload));
                }
            }
            _ => {}
        }
    }

    fn all_images(&self) -> Vec<u32> {
        let mut ids: Vec<u32> = self
            .images
            .by_app
            .values()
            .copied()
            .chain(self.images.size.keys().copied())
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    fn finish(&mut self, parser: &mut vt100::Parser, transfer: Transfer, fx: &mut Effects) {
        if let Some(placement) = transfer.place {
            self.place(parser, transfer.id, placement, fx);
        }
        fx.session
            .extend(transfer.reply.unwrap_or_default().into_bytes());
    }

    fn placement(keys: &Keys<'_>) -> Placement {
        Placement {
            cols: keys.num(b'c'),
            rows: keys.num(b'r'),
            rect: [
                keys.num(b'x'),
                keys.num(b'y'),
                keys.num(b'w'),
                keys.num(b'h'),
            ],
            keep_cursor: keys.num(b'C') == 1,
        }
    }

    /// The image's size in pixels, from the command or the start of its data.
    fn image_size(&self, keys: &Keys<'_>, payload: &[u8]) -> Option<(u32, u32)> {
        let decode = |data: &[u8]| base64::engine::general_purpose::STANDARD.decode(data).ok();
        if keys.num(b'f') != 100 {
            return Some((keys.num(b's'), keys.num(b'v'))).filter(|s| s.0 > 0 && s.1 > 0);
        }
        match keys.letter(b't', b'd') {
            // The data itself: the header is in the first few bytes.
            b'd' if keys.get(b'o').is_none() => {
                png_size(&decode(&payload[..payload.len().min(44) / 4 * 4])?)
            }
            // A file the terminal will read: read its header too.
            b'f' | b't' => {
                use std::io::Read;
                let path = String::from_utf8(decode(payload)?).ok()?;
                let mut head = [0u8; 32];
                std::fs::File::open(path).ok()?.read_exact(&mut head).ok()?;
                png_size(&head)
            }
            _ => None,
        }
    }

    /// Shows an image at the cursor: tells the real terminal how many cells it covers, and fills those cells in the
    /// grid with placeholders naming the image, its row and its column.
    fn place(&mut self, parser: &mut vt100::Parser, id: u32, p: Placement, fx: &mut Effects) {
        let (screen_rows, screen_cols) = parser.screen().size();
        let (row, col) = parser.screen().cursor_position();
        let (cell_w, cell_h) = match self.cell {
            (0, _) | (_, 0) => (9u32, 18u32),
            (w, h) => (w as u32, h as u32),
        };
        let (px_w, px_h) = match (p.rect[2], p.rect[3]) {
            (0, _) | (_, 0) => self.images.size.get(&id).copied().unwrap_or((0, 0)),
            part => part,
        };
        let (mut cols, mut rows) = (p.cols, p.rows);
        if px_w > 0 && px_h > 0 {
            // Whatever the program left out follows from the image's shape.
            match (cols, rows) {
                (0, 0) => {
                    cols = px_w.div_ceil(cell_w);
                    rows = px_h.div_ceil(cell_h);
                }
                (c, 0) => {
                    rows = (px_h as u64 * c as u64 * cell_w as u64)
                        .div_ceil(px_w as u64 * cell_h as u64) as u32
                }
                (0, r) => {
                    cols = (px_w as u64 * r as u64 * cell_h as u64)
                        .div_ceil(px_h as u64 * cell_w as u64) as u32
                }
                _ => {}
            }
        }
        // Size unknown and not given: a modest box rather than nothing.
        let limit = DIACRITICS.len() as u32;
        let cols = if cols == 0 { 40 } else { cols }
            .clamp(1, (screen_cols.saturating_sub(col) as u32).clamp(1, limit));
        let mut rows = if rows == 0 { 12 } else { rows }.clamp(1, limit);
        if p.keep_cursor {
            rows = rows.min(screen_rows.saturating_sub(row).max(1) as u32);
        }

        let rect = if p.rect.iter().any(|v| *v > 0) {
            format!(
                ",x={},y={},w={},h={}",
                p.rect[0], p.rect[1], p.rect[2], p.rect[3]
            )
        } else {
            String::new()
        };
        fx.terminal.extend(graphics_command(
            &format!("a=p,U=1,i={id},p=1,c={cols},r={rows},q=2{rect}"),
            &[],
        ));

        let attrs = parser.screen().attributes_formatted();
        let mut text = format!(
            "\x1b[0m\x1b[38;2;{};{};{}m",
            (id >> 16) & 255,
            (id >> 8) & 255,
            id & 255
        );
        for r in 0..rows {
            if r > 0 {
                // Down a line (scrolling if the program expects that), back to the image's left edge.
                text.push_str(if p.keep_cursor { "\x1b[B" } else { "\n" });
                text.push_str(&format!("\x1b[{}G", col + 1));
            }
            for c in 0..cols {
                text.push(PLACEHOLDER);
                text.push(DIACRITICS[r as usize]);
                text.push(DIACRITICS[c as usize]);
            }
        }
        text.push_str("\x1b[0m");
        let mut bytes = text.into_bytes();
        bytes.extend(attrs);
        if p.keep_cursor {
            bytes.extend(format!("\x1b[{};{}H", row + 1, col + 1).into_bytes());
        }
        parser.process(&bytes);
    }

    /// Frees every image this view put on the real terminal. For when the view closes.
    pub fn cleanup(&mut self) -> Vec<u8> {
        let ids = self.all_images();
        self.images = Images::default();
        ids.into_iter()
            .flat_map(|id| graphics_command(&format!("a=d,d=I,i={id},q=2"), &[]))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).into_owned()
    }

    // A 1x1 PNG's first 33 bytes are enough for its size; this header says 90 x 36 pixels.
    fn png(width: u32, height: u32) -> String {
        let mut data = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec();
        data.extend(width.to_be_bytes());
        data.extend(height.to_be_bytes());
        data.extend([8, 6, 0, 0, 0, 0, 0, 0, 0]);
        base64::engine::general_purpose::STANDARD.encode(data)
    }

    #[test]
    fn a_link_is_carried_on_every_character_it_covers_and_nowhere_else() {
        let mut rich = Rich::default();
        let mut p = vt100::Parser::new(5, 40, 100);
        rich.feed(
            &mut p,
            b"see \x1b]8;;https://example.com/a\x1b\\the docs\x1b]8;;\x1b\\ now",
        );
        let cell = |col| p.screen().cell(0, col).unwrap().contents().to_string();
        assert_eq!(link_of(&cell(0)), ("s".into(), None));
        let fourth = cell(4);
        let (shown, link) = link_of(&fourth);
        assert_eq!(shown, "t");
        assert_eq!(rich.uri(link.unwrap()), Some("https://example.com/a"));
        assert_eq!(
            link_of(&cell(7)).1,
            link,
            "the space inside the link belongs to it"
        );
        assert_eq!(link_of(&cell(11)).1, link);
        assert_eq!(
            link_of(&cell(12)),
            (" ".into(), None),
            "text after the link is plain"
        );
        assert_eq!(
            p.screen()
                .contents()
                .chars()
                .filter(|c| !is_mark(*c))
                .collect::<String>()
                .trim_end(),
            "see the docs now"
        );
    }

    #[test]
    fn links_survive_being_split_across_reads_and_scrolling() {
        let mut rich = Rich::default();
        let mut p = vt100::Parser::new(3, 20, 100);
        for piece in [
            &b"\x1b]8;id=1;https://exa"[..],
            b"mple.com/long\x07caf",
            b"\xc3",
            b"\xa9\x1b]8;;\x07\r\n1\r\n2\r\n3\r\n",
        ] {
            rich.feed(&mut p, piece);
        }
        p.screen_mut().set_scrollback(2);
        let (shown, link) = link_of(p.screen().cell(0, 3).unwrap().contents());
        assert_eq!(
            shown, "\u{e9}",
            "a character split across reads is put back together"
        );
        assert_eq!(rich.uri(link.unwrap()), Some("https://example.com/long"));
    }

    #[test]
    fn the_same_address_reuses_one_table_entry_and_control_characters_are_removed() {
        let mut rich = Rich::default();
        let mut p = vt100::Parser::new(3, 40, 0);
        rich.feed(&mut p, b"\x1b]8;;http://a/\x07x\x1b]8;;\x07 \x1b]8;;http://b/\x07y\x1b]8;;\x07 \x1b]8;;http://a/\x07z\x1b]8;;\x07");
        let link = |p: &vt100::Parser, col| {
            link_of(p.screen().cell(0, col).unwrap().contents())
                .1
                .unwrap()
        };
        assert_eq!(link(&p, 0), link(&p, 4));
        assert_ne!(link(&p, 0), link(&p, 2));
        rich.feed(&mut p, b"\x1b]8;;http://c/\x01evil\x07q\x1b]8;;\x07");
        assert_eq!(rich.uri(link(&p, 5)), Some("http://c/evil"));
    }

    #[test]
    fn other_sequences_reach_the_grid_unchanged() {
        let mut rich = Rich::default();
        let mut p = vt100::Parser::new(3, 20, 0);
        rich.feed(
            &mut p,
            b"\x1b[31mred\x1b[0m \x1b]0;a title\x07\x1bP+q544e\x1b\\ok\x1b[2;3Hx",
        );
        assert_eq!(p.screen().contents(), "red ok\n  x");
        assert_eq!(
            p.screen().cell(0, 0).unwrap().fgcolor(),
            vt100::Color::Idx(1)
        );
    }

    #[test]
    fn an_image_goes_to_the_terminal_and_its_cells_are_marked_in_the_grid() {
        let mut rich = Rich {
            graphics: true,
            cell: (9, 18),
            ..Rich::default()
        };
        let mut p = vt100::Parser::new(10, 40, 0);
        rich.feed(&mut p, b"ab");
        let fx = rich.feed(
            &mut p,
            format!("\x1b_Ga=T,f=100,i=7;{}\x1b\\", png(90, 36)).as_bytes(),
        );
        let sent = text(&fx.terminal);
        let id: u32 = sent
            .split("i=")
            .nth(1)
            .unwrap()
            .split(',')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert!(
            sent.starts_with(&format!("\x1b_Ga=t,i={id},q=2,f=100;")),
            "{sent:?}"
        );
        assert!(
            sent.contains(&format!("\x1b_Ga=p,U=1,i={id},p=1,c=10,r=2,q=2\x1b\\")),
            "90x36 pixels is 10x2 cells: {sent:?}"
        );
        assert_eq!(
            text(&fx.session),
            "\x1b_Gi=7;OK\x1b\\",
            "the program is told it worked, under its own id"
        );
        let cell = p.screen().cell(1, 5).unwrap();
        assert_eq!(
            cell.contents(),
            format!("{PLACEHOLDER}{}{}", DIACRITICS[1], DIACRITICS[3]),
            "row 1, column 3 of the image"
        );
        assert_eq!(
            cell.fgcolor(),
            vt100::Color::Rgb((id >> 16) as u8, (id >> 8) as u8, id as u8)
        );
        assert_eq!(
            p.screen().cursor_position(),
            (1, 12),
            "the cursor ends after the image's last row"
        );
        assert!(!p.screen().cell(1, 12).unwrap().has_contents());
    }

    #[test]
    fn an_image_sent_in_pieces_is_placed_once_it_is_whole() {
        let mut rich = Rich {
            graphics: true,
            cell: (10, 20),
            ..Rich::default()
        };
        let mut p = vt100::Parser::new(10, 40, 0);
        let data = png(20, 20);
        let fx = rich.feed(
            &mut p,
            format!("\x1b_Ga=T,f=100,m=1;{}\x1b\\", &data[..20]).as_bytes(),
        );
        assert!(!text(&fx.terminal).contains("a=p"));
        assert!(!p.screen().cell(0, 0).unwrap().has_contents());
        let fx = rich.feed(
            &mut p,
            format!("\x1b_Gm=0;{}\x1b\\", &data[20..]).as_bytes(),
        );
        assert!(text(&fx.terminal).starts_with("\x1b_Gm=0,q=2;"));
        assert!(text(&fx.terminal).contains("a=p,U=1"));
        assert!(fx.session.is_empty(), "an image with no id gets no reply");
        assert!(
            p.screen()
                .cell(0, 0)
                .unwrap()
                .contents()
                .starts_with(PLACEHOLDER)
        );
    }

    #[test]
    fn queries_are_answered_deletes_are_passed_on_and_closing_frees_everything() {
        let mut rich = Rich {
            graphics: true,
            ..Rich::default()
        };
        let mut p = vt100::Parser::new(10, 40, 0);
        assert_eq!(
            text(
                &rich
                    .feed(&mut p, b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\")
                    .session
            ),
            "\x1b_Gi=31;OK\x1b\\"
        );
        rich.feed(&mut p, b"\x1b_Ga=t,f=24,s=1,v=1,i=5,q=2;AAAA\x1b\\");
        rich.feed(&mut p, b"\x1b_Ga=t,f=24,s=1,v=1,i=6,q=2;AAAA\x1b\\");
        let fx = rich.feed(&mut p, b"\x1b_Ga=d,d=I,i=5\x1b\\");
        assert_eq!(text(&fx.terminal).matches("a=d,d=I").count(), 1);
        let freed = text(&rich.cleanup());
        assert_eq!(
            freed.matches("a=d,d=I").count(),
            1,
            "only the image still held: {freed:?}"
        );
        assert!(rich.cleanup().is_empty());
    }

    #[test]
    fn nothing_is_sent_when_the_terminal_cannot_draw_images_or_while_replaying() {
        let mut rich = Rich::default();
        let mut p = vt100::Parser::new(10, 40, 0);
        let fx = rich.feed(
            &mut p,
            format!("x\x1b_Ga=T,f=100,i=7;{}\x1b\\y", png(90, 36)).as_bytes(),
        );
        assert!(fx.terminal.is_empty() && fx.session.is_empty());
        assert_eq!(
            p.screen().contents(),
            "xy",
            "the command is swallowed, not printed"
        );
        let mut rich = Rich {
            graphics: true,
            replaying: true,
            ..Rich::default()
        };
        let fx = rich.feed(
            &mut p,
            format!("\x1b_Ga=T,f=100,i=7;{}\x1b\\", png(90, 36)).as_bytes(),
        );
        assert!(!fx.terminal.is_empty(), "old images are shown again");
        assert!(
            fx.session.is_empty(),
            "but the program is not answered twice"
        );
    }

    #[test]
    fn an_image_is_kept_inside_the_screen() {
        let mut rich = Rich {
            graphics: true,
            cell: (10, 20),
            ..Rich::default()
        };
        let mut p = vt100::Parser::new(4, 20, 0);
        rich.feed(&mut p, b"0123456789abcde");
        let fx = rich.feed(&mut p, b"\x1b_Ga=T,f=24,s=1000,v=40,C=1;AAAA\x1b\\");
        assert!(
            text(&fx.terminal).contains("c=5,r=2"),
            "only 5 columns are left on the line: {:?}",
            text(&fx.terminal)
        );
        assert_eq!(
            p.screen().cursor_position(),
            (0, 15),
            "C=1 leaves the cursor where it was"
        );
    }
}
