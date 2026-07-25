use crate::config::Location;
use crate::fs_safety;
use crate::http::{Request, Response};
use crate::multipart;
use std::fs;
use std::path::Path;

pub fn create(location: &Location, request: &Request) -> Response {
    match request.header("content-type") {
        Some(content_type) if content_type.starts_with("multipart/form-data") => {
            create_from_multipart(location, content_type, &request.body)
        }
        _ => create_from_raw_body(location, &request.path, &request.body),
    }
}

/// Legacy path for plain (non-multipart) POST bodies: the request path
/// itself names the file to write under the location root, e.g.
/// `POST /upload/note.txt` with a raw body writes `note.txt` verbatim.
fn create_from_raw_body(location: &Location, request_path: &str, body: &[u8]) -> Response {
    let canonical_root = match fs_safety::canonical_root(&location.root) {
        Ok(root) => root,
        Err(response) => return response,
    };

    let relative = fs_safety::relative_path(&location.path, request_path);
    if relative.is_empty() {
        return Response::error(400, "POST target must include a file name");
    }

    let target = Path::new(&location.root).join(relative);
    let parent = match target.parent() {
        Some(parent) => parent,
        None => return Response::error(400, "Invalid target path"),
    };
    let canonical_parent = match fs::canonicalize(parent) {
        Ok(parent) => parent,
        Err(_) => return Response::error(404, "Parent directory does not exist"),
    };
    if !fs_safety::within_root(&canonical_parent, &canonical_root) {
        return Response::error(403, "Forbidden");
    }

    let file_name = match target.file_name() {
        Some(name) => name,
        None => return Response::error(400, "Invalid target path"),
    };

    match fs::write(canonical_parent.join(file_name), body) {
        Ok(()) => Response::new(201, "Created"),
        Err(_) => Response::error(500, "Failed to write file"),
    }
}

/// Real browser-style file upload: the filename comes from the part's
/// `Content-Disposition` header, not the URL, and is written directly into
/// the location root (any directory components in the submitted filename
/// are stripped, so it can't be used for traversal).
fn create_from_multipart(location: &Location, content_type: &str, body: &[u8]) -> Response {
    let canonical_root = match fs_safety::canonical_root(&location.root) {
        Ok(root) => root,
        Err(response) => return response,
    };

    let parts = match multipart::parse(content_type, body) {
        Ok(parts) => parts,
        Err(message) => {
            return Response::error(400, &format!("Malformed multipart body: {}", message))
        }
    };

    let Some(file_part) = parts.iter().find(|part| {
        part.filename
            .as_deref()
            .is_some_and(|name| !name.is_empty())
    }) else {
        return Response::error(400, "Multipart body did not contain a file part");
    };

    let raw_filename = file_part.filename.as_deref().unwrap_or("upload");
    let safe_name = Path::new(raw_filename)
        .file_name()
        .map(|name| name.to_string_lossy().to_string());
    let Some(safe_name) = safe_name.filter(|name| !name.is_empty()) else {
        return Response::error(400, "Invalid upload filename");
    };

    match fs::write(canonical_root.join(&safe_name), &file_part.body) {
        Ok(()) => Response::new(201, "Created"),
        Err(_) => Response::error(500, "Failed to write uploaded file"),
    }
}

