use crate::config::Location;
use crate::fs_safety;
use crate::http::{Request, Response};
use std::ffi::CString;
use std::fs;
use std::os::unix::io::RawFd;
use std::path::Path;
use std::time::{Duration, Instant};

const CGI_TIMEOUT: Duration = Duration::from_secs(5);
const READ_CHUNK: usize = 4096;

/// Everything about the request/connection a CGI script's environment
/// variables need, beyond what's already in the `Request` itself.
pub struct CgiContext<'a> {
    pub server_name: &'a str,
    pub server_port: &'a str,
    pub remote_addr: &'a str,
}

/// Looks up the CGI interpreter configured for a request path's file
/// extension under this location, if any (e.g. "sh" -> "/bin/sh").
pub fn interpreter_for(location: &Location, request_path: &str) -> Option<String> {
    let relative = fs_safety::relative_path(&location.path, request_path);
    let extension = Path::new(relative).extension()?.to_str()?;
    location.cgi.get(extension).cloned()
}

pub fn execute(
    location: &Location,
    interpreter: &str,
    request: &Request,
    request_path: &str,
    ctx: &CgiContext,
) -> Response {
    execute_with_timeout(
        location,
        interpreter,
        request,
        request_path,
        ctx,
        CGI_TIMEOUT,
    )
}

fn execute_with_timeout(
    location: &Location,
    interpreter: &str,
    request: &Request,
    request_path: &str,
    ctx: &CgiContext,
    timeout: Duration,
) -> Response {
    let canonical_root = match fs_safety::canonical_root(&location.root) {
        Ok(root) => root,
        Err(response) => return response,
    };

    let relative = fs_safety::relative_path(&location.path, request_path);
    let script_path = Path::new(&location.root).join(relative);
    let canonical_script = match fs::canonicalize(&script_path) {
        Ok(path) => path,
        Err(_) => return Response::error(404, "Not Found"),
    };
    if !fs_safety::within_root(&canonical_script, &canonical_root) {
        return Response::error(403, "Forbidden");
    }
    if !canonical_script.is_file() {
        return Response::error(404, "Not Found");
    }

    let mut stdin_pipe = [0 as RawFd; 2];
    let mut stdout_pipe = [0 as RawFd; 2];
    unsafe {
        if libc::pipe(stdin_pipe.as_mut_ptr()) != 0 {
            return Response::error(500, "Failed to create CGI stdin pipe");
        }
        if libc::pipe(stdout_pipe.as_mut_ptr()) != 0 {
            libc::close(stdin_pipe[0]);
            libc::close(stdin_pipe[1]);
            return Response::error(500, "Failed to create CGI stdout pipe");
        }
    }

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        unsafe {
            libc::close(stdin_pipe[0]);
            libc::close(stdin_pipe[1]);
            libc::close(stdout_pipe[0]);
            libc::close(stdout_pipe[1]);
        }
        return Response::error(500, "fork() failed");
    }

    if pid == 0 {
        run_child(
            interpreter,
            &canonical_script,
            request,
            request_path,
            ctx,
            stdin_pipe,
            stdout_pipe,
        );
        // run_child only returns on failure to exec.
        unsafe { libc::_exit(127) };
    }

    // Parent: close the ends the child uses, keep our own.
    unsafe {
        libc::close(stdin_pipe[0]);
        libc::close(stdout_pipe[1]);
    }
    let stdin_write_fd = stdin_pipe[1];
    let stdout_read_fd = stdout_pipe[0];
    set_nonblocking(stdin_write_fd);
    set_nonblocking(stdout_read_fd);

    let deadline = Instant::now() + timeout;
    match pump_io(stdin_write_fd, stdout_read_fd, &request.body, deadline) {
        Ok(output) => {
            reap(pid, Duration::from_secs(1));
            parse_cgi_output(&output)
        }
        Err(response) => {
            unsafe { libc::kill(pid, libc::SIGKILL) };
            reap(pid, Duration::from_secs(1));
            response
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_child(
    interpreter: &str,
    script_path: &Path,
    request: &Request,
    request_path: &str,
    ctx: &CgiContext,
    stdin_pipe: [RawFd; 2],
    stdout_pipe: [RawFd; 2],
) {
    unsafe {
        libc::dup2(stdin_pipe[0], 0);
        libc::dup2(stdout_pipe[1], 1);
        libc::close(stdin_pipe[0]);
        libc::close(stdin_pipe[1]);
        libc::close(stdout_pipe[0]);
        libc::close(stdout_pipe[1]);
    }

    set_cgi_env(request, request_path, script_path, ctx);

    let Ok(interpreter_c) = CString::new(interpreter) else {
        return;
    };
    let Ok(script_c) = CString::new(script_path.to_string_lossy().as_bytes()) else {
        return;
    };
    let args = [interpreter_c.as_ptr(), script_c.as_ptr(), std::ptr::null()];
    unsafe {
        libc::execv(interpreter_c.as_ptr(), args.as_ptr());
    }
}

fn set_cgi_env(request: &Request, request_path: &str, script_path: &Path, ctx: &CgiContext) {
    std::env::set_var("GATEWAY_INTERFACE", "CGI/1.1");
    std::env::set_var("SERVER_PROTOCOL", &request.version);
    std::env::set_var("SERVER_SOFTWARE", "localhost/0.1");
    std::env::set_var("SERVER_NAME", ctx.server_name);
    std::env::set_var("SERVER_PORT", ctx.server_port);
    std::env::set_var("REMOTE_ADDR", ctx.remote_addr);
    std::env::set_var("REQUEST_METHOD", request.method.as_str());
    std::env::set_var("SCRIPT_NAME", request_path);
    std::env::set_var("SCRIPT_FILENAME", script_path.to_string_lossy().as_ref());
    std::env::set_var("PATH_INFO", "");
    std::env::set_var("QUERY_STRING", request.query.as_deref().unwrap_or(""));
    std::env::set_var("CONTENT_LENGTH", request.body.len().to_string());
    std::env::set_var("CONTENT_TYPE", request.header("content-type").unwrap_or(""));
}

fn set_nonblocking(fd: RawFd) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL, 0);
        libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
}

