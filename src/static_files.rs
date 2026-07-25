use crate::config::Location;
use crate::fs_safety;
use crate::http::Response;
use std::fs;
use std::path::Path;

pub fn serve(location: &Location, request_path: &str) -> Response {
    let canonical_root = match fs_safety::canonical_root(&location.root) {
        Ok(root) => root,
        Err(response) => return response,
    };

    let relative = fs_safety::relative_path(&location.path, request_path);

    let joined = if relative.is_empty() {
        canonical_root.clone()
    } else {
        Path::new(&location.root).join(relative)
    };

    let mut target = match fs::canonicalize(&joined) {
        Ok(target) => target,
        Err(_) => return Response::error(404, "Not Found"),
    };

    if !fs_safety::within_root(&target, &canonical_root) {
        return Response::error(403, "Forbidden");
    }

    if target.is_dir() {
        let index_path = location
            .index
            .as_ref()
            .map(|index_name| target.join(index_name));
        let servable_index = index_path
            .filter(|path| fs_safety::within_root(path, &canonical_root) && path.is_file());

        target = match servable_index {
            Some(index_path) => index_path,
            None if location.autoindex => return render_autoindex(&target, request_path),
            None if location.index.is_some() => return Response::error(404, "Not Found"),
            None => return Response::error(403, "Forbidden"),
        };
    }

    match fs::read(&target) {
        Ok(bytes) => Response::new(200, "OK")
            .header("Content-Type", content_type_for(&target))
            .body(bytes),
        Err(_) => Response::error(404, "Not Found"),
    }
}

/// Renders a simple directory listing when `autoindex` is enabled and
/// there's no servable index file.
fn render_autoindex(dir: &Path, request_path: &str) -> Response {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return Response::error(500, "Failed to read directory"),
    };

    let mut listing: Vec<(String, bool)> = entries
        .flatten()
        .map(|entry| {
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            (entry.file_name().to_string_lossy().into_owned(), is_dir)
        })
        .collect();
    listing.sort();

    let base = if request_path.ends_with('/') {
        request_path.to_string()
    } else {
        format!("{}/", request_path)
    };

    let mut body = format!(
        "<!doctype html>\n<html>\n<head><title>Index of {path}</title></head>\n<body>\n<h1>Index of {path}</h1>\n<ul>\n",
        path = html_escape(request_path)
    );
    if request_path != "/" {
        body.push_str("<li><a href=\"../\">../</a></li>\n");
    }
    for (name, is_dir) in listing {
        let suffix = if is_dir { "/" } else { "" };
        body.push_str(&format!(
            "<li><a href=\"{base}{name}{suffix}\">{name}{suffix}</a></li>\n",
            base = base,
            name = html_escape(&name),
            suffix = suffix
        ));
    }
    body.push_str("</ul>\n</body>\n</html>\n");

    Response::new(200, "OK")
        .header("Content-Type", "text/html")
        .body(body.into_bytes())
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn content_type_for(path: &Path) -> &'static str {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("html") | Some("htm") => "text/html",
        Some("css") => "text/css",
        Some("js") => "application/javascript",
        Some("json") => "application/json",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("txt") => "text/plain",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(name: &str) -> PathBuf {
        let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("localhost_static_test_{}_{}", name, unique));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn location(root: &Path, path: &str, index: Option<&str>) -> Location {
        Location {
            path: path.to_string(),
            root: root.to_string_lossy().to_string(),
            index: index.map(str::to_string),
            methods: vec!["GET".to_string()],
            autoindex: false,
            cgi: std::collections::HashMap::new(),
            client_max_body_size: crate::config::DEFAULT_MAX_BODY_SIZE,
        }
    }

    #[test]
    fn serves_an_existing_file() {
        let root = temp_dir("serves_file");
        fs::write(root.join("about.html"), b"<h1>About</h1>").unwrap();
        let location = location(&root, "/", None);

        let response = serve(&location, "/about.html");
        let bytes = response.to_bytes();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Content-Type: text/html"));
        assert!(text.ends_with("<h1>About</h1>"));
    }

    #[test]
    fn serves_index_for_directory_root() {
        let root = temp_dir("serves_index");
        fs::write(root.join("index.html"), b"home").unwrap();
        let location = location(&root, "/", Some("index.html"));

        let response = serve(&location, "/");
        let text = String::from_utf8(response.to_bytes()).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.ends_with("home"));
    }

    #[test]
    fn missing_file_is_404() {
        let root = temp_dir("missing_file");
        let location = location(&root, "/", None);

        let response = serve(&location, "/nope.html");
        let text = String::from_utf8(response.to_bytes()).unwrap();
        assert!(text.starts_with("HTTP/1.1 404 Not Found\r\n"));
    }

    #[test]
    fn blocks_path_traversal_outside_root() {
        let root = temp_dir("traversal_root");
        fs::create_dir_all(root.join("public")).unwrap();
        fs::write(root.join("secret.txt"), b"top secret").unwrap();
        let location = location(&root.join("public"), "/", None);

        let response = serve(&location, "/../secret.txt");
        let text = String::from_utf8(response.to_bytes()).unwrap();
        assert!(text.starts_with("HTTP/1.1 403 Forbidden\r\n"));
    }

    #[test]
    fn directory_without_index_is_forbidden() {
        let root = temp_dir("no_index");
        let location = location(&root, "/", None);

        let response = serve(&location, "/");
        let text = String::from_utf8(response.to_bytes()).unwrap();
        assert!(text.starts_with("HTTP/1.1 403 Forbidden\r\n"));
    }

    #[test]
    fn autoindex_lists_directory_contents() {
        let root = temp_dir("autoindex");
        fs::write(root.join("b.txt"), b"").unwrap();
        fs::write(root.join("a.txt"), b"").unwrap();
        fs::create_dir_all(root.join("sub")).unwrap();
        let mut loc = location(&root, "/files", None);
        loc.autoindex = true;

        let response = serve(&loc, "/files");
        let text = String::from_utf8(response.to_bytes()).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Content-Type: text/html"));
        assert!(text.contains("href=\"/files/a.txt\""));
        assert!(text.contains("href=\"/files/b.txt\""));
        assert!(text.contains("href=\"/files/sub/\""));
    }

    #[test]
    fn autoindex_used_when_configured_index_file_is_missing() {
        let root = temp_dir("autoindex_missing_index");
        fs::write(root.join("readme.txt"), b"").unwrap();
        let mut loc = location(&root, "/", Some("index.html"));
        loc.autoindex = true;

        let response = serve(&loc, "/");
        let text = String::from_utf8(response.to_bytes()).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("readme.txt"));
    }

    #[test]
    fn autoindex_escapes_html_in_filenames() {
        let root = temp_dir("autoindex_escape");
        fs::write(root.join("<script>.txt"), b"").unwrap();
        let mut loc = location(&root, "/", None);
        loc.autoindex = true;

        let response = serve(&loc, "/");
        let text = String::from_utf8(response.to_bytes()).unwrap();
        assert!(!text.contains("<script>.txt\""));
        assert!(text.contains("&lt;script&gt;.txt"));
    }
}