pub fn delete(location: &Location, request_path: &str) -> Response {
    let canonical_root = match fs_safety::canonical_root(&location.root) {
        Ok(root) => root,
        Err(response) => return response,
    };

    let relative = fs_safety::relative_path(&location.path, request_path);
    if relative.is_empty() {
        return Response::error(400, "DELETE target must include a file name");
    }

    let target = Path::new(&location.root).join(relative);
    let canonical_target = match fs::canonicalize(&target) {
        Ok(target) => target,
        Err(_) => return Response::error(404, "Not Found"),
    };

    if !fs_safety::within_root(&canonical_target, &canonical_root) {
        return Response::error(403, "Forbidden");
    }

    if canonical_target.is_dir() {
        return Response::error(403, "Forbidden");
    }

    match fs::remove_file(&canonical_target) {
        Ok(()) => Response::new(204, "No Content"),
        Err(_) => Response::error(500, "Failed to delete file"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::Method;
    use std::collections::HashMap;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(name: &str) -> PathBuf {
        let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("localhost_file_ops_test_{}_{}", name, unique));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn location(root: &Path, path: &str) -> Location {
        Location {
            path: path.to_string(),
            root: root.to_string_lossy().to_string(),
            index: None,
            methods: vec!["GET".to_string(), "POST".to_string(), "DELETE".to_string()],
            autoindex: false,
            cgi: HashMap::new(),
            client_max_body_size: crate::config::DEFAULT_MAX_BODY_SIZE,
        }
    }

    fn post_request(path: &str, headers: HashMap<String, String>, body: &[u8]) -> Request {
        Request {
            method: Method::Post,
            path: path.to_string(),
            query: None,
            version: "HTTP/1.1".to_string(),
            headers,
            body: body.to_vec(),
        }
    }

    #[test]
    fn creates_a_new_file() {
        let root = temp_dir("creates_new_file");
        let location = location(&root, "/upload");
        let request = post_request("/upload/note.txt", HashMap::new(), b"hello");

        let response = create(&location, &request);
        let text = String::from_utf8(response.to_bytes()).unwrap();
        assert!(text.starts_with("HTTP/1.1 201 Created\r\n"));
        assert_eq!(fs::read(root.join("note.txt")).unwrap(), b"hello");
    }

    #[test]
    fn create_blocks_traversal_outside_root() {
        let root = temp_dir("create_traversal_root");
        fs::create_dir_all(root.join("public")).unwrap();
        let location = location(&root.join("public"), "/upload");
        let request = post_request("/upload/../evil.txt", HashMap::new(), b"pwned");

        let response = create(&location, &request);
        let text = String::from_utf8(response.to_bytes()).unwrap();
        assert!(text.starts_with("HTTP/1.1 403 Forbidden\r\n"));
        assert!(!root.join("evil.txt").exists());
    }

    #[test]
    fn create_without_file_name_is_400() {
        let root = temp_dir("create_no_name");
        let location = location(&root, "/upload");
        let request = post_request("/upload", HashMap::new(), b"hello");

        let response = create(&location, &request);
        let text = String::from_utf8(response.to_bytes()).unwrap();
        assert!(text.starts_with("HTTP/1.1 400 Bad Request\r\n"));
    }

    #[test]
    fn creates_file_from_multipart_upload() {
        let root = temp_dir("multipart_upload");
        let location = location(&root, "/upload");
        let body = concat!(
            "--BOUNDARY\r\n",
            "Content-Disposition: form-data; name=\"file\"; filename=\"pic.png\"\r\n",
            "Content-Type: image/png\r\n",
            "\r\n",
            "binarydata\r\n",
            "--BOUNDARY--\r\n",
        );
        let mut headers = HashMap::new();
        headers.insert(
            "content-type".to_string(),
            "multipart/form-data; boundary=BOUNDARY".to_string(),
        );
        let request = post_request("/upload", headers, body.as_bytes());

        let response = create(&location, &request);
        let text = String::from_utf8(response.to_bytes()).unwrap();
        assert!(text.starts_with("HTTP/1.1 201 Created\r\n"));
        assert_eq!(fs::read(root.join("pic.png")).unwrap(), b"binarydata");
    }

    #[test]
    fn multipart_upload_sanitizes_filename_traversal() {
        let root = temp_dir("multipart_traversal");
        let location = location(&root, "/upload");
        let body = concat!(
            "--BOUNDARY\r\n",
            "Content-Disposition: form-data; name=\"file\"; filename=\"../../evil.txt\"\r\n",
            "\r\n",
            "pwned\r\n",
            "--BOUNDARY--\r\n",
        );
        let mut headers = HashMap::new();
        headers.insert(
            "content-type".to_string(),
            "multipart/form-data; boundary=BOUNDARY".to_string(),
        );
        let request = post_request("/upload", headers, body.as_bytes());

        let response = create(&location, &request);
        let text = String::from_utf8(response.to_bytes()).unwrap();
        assert!(text.starts_with("HTTP/1.1 201 Created\r\n"));
        assert_eq!(fs::read(root.join("evil.txt")).unwrap(), b"pwned");
        assert!(!root.parent().unwrap().join("evil.txt").exists());
    }

    #[test]
    fn deletes_an_existing_file() {
        let root = temp_dir("deletes_file");
        fs::write(root.join("note.txt"), b"hello").unwrap();
        let location = location(&root, "/upload");

        let response = delete(&location, "/upload/note.txt");
        let text = String::from_utf8(response.to_bytes()).unwrap();
        assert!(text.starts_with("HTTP/1.1 204 No Content\r\n"));
        assert!(!root.join("note.txt").exists());
    }

    #[test]
    fn delete_missing_file_is_404() {
        let root = temp_dir("delete_missing");
        let location = location(&root, "/upload");

        let response = delete(&location, "/upload/nope.txt");
        let text = String::from_utf8(response.to_bytes()).unwrap();
        assert!(text.starts_with("HTTP/1.1 404 Not Found\r\n"));
    }

    #[test]
    fn delete_blocks_traversal_outside_root() {
        let root = temp_dir("delete_traversal_root");
        fs::create_dir_all(root.join("public")).unwrap();
        fs::write(root.join("secret.txt"), b"top secret").unwrap();
        let location = location(&root.join("public"), "/upload");

        let response = delete(&location, "/upload/../secret.txt");
        let text = String::from_utf8(response.to_bytes()).unwrap();
        assert!(text.starts_with("HTTP/1.1 403 Forbidden\r\n"));
        assert!(root.join("secret.txt").exists());
    }

    #[test]
    fn delete_refuses_to_remove_a_directory() {
        let root = temp_dir("delete_directory");
        fs::create_dir_all(root.join("subdir")).unwrap();
        let location = location(&root, "/upload");

        let response = delete(&location, "/upload/subdir");
        let text = String::from_utf8(response.to_bytes()).unwrap();
        assert!(text.starts_with("HTTP/1.1 403 Forbidden\r\n"));
        assert!(root.join("subdir").exists());
    }
}
