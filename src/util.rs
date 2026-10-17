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
    fn strips_escapes() {
        assert_eq!(strip_ansi(b"\x1b[31mred\x1b[0m\r\nok\x1b]0;title\x07!"), "red\nok!");
    }
}
