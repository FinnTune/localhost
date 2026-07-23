use std::collections::HashMap;
use std::str;

const MAX_HEADER_BYTES: usize = 8 * 1024;
const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Put,
    Delete,
    Head,
    Options,
    Patch,
    Other(String),
}

impl Method {
    fn parse(raw: &str) -> Method {
        match raw {
            "GET" => Method::Get,
            "POST" => Method::Post,
            "PUT" => Method::Put,
            "DELETE" => Method::Delete,
            "HEAD" => Method::Head,
            "OPTIONS" => Method::Options,
            "PATCH" => Method::Patch,
            other => Method::Other(other.to_string()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Delete => "DELETE",
            Method::Head => "HEAD",
            Method::Options => "OPTIONS",
            Method::Patch => "PATCH",
            Method::Other(raw) => raw,
        }
    }
}

#[derive(Debug)]
pub struct Request {
    pub method: Method,
    pub path: String,
    // Not read yet: query strings and Content-Type are CGI (Phase 7) concerns.
    #[allow(dead_code)]
    pub query: Option<String>,
    pub version: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }

    /// Whether this connection should stay open for another request after
    /// this one, per the HTTP/1.1 (default persistent) vs HTTP/1.0 (default
    /// non-persistent) rules and any explicit `Connection` header override.
    pub fn keep_alive(&self) -> bool {
        match self.header("connection") {
            Some(value) if value.eq_ignore_ascii_case("close") => false,
            Some(value) if value.eq_ignore_ascii_case("keep-alive") => true,
            _ => self.version != "HTTP/1.0",
        }
    }
}

/// Result of feeding the accumulated connection buffer to the parser.
/// `Incomplete` means more bytes are needed before a decision can be made;
/// callers should keep reading and re-parse from the start of the buffer.
pub enum ParseOutcome {
    Incomplete,
    Complete { request: Request, consumed: usize },
    Invalid { status: u16, message: String },
}

pub fn parse(buffer: &[u8]) -> ParseOutcome {
    let header_end = match find_header_end(buffer) {
        Some(idx) => idx,
        None => {
            if buffer.len() > MAX_HEADER_BYTES {
                return ParseOutcome::Invalid {
                    status: 431,
                    message: "Request header fields too large".to_string(),
                };
            }
            return ParseOutcome::Incomplete;
        }
    };

    let head = match str::from_utf8(&buffer[..header_end]) {
        Ok(s) => s,
        Err(_) => {
            return ParseOutcome::Invalid {
                status: 400,
                message: "Request headers are not valid UTF-8".to_string(),
            }
        }
    };

    let mut lines = head.split("\r\n");
    let request_line = match lines.next() {
        Some(line) if !line.is_empty() => line,
        _ => {
            return ParseOutcome::Invalid {
                status: 400,
                message: "Missing request line".to_string(),
            }
        }
    };

    let tokens: Vec<&str> = request_line.split(' ').collect();
    if tokens.len() != 3 {
        return ParseOutcome::Invalid {
            status: 400,
            message: format!("Malformed request line: '{}'", request_line),
        };
    }
    let (method_str, target, version) = (tokens[0], tokens[1], tokens[2]);
    if !version.starts_with("HTTP/") {
        return ParseOutcome::Invalid {
            status: 400,
            message: format!("Unsupported protocol version: '{}'", version),
        };
    }

    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), Some(q.to_string())),
        None => (target.to_string(), None),
    };

    let mut headers = HashMap::new();
    for line in lines {
        let (name, value) = match line.split_once(':') {
            Some(pair) => pair,
            None => {
                return ParseOutcome::Invalid {
                    status: 400,
                    message: format!("Malformed header line: '{}'", line),
                }
            }
        };
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
    }

    let body_start = header_end + 4;
    let is_chunked = headers
        .get("transfer-encoding")
        .is_some_and(|v| v.eq_ignore_ascii_case("chunked"));

    let (body, consumed) = if is_chunked {
        match decode_chunked_body(buffer, body_start) {
            ChunkedOutcome::Incomplete => return ParseOutcome::Incomplete,
            ChunkedOutcome::Invalid(message) => {
                return ParseOutcome::Invalid {
                    status: 400,
                    message,
                }
            }
            ChunkedOutcome::Complete { body, end } => (body, end),
        }
    } else {
        let content_length = match headers.get("content-length") {
            Some(v) => match v.parse::<usize>() {
                Ok(n) => n,
                Err(_) => {
                    return ParseOutcome::Invalid {
                        status: 400,
                        message: format!("Invalid Content-Length header: '{}'", v),
                    }
                }
            },
            None => 0,
        };

        if content_length > MAX_BODY_BYTES {
            return ParseOutcome::Invalid {
                status: 413,
                message: "Request body exceeds maximum allowed size".to_string(),
            };
        }

        let total_needed = body_start + content_length;
        if buffer.len() < total_needed {
            return ParseOutcome::Incomplete;
        }

        (buffer[body_start..total_needed].to_vec(), total_needed)
    };

    ParseOutcome::Complete {
        request: Request {
            method: Method::parse(method_str),
            path,
            query,
            version: version.to_string(),
            headers,
            body,
        },
        consumed,
    }
}

