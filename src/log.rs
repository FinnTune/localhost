use std::net::SocketAddr;

pub fn green(s: &str) -> String {
    format!("\x1b[32m{}\x1b[0m", s)
}

pub fn blue(s: &str) -> String {
    format!("\x1b[34m{}\x1b[0m", s)
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Current local time as `10/Oct/2023:13:55:36 +0000`, the timestamp format
/// Combined Log Format uses. Built from `libc::localtime_r`'s raw `tm`
/// fields (including the glibc `tm_gmtoff` extension for the numeric UTC
/// offset) rather than `strftime`'s `%b`/`%z`, since `%b` is affected by the
/// process's locale (`LC_TIME`) and would silently produce month
/// abbreviations Combined Log Format doesn't expect on any non-English
/// locale.
fn timestamp() -> String {
    unsafe {
        let mut t: libc::time_t = 0;
        libc::time(&mut t);
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);

        let offset_minutes = tm.tm_gmtoff / 60;
        let sign = if offset_minutes < 0 { '-' } else { '+' };
        let offset_minutes = offset_minutes.abs();

        format!(
            "{:02}/{}/{}:{:02}:{:02}:{:02} {}{:02}{:02}",
            tm.tm_mday,
            MONTHS[tm.tm_mon as usize],
            1900 + tm.tm_year,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec,
            sign,
            offset_minutes / 60,
            offset_minutes % 60,
        )
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
