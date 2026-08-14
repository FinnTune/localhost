use std::net::SocketAddr;

pub fn green(s: &str) -> String {
    format!("\x1b[32m{}\x1b[0m", s)
}

pub fn blue(s: &str) -> String {
    format!("\x1b[34m{}\x1b[0m", s)
}

/// Current local time as `10/Oct/2023:13:55:36 +0000`, the timestamp format
/// Combined Log Format uses. Hand-rolled via `libc::strftime` rather than a
/// `time`/`chrono` crate, matching this repo's libc-only dependency policy.
fn timestamp() -> String {
    unsafe {
        let mut t: libc::time_t = 0;
        libc::time(&mut t);
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);

        let fmt = b"%d/%b/%Y:%H:%M:%S %z\0";
        let mut buf = [0 as libc::c_char; 32];
        let len = libc::strftime(
            buf.as_mut_ptr(),
            buf.len(),
            fmt.as_ptr() as *const libc::c_char,
            &tm,
        );
        let bytes: Vec<u8> = buf[..len].iter().map(|&c| c as u8).collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

/// Writes one line to stdout in Combined Log Format — the format nginx and
/// Apache both use, so it's parseable by existing tools (goaccess, awk,
/// etc.) without a custom format string. `-` stands in for fields this
/// server has no equivalent of (remote user, missing headers) or for
/// requests that never parsed into a method/path/version at all.
#[allow(clippy::too_many_arguments)]
pub fn access(
    peer: &SocketAddr,
    method: &str,
    path: &str,
    version: &str,
    referer: Option<&str>,
    user_agent: Option<&str>,
    status: u16,
    body_len: usize,
) {
    println!(
        "{} - - [{}] \"{} {} {}\" {} {} \"{}\" \"{}\"",
        peer.ip(),
        timestamp(),
        method,
        path,
        version,
        status,
        body_len,
        referer.unwrap_or("-"),
        user_agent.unwrap_or("-"),
    );
}