/// Concurrently writes `body` to the child's stdin and reads its stdout,
/// so a script that echoes input as it arrives can't deadlock against a
/// full pipe buffer in either direction. Returns the collected stdout once
/// the child closes it (EOF), or an error response on timeout/I/O failure.
fn pump_io(
    stdin_write_fd: RawFd,
    stdout_read_fd: RawFd,
    body: &[u8],
    deadline: Instant,
) -> Result<Vec<u8>, Response> {
    let mut output = Vec::new();
    let mut body_offset = 0usize;
    let mut stdin_open = true;

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(Response::error(504, "CGI script timed out"));
        }

        let mut fds = Vec::with_capacity(2);
        if stdin_open {
            fds.push(libc::pollfd {
                fd: stdin_write_fd,
                events: libc::POLLOUT,
                revents: 0,
            });
        }
        fds.push(libc::pollfd {
            fd: stdout_read_fd,
            events: libc::POLLIN,
            revents: 0,
        });

        let timeout_ms = remaining.as_millis().min(i32::MAX as u128) as i32;
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) };
        if ready < 0 {
            if stdin_open {
                unsafe { libc::close(stdin_write_fd) };
            }
            unsafe { libc::close(stdout_read_fd) };
            return Err(Response::error(500, "CGI I/O poll failed"));
        }

        for pfd in &fds {
            if pfd.fd == stdout_read_fd && pfd.revents != 0 {
                let mut chunk = [0u8; READ_CHUNK];
                let n = unsafe {
                    libc::read(
                        stdout_read_fd,
                        chunk.as_mut_ptr() as *mut libc::c_void,
                        chunk.len(),
                    )
                };
                if n > 0 {
                    output.extend_from_slice(&chunk[..n as usize]);
                } else if n == 0 {
                    if stdin_open {
                        unsafe { libc::close(stdin_write_fd) };
                    }
                    unsafe { libc::close(stdout_read_fd) };
                    return Ok(output);
                }
                // n < 0: spurious wakeup (EAGAIN) or transient error; loop again.
            }

            if stdin_open && pfd.fd == stdin_write_fd && pfd.revents & libc::POLLOUT != 0 {
                if body_offset < body.len() {
                    let chunk_len = (body.len() - body_offset).min(READ_CHUNK);
                    let n = unsafe {
                        libc::write(
                            stdin_write_fd,
                            body[body_offset..].as_ptr() as *const libc::c_void,
                            chunk_len,
                        )
                    };
                    if n > 0 {
                        body_offset += n as usize;
                    }
                }
                if body_offset >= body.len() {
                    unsafe { libc::close(stdin_write_fd) };
                    stdin_open = false;
                }
            }
        }
    }
}

