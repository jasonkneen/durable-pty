use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
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
        PlainLines {
            state: PlainState::Ground,
            pending: vec![],
            recent: Default::default(),
        }
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
                            b'H' | b'f' | b'd' | b'A' | b'B' | b'E' | b'F' | b'J' => {
                                self.end_line(&mut out)
                            }
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
        std::fs::read_link(format!("/proc/{pid}/cwd"))
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
    }
    #[cfg(target_os = "macos")]
    {
        let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
        let n = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                &mut info as *mut _ as *mut libc::c_void,
                size,
            )
        };
        if n != size {
            return None;
        }
        let path = unsafe {
            std::ffi::CStr::from_ptr(info.pvi_cdir.vip_path.as_ptr() as *const libc::c_char)
        };
        let s = path.to_string_lossy().into_owned();
        (!s.is_empty()).then_some(s)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

/// The parent of a process, when it can be found.
pub fn parent_pid(pid: u32) -> Option<u32> {
    #[cfg(target_os = "linux")]
    {
        // "pid (comm) state ppid ..."; comm may contain spaces and brackets, so read after the last ')'.
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        stat.rsplit_once(')')?
            .1
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()
    }
    #[cfg(target_os = "macos")]
    {
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let n = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDTBSDINFO,
                0,
                &mut info as *mut _ as *mut libc::c_void,
                size,
            )
        };
        (n == size).then_some(info.pbi_ppid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

/// A process and its ancestors, nearest first.
pub fn ancestry(pid: u32) -> Vec<u32> {
    let mut chain = vec![pid];
    let mut cur = pid;
    while chain.len() < 64 {
        match parent_pid(cur) {
            Some(p) if p > 1 && !chain.contains(&p) => {
                chain.push(p);
                cur = p;
            }
            _ => break,
        }
    }
    chain
}

/// A running process whose executable is named `dpty`, with its argv.
#[derive(Debug, PartialEq, Eq)]
pub struct ReportedProc {
    pub pid: u32,
    pub role: String,
    pub session: String,
    pub cmd: String,
}

/// Classifies the `dpty` processes the OS is running. `skip` is the process asking, which exits as soon as it has
/// the answer. A pid the daemon owns is a shim or a client of that session; anything else keeps the command it was
/// started as (`attach`, `mcp`, …) so a process with no session is still on the list.
pub fn classify_dpty(
    daemon: u32,
    shims: &std::collections::HashMap<u32, String>,
    clients: &std::collections::HashMap<u32, String>,
    running: &[(u32, Vec<String>)],
    skip: Option<u32>,
) -> Vec<ReportedProc> {
    let mut rows = Vec::new();
    for (pid, args) in running {
        if skip.is_some_and(|s| s == *pid) {
            continue;
        }
        let (role, session) = if *pid == daemon {
            ("daemon".to_string(), String::new())
        } else if let Some(name) = shims.get(pid) {
            ("shim".to_string(), name.clone())
        } else if let Some(name) = clients.get(pid) {
            ("client".to_string(), name.clone())
        } else if args.get(1).map(String::as_str) == Some("<defunct>") {
            ("zombie".to_string(), String::new())
        } else {
            (subcommand(args), String::new())
        };
        rows.push(ReportedProc {
            pid: *pid,
            role,
            session,
            cmd: short_cmd(args),
        });
    }
    rows.sort_by(|a, b| {
        role_rank(&a.role)
            .cmp(&role_rank(&b.role))
            .then(a.pid.cmp(&b.pid))
    });
    rows
}

fn role_rank(role: &str) -> u8 {
    match role {
        "daemon" => 0,
        "shim" => 1,
        "client" => 2,
        _ => 3,
    }
}

fn subcommand(args: &[String]) -> String {
    args.get(1)
        .filter(|s| !s.starts_with('-'))
        .cloned()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "other".to_string())
}