enum ChunkedOutcome {
    Incomplete,
    Invalid(String),
    Complete { body: Vec<u8>, end: usize },
}

/// Decodes a chunked request body starting at `start` (right after the
/// header terminator). No chunk extensions or trailer headers are
/// supported; a zero-size chunk must be followed immediately by `\r\n`.
fn decode_chunked_body(buffer: &[u8], start: usize) -> ChunkedOutcome {
    let mut body = Vec::new();
    let mut pos = start;

    loop {
        let size_line_end = match find_crlf(buffer, pos) {
            Some(idx) => idx,
            None => return ChunkedOutcome::Incomplete,
        };
        let size_line = match str::from_utf8(&buffer[pos..size_line_end]) {
            Ok(s) => s,
            Err(_) => return ChunkedOutcome::Invalid("Invalid chunk size line".to_string()),
        };
        let size_str = size_line.split(';').next().unwrap_or("").trim();
        let size = match usize::from_str_radix(size_str, 16) {
            Ok(n) => n,
            Err(_) => {
                return ChunkedOutcome::Invalid(format!("Invalid chunk size: '{}'", size_str))
            }
        };

        let chunk_start = size_line_end + 2;

        if size == 0 {
            let terminator_end = chunk_start + 2;
            if buffer.len() < terminator_end {
                return ChunkedOutcome::Incomplete;
            }
            if &buffer[chunk_start..terminator_end] != b"\r\n" {
                return ChunkedOutcome::Invalid("Malformed chunked body terminator".to_string());
            }
            return ChunkedOutcome::Complete {
                body,
                end: terminator_end,
            };
        }

        if body.len() + size > MAX_BODY_BYTES {
            return ChunkedOutcome::Invalid(
                "Request body exceeds maximum allowed size".to_string(),
            );
        }

        let chunk_end = chunk_start + size;
        if buffer.len() < chunk_end + 2 {
            return ChunkedOutcome::Incomplete;
        }
        if &buffer[chunk_end..chunk_end + 2] != b"\r\n" {
            return ChunkedOutcome::Invalid("Malformed chunk terminator".to_string());
        }

        body.extend_from_slice(&buffer[chunk_start..chunk_end]);
        pos = chunk_end + 2;
    }
}

