pub struct Response {
    status: u16,
    reason: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16, reason: &str) -> Self {
        Response {
            status,
            reason: reason.to_string(),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    pub fn body(mut self, body: Vec<u8>) -> Self {
        self.body = body;
        self
    }

    pub fn error(status: u16, message: &str) -> Self {
        Response::new(status, reason_phrase(status))
            .header("Content-Type", "text/plain")
            .body(format!("{}\n", message).into_bytes())
    }

    /// Strips the body while preserving the `Content-Length` it would have
    /// had — what HEAD requires (RFC 7231 SS4.3.2: identical response
    /// headers to GET, no body).
    pub fn without_body(mut self) -> Self {
        if !self.has_header("content-length") {
            self.headers
                .push(("Content-Length".to_string(), self.body.len().to_string()));
        }
        self.body.clear();
        self
    }

    pub fn status(&self) -> u16 {
        self.status
    }

    pub fn body_len(&self) -> usize {
        self.body.len()
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = format!("HTTP/1.1 {} {}\r\n", self.status, self.reason).into_bytes();
        for (name, value) in &self.headers {
            out.extend_from_slice(format!("{}: {}\r\n", name, value).as_bytes());
        }
        if !self.has_header("content-length") {
            out.extend_from_slice(format!("Content-Length: {}\r\n", self.body.len()).as_bytes());
        }
        // RFC 7231 SS7.1.1.2: an origin server MUST send a Date header in
        // every response that has one (i.e. always, here — this server
        // always has a working clock).
        if !self.has_header("date") {
            out.extend_from_slice(format!("Date: {}\r\n", http_date()).as_bytes());
        }
        if !self.has_header("server") {
            out.extend_from_slice(b"Server: localhost/0.1\r\n");
        }
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(&self.body);
        out
    }

    fn has_header(&self, name: &str) -> bool {
        self.headers
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case(name))
    }
}

const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Current time as an RFC 7231 IMF-fixdate (`Tue, 15 Nov 1994 08:12:31 GMT`),
/// the format the `Date` header requires. Built from `libc::gmtime_r`'s raw
/// `tm` fields rather than `strftime`'s `%a`/`%b`, which are affected by the
/// process's locale (`LC_TIME`) and would silently produce a
/// spec-non-compliant header on any non-English locale.
fn http_date() -> String {
    unsafe {
        let mut t: libc::time_t = 0;
        libc::time(&mut t);
        let mut tm: libc::tm = std::mem::zeroed();
        libc::gmtime_r(&t, &mut tm);
        format!(
            "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
            WEEKDAYS[tm.tm_wday as usize],
            tm.tm_mday,
            MONTHS[tm.tm_mon as usize],
            1900 + tm.tm_year,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec,
        )
    }
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        504 => "Gateway Timeout",
        505 => "HTTP Version Not Supported",
        _ => "Error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_status_line_and_auto_content_length() {
        let response = Response::new(200, "OK");
        let bytes = response.to_bytes();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Content-Length: 0\r\n"));
        assert!(text.ends_with("\r\n\r\n"));
    }

    #[test]
    fn error_response_includes_message_body() {
        let response = Response::error(400, "bad stuff");
        let bytes = response.to_bytes();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("HTTP/1.1 400 Bad Request\r\n"));
        assert!(text.ends_with("bad stuff\n"));
    }

    #[test]
    fn includes_date_and_server_headers() {
        let text = String::from_utf8(Response::new(200, "OK").to_bytes()).unwrap();
        assert!(text.contains("\r\nServer: localhost/0.1\r\n"));
        // Loose shape check rather than an exact date (the clock ticks
        // between building the response and asserting on it): a weekday
        // abbreviation, comma, and trailing "GMT" on the Date line.
        let date_line = text
            .lines()
            .find(|line| line.starts_with("Date: "))
            .expect("Date header missing");
        assert!(date_line.ends_with("GMT"));
        assert!(date_line.contains(", "));
    }

    #[test]
    fn does_not_duplicate_headers_the_caller_already_set() {
        let text = String::from_utf8(
            Response::new(200, "OK")
                .header("Date", "custom")
                .header("Server", "custom")
                .to_bytes(),
        )
        .unwrap();
        assert_eq!(text.matches("Date:").count(), 1);
        assert_eq!(text.matches("Server:").count(), 1);
    }
}
