/// One `multipart/form-data` body part: a form field (`filename` is `None`)
/// or an uploaded file (`filename` is `Some`).
#[derive(Debug)]
pub struct Part {
    #[allow(dead_code)] // field names aren't used yet; only file parts matter today
    pub name: Option<String>,
    pub filename: Option<String>,
    #[allow(dead_code)] // not surfaced anywhere yet, but part of a complete parse
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

/// Parses a `multipart/form-data` body per RFC 7578, given the raw
/// `Content-Type` header value (to extract the boundary) and the body
/// bytes. No nested multipart parts.
pub fn parse(content_type: &str, body: &[u8]) -> Result<Vec<Part>, String> {
    let boundary = extract_field(content_type, "boundary")
        .ok_or_else(|| "missing multipart boundary".to_string())?;
    let delimiter = format!("--{}", boundary).into_bytes();

    let mut parts = Vec::new();
    let mut pos = match find(body, &delimiter, 0) {
        Some(idx) => idx + delimiter.len(),
        None => return Err("no multipart boundary found in body".to_string()),
    };

    loop {
        if body[pos..].starts_with(b"--") {
            break;
        }
        if !body[pos..].starts_with(b"\r\n") {
            return Err("malformed multipart part: expected CRLF after boundary".to_string());
        }
        pos += 2;

        let header_end = find(body, b"\r\n\r\n", pos)
            .ok_or_else(|| "malformed multipart part: missing header terminator".to_string())?;
        let header_text = std::str::from_utf8(&body[pos..header_end])
            .map_err(|_| "multipart headers are not valid UTF-8".to_string())?;

        let mut name = None;
        let mut filename = None;
        let mut content_type_header = None;
        for line in header_text.split("\r\n") {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            if key.trim().eq_ignore_ascii_case("content-disposition") {
                name = extract_field(value, "name");
                filename = extract_field(value, "filename");
            } else if key.trim().eq_ignore_ascii_case("content-type") {
                content_type_header = Some(value.to_string());
            }
        }

        let body_start = header_end + 4;
        let mut next_delimiter = Vec::with_capacity(delimiter.len() + 2);
        next_delimiter.extend_from_slice(b"\r\n");
        next_delimiter.extend_from_slice(&delimiter);
        let next_delim_pos = find(body, &next_delimiter, body_start)
            .ok_or_else(|| "malformed multipart part: missing terminating boundary".to_string())?;

        parts.push(Part {
            name,
            filename,
            content_type: content_type_header,
            body: body[body_start..next_delim_pos].to_vec(),
        });

        pos = next_delim_pos + next_delimiter.len();
    }

    Ok(parts)
}

/// Extracts a `key="value"` (or unquoted `key=value`) field from a
/// semicolon-separated header value, e.g. `form-data; name="file"` or
/// `multipart/form-data; boundary=XYZ`.
fn extract_field(header_value: &str, field: &str) -> Option<String> {
    let prefix = format!("{}=", field);
    header_value.split(';').find_map(|segment| {
        segment
            .trim()
            .strip_prefix(prefix.as_str())
            .map(|value| value.trim_matches('"').to_string())
    })
}

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from > haystack.len() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|i| i + from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_body(boundary: &str) -> Vec<u8> {
        format!(
            "--{b}\r\n\
             Content-Disposition: form-data; name=\"note\"\r\n\
             \r\n\
             hello\r\n\
             --{b}\r\n\
             Content-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\n\
             Content-Type: text/plain\r\n\
             \r\n\
             file contents\r\n\
             --{b}--\r\n",
            b = boundary
        )
        .into_bytes()
    }

    #[test]
    fn parses_field_and_file_parts() {
        let body = sample_body("XYZ");
        let parts = parse("multipart/form-data; boundary=XYZ", &body).unwrap();

        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].name.as_deref(), Some("note"));
        assert_eq!(parts[0].filename, None);
        assert_eq!(parts[0].body, b"hello");

        assert_eq!(parts[1].filename.as_deref(), Some("a.txt"));
        assert_eq!(parts[1].content_type.as_deref(), Some("text/plain"));
        assert_eq!(parts[1].body, b"file contents");
    }

    #[test]
    fn rejects_missing_boundary() {
        let err = parse("multipart/form-data", b"whatever").unwrap_err();
        assert!(err.contains("boundary"));
    }

    #[test]
    fn rejects_unterminated_part() {
        let body = b"--XYZ\r\nContent-Disposition: form-data; name=\"x\"\r\n\r\nvalue".to_vec();
        let err = parse("multipart/form-data; boundary=XYZ", &body).unwrap_err();
        assert!(err.contains("terminating boundary"));
    }

    #[test]
    fn handles_binary_content_containing_crlf() {
        let boundary = "XYZ";
        let mut body = Vec::new();
        body.extend_from_slice(b"--XYZ\r\n");
        body.extend_from_slice(
            b"Content-Disposition: form-data; name=\"file\"; filename=\"bin.dat\"\r\n\r\n",
        );
        body.extend_from_slice(&[0u8, 1, 2, b'\r', b'\n', 3, 4]);
        body.extend_from_slice(b"\r\n--XYZ--\r\n");

        let parts = parse(
            &format!("multipart/form-data; boundary={}", boundary),
            &body,
        )
        .unwrap();
        assert_eq!(parts[0].body, vec![0u8, 1, 2, b'\r', b'\n', 3, 4]);
    }
}