fn find_crlf(buffer: &[u8], from: usize) -> Option<usize> {
    if from > buffer.len() {
        return None;
    }
    buffer[from..]
        .windows(2)
        .position(|w| w == b"\r\n")
        .map(|i| i + from)
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|w| w == b"\r\n\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expect_complete(buffer: &[u8]) -> (Request, usize) {
        match parse(buffer) {
            ParseOutcome::Complete { request, consumed } => (request, consumed),
            ParseOutcome::Incomplete => panic!("expected Complete, got Incomplete"),
            ParseOutcome::Invalid { status, message } => {
                panic!("expected Complete, got Invalid({}, {})", status, message)
            }
        }
    }

    #[test]
    fn parses_simple_get() {
        let raw = b"GET /about?x=1 HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let (request, consumed) = expect_complete(raw);
        assert_eq!(request.method, Method::Get);
        assert_eq!(request.path, "/about");
        assert_eq!(request.query.as_deref(), Some("x=1"));
        assert_eq!(request.header("host"), Some("localhost"));
        assert_eq!(consumed, raw.len());
    }

    #[test]
    fn reports_incomplete_headers() {
        let raw = b"GET / HTTP/1.1\r\nHost: localhost\r\n";
        assert!(matches!(parse(raw), ParseOutcome::Incomplete));
    }

    #[test]
    fn waits_for_full_body() {
        let raw = b"POST /submit HTTP/1.1\r\nContent-Length: 5\r\n\r\nhi";
        assert!(matches!(parse(raw), ParseOutcome::Incomplete));

        let raw_full = b"POST /submit HTTP/1.1\r\nContent-Length: 5\r\n\r\nhello";
        let (request, consumed) = expect_complete(raw_full);
        assert_eq!(request.method, Method::Post);
        assert_eq!(request.body, b"hello");
        assert_eq!(consumed, raw_full.len());
    }

    #[test]
    fn rejects_malformed_request_line() {
        let raw = b"GET /\r\n\r\n";
        match parse(raw) {
            ParseOutcome::Invalid { status, .. } => assert_eq!(status, 400),
            _ => panic!("expected Invalid(400)"),
        }
    }

    #[test]
    fn rejects_bad_content_length() {
        let raw = b"GET / HTTP/1.1\r\nContent-Length: not-a-number\r\n\r\n";
        match parse(raw) {
            ParseOutcome::Invalid { status, .. } => assert_eq!(status, 400),
            _ => panic!("expected Invalid(400)"),
        }
    }

    #[test]
    fn pipelined_requests_report_correct_consumed_length() {
        let raw = b"GET / HTTP/1.1\r\nHost: a\r\n\r\nGET /next HTTP/1.1\r\nHost: a\r\n\r\n";
        let (request, consumed) = expect_complete(raw);
        assert_eq!(request.path, "/");
        assert!(consumed < raw.len());
        let (next_request, _) = expect_complete(&raw[consumed..]);
        assert_eq!(next_request.path, "/next");
    }

    #[test]
    fn decodes_chunked_body() {
        let raw = b"POST /submit HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        let (request, consumed) = expect_complete(raw);
        assert_eq!(request.body, b"Wikipedia");
        assert_eq!(consumed, raw.len());
    }

    #[test]
    fn waits_for_full_chunked_body() {
        let raw = b"POST /submit HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nWiki\r\n";
        assert!(matches!(parse(raw), ParseOutcome::Incomplete));
    }

    #[test]
    fn rejects_malformed_chunk_size() {
        let raw =
            b"POST /submit HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\nhi\r\n0\r\n\r\n";
        match parse(raw) {
            ParseOutcome::Invalid { status, .. } => assert_eq!(status, 400),
            _ => panic!("expected Invalid(400)"),
        }
    }

    #[test]
    fn http_1_1_defaults_to_keep_alive() {
        let raw = b"GET / HTTP/1.1\r\nHost: a\r\n\r\n";
        let (request, _) = expect_complete(raw);
        assert!(request.keep_alive());
    }

    #[test]
    fn http_1_0_defaults_to_close() {
        let raw = b"GET / HTTP/1.0\r\nHost: a\r\n\r\n";
        let (request, _) = expect_complete(raw);
        assert!(!request.keep_alive());
    }

    #[test]
    fn explicit_connection_header_overrides_defaults() {
        let raw = b"GET / HTTP/1.1\r\nConnection: close\r\n\r\n";
        let (request, _) = expect_complete(raw);
        assert!(!request.keep_alive());

        let raw = b"GET / HTTP/1.0\r\nConnection: keep-alive\r\n\r\n";
        let (request, _) = expect_complete(raw);
        assert!(request.keep_alive());
    }
}