/// Reaps the child, waiting briefly for a natural exit before giving up
/// (the caller is responsible for killing it first if that's warranted).
fn reap(pid: libc::pid_t, patience: Duration) {
    let deadline = Instant::now() + patience;
    loop {
        let mut status = 0;
        let result = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if result != 0 {
            return;
        }
        if Instant::now() >= deadline {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
                libc::waitpid(pid, &mut status, 0);
            }
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Parses a CGI script's stdout into an HTTP response: headers up to the
/// first blank line (a "Status: <code> <reason>" header sets the response
/// status, defaulting to 200), then the body. No trailers, no chunked
/// output from the script itself.
fn parse_cgi_output(output: &[u8]) -> Response {
    let separator = output
        .windows(2)
        .position(|w| w == b"\n\n")
        .map(|i| (i, 2))
        .or_else(|| {
            output
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .map(|i| (i, 4))
        });

    let Some((header_end, separator_len)) = separator else {
        return Response::error(502, "CGI script produced no header/body separator");
    };

    let head = match std::str::from_utf8(&output[..header_end]) {
        Ok(s) => s,
        Err(_) => return Response::error(502, "CGI script headers are not valid UTF-8"),
    };

    let mut status = 200u16;
    let mut reason = "OK".to_string();
    let mut headers = Vec::new();

    for line in head.split('\n') {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Response::error(502, "Malformed CGI header line");
        };
        let name = name.trim();
        let value = value.trim();

        if name.eq_ignore_ascii_case("status") {
            let mut parts = value.splitn(2, ' ');
            match parts.next().and_then(|code| code.parse::<u16>().ok()) {
                Some(code) => status = code,
                None => return Response::error(502, "Malformed CGI Status header"),
            }
            reason = parts.next().unwrap_or("OK").to_string();
        } else {
            headers.push((name.to_string(), value.to_string()));
        }
    }

    let body = output[header_end + separator_len..].to_vec();
    let mut response = Response::new(status, &reason);
    for (name, value) in headers {
        response = response.header(&name, &value);
    }
    response.body(body)
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
        let dir = std::env::temp_dir().join(format!("localhost_cgi_test_{}_{}", name, unique));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn location(root: &Path) -> Location {
        Location {
            path: "/cgi-bin".to_string(),
            root: root.to_string_lossy().to_string(),
            index: None,
            methods: vec!["GET".to_string(), "POST".to_string()],
            autoindex: false,
            cgi: HashMap::new(),
        }
    }

    fn request(method: Method, body: &[u8]) -> Request {
        Request {
            method,
            path: "/cgi-bin/script.sh".to_string(),
            query: None,
            version: "HTTP/1.1".to_string(),
            headers: HashMap::new(),
            body: body.to_vec(),
        }
    }

    fn context<'a>() -> CgiContext<'a> {
        CgiContext {
            server_name: "localhost",
            server_port: "8080",
            remote_addr: "127.0.0.1",
        }
    }

    #[test]
    fn runs_script_and_captures_output() {
        let root = temp_dir("basic");
        fs::write(
            root.join("script.sh"),
            "#!/bin/sh\necho 'Content-Type: text/plain'\necho ''\necho 'Hello CGI'\n",
        )
        .unwrap();
        let location = location(&root);
        let req = request(Method::Get, b"");

        let response = execute(&location, "/bin/sh", &req, "/cgi-bin/script.sh", &context());
        let text = String::from_utf8(response.to_bytes()).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Content-Type: text/plain"));
        assert!(text.ends_with("Hello CGI\n"));
    }

    #[test]
    fn passes_request_body_and_env_vars_to_script() {
        let root = temp_dir("stdin_env");
        fs::write(
            root.join("script.sh"),
            "#!/bin/sh\necho 'Content-Type: text/plain'\necho ''\necho \"Method: $REQUEST_METHOD\"\ncat\n",
        )
        .unwrap();
        let location = location(&root);
        let req = request(Method::Post, b"ping");

        let response = execute(&location, "/bin/sh", &req, "/cgi-bin/script.sh", &context());
        let text = String::from_utf8(response.to_bytes()).unwrap();
        assert!(text.contains("Method: POST"));
        assert!(text.ends_with("ping"));
    }

    #[test]
    fn honors_status_header_from_script() {
        let root = temp_dir("status_header");
        fs::write(
            root.join("script.sh"),
            "#!/bin/sh\necho 'Status: 404 Not Found'\necho 'Content-Type: text/plain'\necho ''\necho 'nope'\n",
        )
        .unwrap();
        let location = location(&root);
        let req = request(Method::Get, b"");

        let response = execute(&location, "/bin/sh", &req, "/cgi-bin/script.sh", &context());
        let text = String::from_utf8(response.to_bytes()).unwrap();
        assert!(text.starts_with("HTTP/1.1 404 Not Found\r\n"));
    }

    #[test]
    fn missing_script_is_404() {
        let root = temp_dir("missing");
        let location = location(&root);
        let req = request(Method::Get, b"");

        let response = execute(&location, "/bin/sh", &req, "/cgi-bin/script.sh", &context());
        let text = String::from_utf8(response.to_bytes()).unwrap();
        assert!(text.starts_with("HTTP/1.1 404 Not Found\r\n"));
    }

    #[test]
    fn traversal_outside_root_is_403() {
        let root = temp_dir("traversal_root");
        fs::create_dir_all(root.join("public")).unwrap();
        fs::write(root.join("secret.sh"), "#!/bin/sh\necho hi\n").unwrap();
        let location = Location {
            root: root.join("public").to_string_lossy().to_string(),
            ..location(&root)
        };
        let mut req = request(Method::Get, b"");
        req.path = "/cgi-bin/../secret.sh".to_string();

        let response = execute(
            &location,
            "/bin/sh",
            &req,
            "/cgi-bin/../secret.sh",
            &context(),
        );
        let text = String::from_utf8(response.to_bytes()).unwrap();
        assert!(text.starts_with("HTTP/1.1 403 Forbidden\r\n"));
    }

    #[test]
    fn malformed_output_is_502() {
        let root = temp_dir("malformed");
        fs::write(
            root.join("script.sh"),
            "#!/bin/sh\nprintf 'no separator here'\n",
        )
        .unwrap();
        let location = location(&root);
        let req = request(Method::Get, b"");

        let response = execute(&location, "/bin/sh", &req, "/cgi-bin/script.sh", &context());
        let text = String::from_utf8(response.to_bytes()).unwrap();
        assert!(text.starts_with("HTTP/1.1 502 Bad Gateway\r\n"));
    }

    #[test]
    fn slow_script_times_out() {
        let root = temp_dir("timeout");
        fs::write(root.join("script.sh"), "#!/bin/sh\nsleep 2\necho ''\n").unwrap();
        let location = location(&root);
        let req = request(Method::Get, b"");

        let response = execute_with_timeout(
            &location,
            "/bin/sh",
            &req,
            "/cgi-bin/script.sh",
            &context(),
            Duration::from_millis(200),
        );
        let text = String::from_utf8(response.to_bytes()).unwrap();
        assert!(text.starts_with("HTTP/1.1 504 Gateway Timeout\r\n"));
    }
}
