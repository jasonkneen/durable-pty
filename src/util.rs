use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Removes ANSI escape sequences and carriage returns, leaving readable text.
pub fn strip_ansi(input: &[u8]) -> String {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        let b = input[i];
        if b == 0x1b {
            i += 1;
            match input.get(i) {
                Some(b'[') => {
                    i += 1;
                    while i < input.len() && !(0x40..=0x7e).contains(&input[i]) {
                        i += 1;
                    }
                    i += 1;
                }
                Some(b']') => {
                    i += 1;
                    while i < input.len() {
                        if input[i] == 0x07 {
                            i += 1;
                            break;
                        }
                        if input[i] == 0x1b && input.get(i + 1) == Some(&b'\\') {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                }
                Some(_) => i += 1,
                None => {}
            }
            continue;
        }
        if b != b'\r' && b != 0x07 {
            out.push(b);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Clone, Copy, PartialEq)]
enum PlainState {
    Ground,
    Esc,
    SkipOne,
    Csi,
    Str,
    StrEsc,
}

/// Turns a terminal byte stream into searchable plain lines, across chunk boundaries. Cursor moves end a line (full-screen
/// programs position text instead of printing newlines) and lines seen recently are dropped, so redraws do not flood the index.
pub struct PlainLines {
    state: PlainState,
    pending: Vec<u8>,
    recent: std::collections::VecDeque<u64>,
}

impl Default for PlainLines {
    fn default() -> Self {
        PlainLines { state: PlainState::Ground, pending: vec![], recent: Default::default() }
    }
}

impl PlainLines {
    fn end_line(&mut self, out: &mut Vec<String>) {
        if self.pending.is_empty() {
            return;
        }
        let text = String::from_utf8_lossy(&self.pending).trim().to_string();
        self.pending.clear();
        if text.chars().filter(|c| c.is_alphanumeric()).count() < 2 {
            return;
        }
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut h);
        let h = h.finish();
        if self.recent.contains(&h) {
            return;
        }
        self.recent.push_back(h);
        if self.recent.len() > 256 {
            self.recent.pop_front();
        }
        out.push(text);
    }

    pub fn feed(&mut self, data: &[u8]) -> Vec<String> {
        use PlainState::*;
        let mut out = vec![];
        for &b in data {
            match self.state {
                Ground => match b {
                    0x1b => self.state = Esc,
                    b'\n' | b'\r' => self.end_line(&mut out),
                    b'\t' => self.pending.push(b' '),
                    0..=0x1f | 0x7f => {}
                    _ => {
                        self.pending.push(b);
                        // Break long runs at a character boundary so a line stays a sensible size.
                        if self.pending.len() >= 600 && (b < 0x80 || self.pending.len() >= 608) {
                            self.end_line(&mut out);
                        }
                    }
                },
                Esc => {
                    self.state = match b {
                        b'[' => Csi,
                        b']' | b'P' | b'_' | b'^' | b'X' => Str,
                        b'(' | b')' | b'*' | b'+' | b'#' => SkipOne,
                        _ => Ground,
                    }
                }
                SkipOne => self.state = Ground,
                Csi => {
                    if (0x40..=0x7e).contains(&b) {
                        self.state = Ground;
                        match b {
                            b'H' | b'f' | b'd' | b'A' | b'B' | b'E' | b'F' | b'J' => self.end_line(&mut out),
                            b'C' | b'G' => self.pending.push(b' '),
                            _ => {}
                        }
                    }
                }
                Str => match b {
                    0x07 => self.state = Ground,
                    0x1b => self.state = StrEsc,
                    _ => {}
                },
                StrEsc => self.state = if b == b'\\' { Ground } else { Str },
            }
        }
        out
    }
}

/// Current working directory of a process, for restoring a shell where it was.
pub fn process_cwd(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok().map(|p| p.to_string_lossy().into_owned())
    }
    #[cfg(target_os = "macos")]
    {
        let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
        let n = unsafe { libc::proc_pidinfo(pid as libc::c_int, libc::PROC_PIDVNODEPATHINFO, 0, &mut info as *mut _ as *mut libc::c_void, size) };
        if n != size {
            return None;
        }
        let path = unsafe { std::ffi::CStr::from_ptr(info.pvi_cdir.vip_path.as_ptr() as *const libc::c_char) };
        let s = path.to_string_lossy().into_owned();
        (!s.is_empty()).then_some(s)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

/// Whether a process has any child processes (a shell running a command, job or agent).
/// Errs on the side of "busy": if it cannot tell, it says true.
pub fn has_children(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        match std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")) {
            Ok(s) => !s.trim().is_empty(),
            Err(_) => true,
        }
    }
    #[cfg(target_os = "macos")]
    {
        let mut buf = [0 as libc::pid_t; 64];
        let size = std::mem::size_of_val(&buf) as libc::c_int;
        let n = unsafe { libc::proc_listchildpids(pid as libc::pid_t, buf.as_mut_ptr() as *mut libc::c_void, size) };
        n != 0
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        true
    }
}