fn short_cmd(args: &[String]) -> String {
    args.iter()
        .enumerate()
        .map(|(i, a)| {
            if i == 0 {
                "dpty".to_string()
            } else if let Some(name) = std::path::Path::new(a).file_name().and_then(|n| n.to_str())
                && name.ends_with(".spec")
            {
                name.to_string()
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// argv from a `KERN_PROCARGS2` buffer: argc, the executable, padding NULs, then argc argument strings.
fn argv_from_procargs2(buf: &[u8]) -> Vec<String> {
    if buf.len() < 4 {
        return vec![];
    }
    let Ok(argc_b) = buf[..4].try_into() else {
        return vec![];
    };
    let argc = i32::from_ne_bytes(argc_b);
    if argc <= 0 {
        return vec![];
    }
    let mut i = 4;
    while i < buf.len() && buf[i] != 0 {
        i += 1;
    }
    while i < buf.len() && buf[i] == 0 {
        i += 1;
    }
    let mut args = Vec::with_capacity(argc as usize);
    for _ in 0..argc {
        if i >= buf.len() {
            break;
        }
        let start = i;
        while i < buf.len() && buf[i] != 0 {
            i += 1;
        }
        args.push(String::from_utf8_lossy(&buf[start..i]).into_owned());
        i += 1;
    }
    args
}

fn exe_is_dpty(path: &std::path::Path) -> bool {
    path.file_name().and_then(|n| n.to_str()) == Some("dpty")
}

/// Every process whose name is `dpty`, including a zombie Activity Monitor still lists.
/// A zombie has no executable path, so its argv is `dpty <defunct>`.
pub fn dpty_processes() -> Vec<(u32, Vec<String>)> {
    #[cfg(target_os = "macos")]
    {
        macos_dpty_processes()
    }
    #[cfg(target_os = "linux")]
    {
        linux_dpty_processes()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Vec::new()
    }
}

#[cfg(target_os = "linux")]
fn linux_dpty_processes() -> Vec<(u32, Vec<String>)> {
    let mut out = vec![];
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return out;
    };
    for ent in dir.flatten() {
        let Ok(pid) = ent.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let exe = std::fs::read_link(format!("/proc/{pid}/exe"));
        if exe.as_ref().is_ok_and(|p| !exe_is_dpty(p)) {
            continue;
        }
        if exe.is_err() {
            if linux_zombie_named_dpty(pid) {
                out.push((pid, vec!["dpty".into(), "<defunct>".into()]));
            }
            continue;
        }
        let raw = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
        let args: Vec<String> = raw
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect();
        out.push((
            pid,
            if args.is_empty() {
                vec!["dpty".into()]
            } else {
                args
            },
        ));
    }
    out
}

#[cfg(target_os = "linux")]
fn linux_zombie_named_dpty(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let Some(open) = stat.find('(') else {
        return false;
    };
    let Some(close) = stat.rfind(')') else {
        return false;
    };
    if close <= open {
        return false;
    }
    let name = &stat[open + 1..close];
    let state = stat[close + 1..].split_whitespace().next();
    name == "dpty" && state == Some("Z")
}

#[cfg(target_os = "macos")]
fn macos_dpty_processes() -> Vec<(u32, Vec<String>)> {
    macos_dpty_by_name().unwrap_or_else(macos_dpty_by_path)
}

/// `sizeof(struct kinfo_proc)`, and the `extern_proc` offsets of `p_stat`, `p_pid` and `p_comm`,
/// from the macOS SDK (`sys/proc.h`, `sys/sysctl.h`) on LP64. `p_comm` is `MAXCOMLEN + 1` bytes.
/// Confirmed with `offsetof` on this OS. A record that does not name this process is rejected,
/// and the scan falls back to `proc_pidpath`.
#[cfg(target_os = "macos")]
const KINFO_PROC_SIZE: usize = 648;
#[cfg(target_os = "macos")]
const P_STAT_OFF: usize = 36;
#[cfg(target_os = "macos")]
const P_PID_OFF: usize = 40;
#[cfg(target_os = "macos")]
const P_COMM_OFF: usize = 243;
#[cfg(target_os = "macos")]
const P_COMM_LEN: usize = 17;

/// Processes whose kernel name is `dpty`. This is the name Activity Monitor shows, and it still
/// exists after the process has become a zombie. `None` means the `kinfo_proc` layout did not
/// check out, so the caller must use another scan.
#[cfg(target_os = "macos")]
fn macos_dpty_by_name() -> Option<Vec<(u32, Vec<String>)>> {
    let buf = macos_kinfo()?;
    let (recs, rest) = buf.as_chunks::<KINFO_PROC_SIZE>();
    if !rest.is_empty() {
        return None;
    }
    let mine = macos_proc_name(std::process::id())?;
    let mut saw_self = false;
    let mut out = Vec::new();
    for rec in recs {
        let Some(pid) = rec_pid(rec) else {
            continue;
        };
        let Some(comm) = rec_comm(rec) else {
            continue;
        };
        if pid == std::process::id() {
            // `proc_name` can return more than the 16 bytes stored in `p_comm`.
            let expected = &mine.as_bytes()[..mine.len().min(16)];
            if comm.as_bytes() != expected {
                return None;
            }
            saw_self = true;
        }
        if comm != "dpty" {
            continue;
        }
        let zombie = rec[P_STAT_OFF] == libc::SZOMB as u8;
        let args = if zombie {
            vec!["dpty".into(), "<defunct>".into()]
        } else {
            macos_argv(pid)
                .filter(|a| !a.is_empty())
                .unwrap_or_else(|| vec!["dpty".into()])
        };
        out.push((pid, args));
    }
    saw_self.then_some(out)
}

#[cfg(target_os = "macos")]
fn rec_pid(rec: &[u8]) -> Option<u32> {
    let bytes: [u8; 4] = rec.get(P_PID_OFF..P_PID_OFF + 4)?.try_into().ok()?;
    u32::try_from(i32::from_ne_bytes(bytes))
        .ok()
        .filter(|p| *p > 0)
}

#[cfg(target_os = "macos")]
fn rec_comm(rec: &[u8]) -> Option<String> {
    let bytes = rec.get(P_COMM_OFF..P_COMM_OFF + P_COMM_LEN)?;
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    let name = std::str::from_utf8(&bytes[..end]).ok()?;
    (!name.is_empty()).then(|| name.to_string())
}

#[cfg(target_os = "macos")]
fn macos_proc_name(pid: u32) -> Option<String> {
    // `proc_name` returns 0 unless the buffer is at least the 32-byte `pbi_name` field.
    let mut buf = [0u8; 32];
    // SAFETY: buf is 32 bytes. On success proc_name writes a NUL-terminated name and returns its length.
    let n = unsafe {
        libc::proc_name(
            pid as libc::pid_t,
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len() as u32,
        )
    };
    if n <= 0 {
        return None;
    }
    let n = usize::try_from(n).ok()?;
    let name = std::str::from_utf8(buf.get(..n)?).ok()?;
    (!name.is_empty()).then(|| name.to_string())
}

#[cfg(target_os = "macos")]
fn macos_kinfo() -> Option<Vec<u8>> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_ALL, 0];
    for _ in 0..4 {
        let mut size: libc::size_t = 0;
        // SAFETY: a null output pointer asks for the byte count only.
        if unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as u32,
                std::ptr::null_mut(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        } != 0
            || size == 0
        {
            return None;
        }
        let mut buf = vec![0u8; size];
        let mut got = size;
        // SAFETY: buf is `size` bytes, the length sysctl just reported. On ENOMEM the table grew
        // and the loop retries.
        let rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as u32,
                buf.as_mut_ptr() as *mut libc::c_void,
                &mut got,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc == 0 {
            buf.truncate(got);
            return Some(buf);
        }
        if std::io::Error::last_os_error().raw_os_error() != Some(libc::ENOMEM) {
            return None;
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn macos_dpty_by_path() -> Vec<(u32, Vec<String>)> {
    let mut cap = 512i32;
    let mut pids = vec![0i32; cap as usize];
    let got = loop {
        let bytes = (pids.len() * std::mem::size_of::<i32>()) as i32;
        // SAFETY: pids is a writable buffer of `bytes` bytes. A null buffer is not passed.
        let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr() as *mut libc::c_void, bytes) };
        if n < 0 {
            return vec![];
        }
        if n < cap {
            break n;
        }
        cap *= 2;
        pids.resize(cap as usize, 0);
    };
    let mut out = vec![];
    for pid in pids.into_iter().take(got as usize).filter(|p| *p > 0) {
        let mut path = [0i8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        // SAFETY: path is PROC_PIDPATHINFO_MAXSIZE bytes. On success proc_pidpath writes a NUL-terminated path into it.
        let n = unsafe {
            libc::proc_pidpath(
                pid,
                path.as_mut_ptr() as *mut libc::c_void,
                path.len() as u32,
            )
        };
        if n <= 0 {
            continue;
        }
        // SAFETY: a positive return means the buffer holds a NUL-terminated path.
        let text = unsafe { std::ffi::CStr::from_ptr(path.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        if !exe_is_dpty(std::path::Path::new(&text)) {
            continue;
        }
        let args = macos_argv(pid as u32)
            .filter(|a| !a.is_empty())
            .unwrap_or_else(|| vec![text]);
        out.push((pid as u32, args));
    }
    out
}

#[cfg(target_os = "macos")]
fn macos_argv(pid: u32) -> Option<Vec<String>> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as i32];
    let mut size: libc::size_t = 0;
    // SAFETY: the first call only asks for the buffer size. No output pointer is written.
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
        || size < 4
    {
        return None;
    }
    let mut buf = vec![0u8; size];
    // SAFETY: buf is `size` bytes, which is the length sysctl just reported.
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    buf.truncate(size);
    let args = argv_from_procargs2(&buf);
    (!args.is_empty()).then_some(args)
}

/// The command line of the program a shell is running: the argv of its first child process, or `None` when the shell
/// is idle at its prompt.
pub fn child_job(pid: u32) -> Option<Vec<String>> {
    #[cfg(target_os = "linux")]
    {
        let kids = std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")).ok()?;
        let child: u32 = kids.split_whitespace().next()?.parse().ok()?;
        let raw = std::fs::read(format!("/proc/{child}/cmdline")).ok()?;
        let args: Vec<String> = raw
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect();
        (!args.is_empty()).then_some(args)
    }
    #[cfg(target_os = "macos")]
    {
        let mut buf = [0 as libc::pid_t; 64];
        let size = std::mem::size_of_val(&buf) as libc::c_int;
        // SAFETY: buf is `size` bytes; proc_listchildpids writes at most that many and returns the count.
        let n = unsafe {
            libc::proc_listchildpids(
                pid as libc::pid_t,
                buf.as_mut_ptr() as *mut libc::c_void,
                size,
            )
        };
        let child = *buf.get(..n.max(0) as usize)?.iter().min()?;
        macos_argv(child as u32)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

/// Quotes argv so a POSIX shell reads it back as the same words.
pub fn shell_quote(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if !a.is_empty()
                && a.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_./=:,+@%".contains(c))
            {
                a.clone()
            } else {
                format!("'{}'", a.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
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
        let n = unsafe {
            libc::proc_listchildpids(
                pid as libc::pid_t,
                buf.as_mut_ptr() as *mut libc::c_void,
                size,
            )
        };
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
    let at = if lower.len() == chars.len() && !q.is_empty() {
        lower
            .windows(q.len())
            .position(|w| w == q.as_slice())
            .unwrap_or(0)
    } else {
        0
    };
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
    let Ok(f) = std::fs::File::open(path) else {
        return vec![];
    };
    if f.metadata()
        .map(|m| m.len() > 64 * 1024 * 1024)
        .unwrap_or(true)
    {
        return vec![];
    }
    let q = query.to_lowercase();
    let mut out = vec![];
    for line in std::io::BufReader::new(f)
        .split(b'\n')
        .map_while(|l| l.ok())
    {
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
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

pub fn short_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
}

pub fn basename(cmd: &str) -> String {
    std::path::Path::new(cmd)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| cmd.to_string())
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
        assert!(
            e.contains("NEEDLE")
                && e.starts_with('…')
                && e.ends_with('…')
                && e.chars().count() == 62
        );
        assert_eq!(excerpt("short", "x", 60), "short");
    }

    #[test]
    fn strips_escapes() {
        assert_eq!(
            strip_ansi(b"\x1b[31mred\x1b[0m\r\nok\x1b]0;title\x07!"),
            "red\nok!"
        );
    }

    #[test]
    fn procargs2_argv_skips_the_executable_and_the_padding() {
        let mut buf = Vec::new();
        buf.extend(3i32.to_ne_bytes());
        buf.extend(b"/usr/local/bin/dpty\0\0\0");
        buf.extend(b"/usr/local/bin/dpty\0attach\0infinitty-daf804fc\0");
        assert_eq!(
            argv_from_procargs2(&buf),
            vec!["/usr/local/bin/dpty", "attach", "infinitty-daf804fc"]
        );
        assert!(argv_from_procargs2(&[]).is_empty());
        assert!(argv_from_procargs2(&0i32.to_ne_bytes()).is_empty());
    }

    #[test]
    fn every_running_dpty_process_is_reported_once() {
        let mut shims = std::collections::HashMap::new();
        shims.insert(1057, "shell-b170".into());
        let mut clients = std::collections::HashMap::new();
        clients.insert(1040, "shell-b170".into());
        let running = vec![
            (
                1039,
                vec!["/Users/jkneen/.cargo/bin/dpty".into(), "serve".into()],
            ),
            (
                1057,
                vec![
                    "/Users/jkneen/.cargo/bin/dpty".into(),
                    "shim".into(),
                    "/Users/jkneen/.durable-pty/shims/b17042cc.spec".into(),
                ],
            ),
            (
                1040,
                vec![
                    "/Users/jkneen/.cargo/bin/dpty".into(),
                    "new".into(),
                    "--env-all".into(),
                ],
            ),
            (
                28035,
                vec![
                    "/Users/jkneen/.cargo/bin/dpty".into(),
                    "attach".into(),
                    "infinitty-daf804fc".into(),
                ],
            ),
            (
                99999,
                vec!["/Users/jkneen/.cargo/bin/dpty".into(), "ls".into()],
            ),
            (1042, vec!["dpty".into(), "<defunct>".into()]),
        ];
        let rows = classify_dpty(1039, &shims, &clients, &running, Some(99999));
        assert_eq!(
            rows,
            vec![
                ReportedProc {
                    pid: 1039,
                    role: "daemon".into(),
                    session: String::new(),
                    cmd: "dpty serve".into()
                },
                ReportedProc {
                    pid: 1057,
                    role: "shim".into(),
                    session: "shell-b170".into(),
                    cmd: "dpty shim b17042cc.spec".into()
                },
                ReportedProc {
                    pid: 1040,
                    role: "client".into(),
                    session: "shell-b170".into(),
                    cmd: "dpty new --env-all".into()
                },
                ReportedProc {
                    pid: 1042,
                    role: "zombie".into(),
                    session: String::new(),
                    cmd: "dpty <defunct>".into()
                },
                ReportedProc {
                    pid: 28035,
                    role: "attach".into(),
                    session: String::new(),
                    cmd: "dpty attach infinitty-daf804fc".into()
                },
            ]
        );
    }

    /// The reported pid set is the set of processes whose name is `dpty`, zombies included.
    /// A process that forks or exits between the two `ps` calls is not required to match.
    #[cfg(target_os = "macos")]
    #[test]
    fn every_process_named_dpty_is_listed() {
        fn named() -> std::collections::BTreeSet<u32> {
            let out = std::process::Command::new("ps")
                .args(["-ax", "-o", "pid=,ucomm="])
                .output()
                .expect("ps");
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter_map(|line| {
                    let mut parts = line.split_whitespace();
                    let pid = parts.next()?.parse::<u32>().ok()?;
                    (parts.next() == Some("dpty")).then_some(pid)
                })
                .collect()
        }
        let before = named();
        let got: std::collections::BTreeSet<u32> =
            dpty_processes().into_iter().map(|(pid, _)| pid).collect();
        let after = named();
        let stable = &before & &after;
        assert!(
            stable.is_subset(&got),
            "missing from dpty scan: {:?}",
            &stable - &got
        );
        let seen = &before | &after;
        assert!(
            got.is_subset(&seen),
            "scan reported a pid ps did not: {:?}",
            &got - &seen
        );
    }
}

#[cfg(test)]
mod job_tests {
    use super::*;

    #[test]
    fn shell_quote_round_trips_awkward_words() {
        let args: Vec<String> = ["npm", "run", "dev", "it's here", "", "a b"]
            .map(String::from)
            .to_vec();
        let line = shell_quote(&args);
        assert_eq!(line, "npm run dev 'it'\\''s here' '' 'a b'");
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("for a in {line}; do printf '[%s]' \"$a\"; done"))
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "[npm][run][dev][it's here][][a b]"
        );
    }

    #[test]
    fn child_job_reads_the_running_command() {
        // A shell of its own, so other tests' child processes are not in the way. The trailing `:` keeps sh from
        // exec'ing sleep in its place.
        let mut shell = std::process::Command::new("sh")
            .args(["-c", "sleep 31; :"])
            .spawn()
            .unwrap();
        let mut job = None;
        for _ in 0..50 {
            job = child_job(shell.id());
            if job.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let _ = std::process::Command::new("pkill")
            .args(["-P", &shell.id().to_string()])
            .status();
        shell.kill().unwrap();
        let _ = shell.wait();
        assert_eq!(job, Some(vec!["sleep".to_string(), "31".to_string()]));
    }
}
