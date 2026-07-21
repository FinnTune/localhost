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
    // query/version/headers/body aren't read yet: routing (Phase 3), method
    // enforcement (Phase 4), and CGI (Phase 7) consume these.
    #[allow(dead_code)]
    pub query: Option<String>,
    #[allow(dead_code)]
    pub version: String,
    #[allow(dead_code)]
    pub headers: HashMap<String, String>,
    #[allow(dead_code)]
    pub body: Vec<u8>,
}

impl Request {
    #[allow(dead_code)] // used once routing/CGI need to read specific headers (Phase 3/7)
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }
}

/// Result of feeding the accumulated connection buffer to the parser.
/// `Incomplete` means more bytes are needed before a decision can be made;
/// callers should keep reading and re-parse from the start of the buffer.
pub enum ParseOutcome {
    Incomplete,
    Complete {
        request: Request,
        // Not read yet: keep-alive (Phase 5) uses this to find the next
        // pipelined request in the same connection buffer.
        #[allow(dead_code)]
        consumed: usize,
    },
    Invalid {
        status: u16,
        message: String,
    },
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

    let body_start = header_end + 4;
    let total_needed = body_start + content_length;
    if buffer.len() < total_needed {
        return ParseOutcome::Incomplete;
    }

    let body = buffer[body_start..total_needed].to_vec();

    ParseOutcome::Complete {
        request: Request {
            method: Method::parse(method_str),
            path,
            query,
            version: version.to_string(),
            headers,
            body,
        },
        consumed: total_needed,
    }
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
}