/// Up to `width` characters of `text` around the first case-insensitive occurrence of `query`.
pub fn excerpt(text: &str, query: &str, width: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= width {
        return text.to_string();
    }
    let lower: Vec<char> = text.to_lowercase().chars().collect();
    let q: Vec<char> = query.to_lowercase().chars().collect();
    // Lowercasing can change the character count; only trust the position when it did not.
    let at = if lower.len() == chars.len() && !q.is_empty() { lower.windows(q.len()).position(|w| w == q.as_slice()).unwrap_or(0) } else { 0 };
    let start = at.saturating_sub(width / 3).min(chars.len() - width);
    let mut s: String = chars[start..start + width].iter().collect();
    if start > 0 {
        s.insert(0, '…');
    }
    if start + width < chars.len() {
        s.push('…');
    }
    s
}

/// Lines of a text file containing `query` (case-insensitive), as short excerpts. Skips files over 64 MB.
pub fn grep_file(path: &std::path::Path, query: &str, max: usize) -> Vec<String> {
    use std::io::BufRead;
    let Ok(f) = std::fs::File::open(path) else { return vec![] };
    if f.metadata().map(|m| m.len() > 64 * 1024 * 1024).unwrap_or(true) {
        return vec![];
    }
    let q = query.to_lowercase();
    let mut out = vec![];
    for line in std::io::BufReader::new(f).split(b'\n').map_while(|l| l.ok()) {
        let line = String::from_utf8_lossy(&line);
        if line.to_lowercase().contains(&q) {
            out.push(excerpt(&line, query, 160));
            if out.len() >= max {
                break;
            }
        }
    }
    out
}

/// Seconds from an env var; unset uses the default, 0 disables.
pub fn env_secs(key: &str, default: u64) -> u64 {
    std::env::var(key).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

pub fn short_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
}

pub fn basename(cmd: &str) -> String {
    std::path::Path::new(cmd).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| cmd.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn plain_lines_survive_chunk_boundaries_and_redraws() {
        let mut p = PlainLines::default();
        let mut got = p.feed(b"\x1b[31mhello wor");
        got.extend(p.feed(b"ld\x1b[0m\r\nnext\x1b"));
        got.extend(p.feed(b"[2;1Hmoved here\x1b]0;a title\x07\x1b[5Cgap\n"));
        assert_eq!(got, ["hello world", "next", "moved here gap"]);
        // A redraw of the same line is not indexed twice.
        assert!(p.feed(b"\x1b[1;1Hhello world\n").is_empty());
    }

    #[test]
    fn excerpt_centres_on_the_match() {
        let text = format!("{}NEEDLE{}", "a".repeat(300), "b".repeat(300));
        let e = excerpt(&text, "needle", 60);
        assert!(e.contains("NEEDLE") && e.starts_with('…') && e.ends_with('…') && e.chars().count() == 62);
        assert_eq!(excerpt("short", "x", 60), "short");
    }

    #[test]
    fn strips_escapes() {
        assert_eq!(strip_ansi(b"\x1b[31mred\x1b[0m\r\nok\x1b]0;title\x07!"), "red\nok!");
    }
}
