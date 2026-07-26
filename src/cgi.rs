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

/// A running (forked, exec'd) CGI script and the state needed to pump its
/// stdin/stdout non-blockingly across multiple event-loop wakeups.
pub struct CgiProcess {
    pid: libc::pid_t,
    stdin_fd: Option<RawFd>,
    stdout_fd: RawFd,
    body: Vec<u8>,
    body_offset: usize,
    output: Vec<u8>,
    deadline: Instant,
}

impl CgiProcess {
    pub fn pid(&self) -> libc::pid_t {
        self.pid
    }

    pub fn stdout_fd(&self) -> RawFd {
        self.stdout_fd
    }

    pub fn stdin_fd(&self) -> Option<RawFd> {
        self.stdin_fd
    }
}

pub enum StartOutcome {
    Started(CgiProcess),
    Failed(Response),
}

enum StepOutcome {
    Continue,
    Done(Response),
}

/// Result of one `advance()` step: which pipe fds were closed during this
/// call (if any — closing happens either because the body finished
/// writing, or as a side effect of stdout hitting EOF), and the finished
/// response, if the process is done.
pub struct AdvanceResult {
    pub closed_fds: Vec<RawFd>,
    pub done: Option<Response>,
}

/// Looks up the CGI interpreter configured for a request path's file
/// extension under this location, if any (e.g. "sh" -> "/bin/sh").
pub fn interpreter_for(location: &Location, request_path: &str) -> Option<String> {
    let relative = fs_safety::relative_path(&location.path, request_path);
    let extension = Path::new(relative).extension()?.to_str()?;
    location.cgi.get(extension).cloned()
}

pub fn start(
    location: &Location,
    interpreter: &str,
    request: &Request,
    request_path: &str,
    ctx: &CgiContext,
) -> StartOutcome {
    start_with_timeout(
        location,
        interpreter,
        request,
        request_path,
        ctx,
        CGI_TIMEOUT,
    )
}

fn start_with_timeout(
    location: &Location,
    interpreter: &str,
    request: &Request,
    request_path: &str,
    ctx: &CgiContext,
    timeout: Duration,
) -> StartOutcome {
    let canonical_root = match fs_safety::canonical_root(&location.root) {
        Ok(root) => root,
        Err(response) => return StartOutcome::Failed(response),
    };

    let relative = fs_safety::relative_path(&location.path, request_path);
    let script_path = Path::new(&location.root).join(relative);
    let canonical_script = match fs::canonicalize(&script_path) {
        Ok(path) => path,
        Err(_) => return StartOutcome::Failed(Response::error(404, "Not Found")),
    };
    if !fs_safety::within_root(&canonical_script, &canonical_root) {
        return StartOutcome::Failed(Response::error(403, "Forbidden"));
    }
    if !canonical_script.is_file() {
        return StartOutcome::Failed(Response::error(404, "Not Found"));
    }

    let mut stdin_pipe = [0 as RawFd; 2];
    let mut stdout_pipe = [0 as RawFd; 2];
    unsafe {
        if libc::pipe(stdin_pipe.as_mut_ptr()) != 0 {
            return StartOutcome::Failed(Response::error(500, "Failed to create CGI stdin pipe"));
        }
        if libc::pipe(stdout_pipe.as_mut_ptr()) != 0 {
            libc::close(stdin_pipe[0]);
            libc::close(stdin_pipe[1]);
            return StartOutcome::Failed(Response::error(500, "Failed to create CGI stdout pipe"));
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
        return StartOutcome::Failed(Response::error(500, "fork() failed"));
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

    // An empty body means there's nothing to write: close stdin immediately
    // so the child sees EOF right away instead of hanging waiting for input.
    let stdin_fd = if request.body.is_empty() {
        unsafe { libc::close(stdin_write_fd) };
        None
    } else {
        Some(stdin_write_fd)
    };

    StartOutcome::Started(CgiProcess {
        pid,
        stdin_fd,
        stdout_fd: stdout_read_fd,
        body: request.body.clone(),
        body_offset: 0,
        output: Vec::new(),
        deadline: Instant::now() + timeout,
    })
}

/// Advances a running CGI process by one step in response to `fd` being
/// readable/writable. Call this whenever epoll reports activity on either
/// `process.stdin_fd()` or `process.stdout_fd()`.
pub fn advance(
    process: &mut CgiProcess,
    fd: RawFd,
    readable: bool,
    writable: bool,
) -> AdvanceResult {
    let mut closed_fds = Vec::new();

    if process.stdin_fd == Some(fd) && writable {
        let prior_stdin = process.stdin_fd;
        write_stdin_chunk(process);
        if prior_stdin.is_some() && process.stdin_fd.is_none() {
            closed_fds.push(fd);
        }
    }

    let mut done = None;
    if fd == process.stdout_fd && readable {
        let prior_stdin = process.stdin_fd;
        let stdout_fd = process.stdout_fd;
        if let StepOutcome::Done(response) = read_stdout_chunk(process) {
            closed_fds.push(stdout_fd);
            // read_stdout_chunk force-closes stdin too if it was still open;
            // report that closure if we haven't already (the write-side
            // branch above only fires when `fd == stdin_fd`, so it can't
            // have already recorded this for a different `fd`).
            if let Some(stdin_fd) = prior_stdin {
                if !closed_fds.contains(&stdin_fd) {
                    closed_fds.push(stdin_fd);
                }
            }
            done = Some(response);
        }
    }

    AdvanceResult { closed_fds, done }
}

fn write_stdin_chunk(process: &mut CgiProcess) {
    let Some(stdin_fd) = process.stdin_fd else {
        return;
    };

    if process.body_offset < process.body.len() {
        let chunk_len = (process.body.len() - process.body_offset).min(READ_CHUNK);
        let n = unsafe {
            libc::write(
                stdin_fd,
                process.body[process.body_offset..].as_ptr() as *const libc::c_void,
                chunk_len,
            )
        };
        if n > 0 {
            process.body_offset += n as usize;
        }
        // n <= 0: spurious wakeup (EAGAIN) or transient error; try again next event.
    }

    if process.body_offset >= process.body.len() {
        unsafe { libc::close(stdin_fd) };
        process.stdin_fd = None;
    }
}

fn read_stdout_chunk(process: &mut CgiProcess) -> StepOutcome {
    let mut chunk = [0u8; READ_CHUNK];
    let n = unsafe {
        libc::read(
            process.stdout_fd,
            chunk.as_mut_ptr() as *mut libc::c_void,
            chunk.len(),
        )
    };
    if n > 0 {
        process.output.extend_from_slice(&chunk[..n as usize]);
        StepOutcome::Continue
    } else if n == 0 {
        if let Some(stdin_fd) = process.stdin_fd.take() {
            unsafe { libc::close(stdin_fd) };
        }
        unsafe { libc::close(process.stdout_fd) };
        StepOutcome::Done(parse_cgi_output(&process.output))
    } else {
        // n < 0: spurious wakeup (EAGAIN) or transient error; try again next event.
        StepOutcome::Continue
    }
}

pub fn is_expired(process: &CgiProcess, now: Instant) -> bool {
    now >= process.deadline
}

/// Sends SIGKILL to a CGI process (e.g. on timeout). Does not reap it —
/// the caller is expected to hand the pid to a non-blocking reap sweep.
pub fn kill(process: &CgiProcess) {
    unsafe { libc::kill(process.pid, libc::SIGKILL) };
}

/// Closes a running process's pipe fds without waiting for it to finish;
/// used when abandoning a CGI process on timeout, before killing it.
pub fn close_pipes(process: &mut CgiProcess) {
    if let Some(stdin_fd) = process.stdin_fd.take() {
        unsafe { libc::close(stdin_fd) };
    }
    unsafe { libc::close(process.stdout_fd) };
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
            client_max_body_size: crate::config::DEFAULT_MAX_BODY_SIZE,
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

    /// Drives a CgiProcess to completion using a real poll() loop, mirroring
    /// what connection.rs's event-driven dispatch does one step at a time.
    /// Test-only: production code never blocks like this.
    fn run_to_completion(mut process: CgiProcess, timeout: Duration) -> Response {
        let deadline = Instant::now() + timeout;
        loop {
            if Instant::now() >= deadline {
                kill(&process);
                close_pipes(&mut process);
                return Response::error(504, "CGI script timed out");
            }

            let mut fds = Vec::with_capacity(2);
            if let Some(stdin_fd) = process.stdin_fd {
                fds.push(libc::pollfd {
                    fd: stdin_fd,
                    events: libc::POLLOUT,
                    revents: 0,
                });
            }
            fds.push(libc::pollfd {
                fd: process.stdout_fd,
                events: libc::POLLIN,
                revents: 0,
            });

            let remaining_ms = deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .min(i32::MAX as u128) as i32;
            let ready =
                unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, remaining_ms) };
            if ready < 0 {
                close_pipes(&mut process);
                return Response::error(500, "CGI I/O poll failed");
            }

            for pfd in &fds {
                // Any nonzero revents (not just POLLIN/POLLOUT specifically)
                // means "try the syscall": a closed peer reports POLLHUP,
                // sometimes without POLLIN also set, and read() is what
                // correctly turns that into an EOF/Done detection.
                let readable = pfd.fd == process.stdout_fd && pfd.revents != 0;
                let writable = Some(pfd.fd) == process.stdin_fd && pfd.revents != 0;
                if readable || writable {
                    if let Some(response) = advance(&mut process, pfd.fd, readable, writable).done {
                        return response;
                    }
                }
            }
        }
    }

    fn execute(
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
        match start_with_timeout(location, interpreter, request, request_path, ctx, timeout) {
            StartOutcome::Failed(response) => response,
            StartOutcome::Started(process) => run_to_completion(process, timeout),
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

    #[test]
    fn is_expired_reports_past_deadline() {
        let root = temp_dir("expiry_check");
        fs::write(root.join("script.sh"), "#!/bin/sh\necho ''\n").unwrap();
        let location = location(&root);
        let req = request(Method::Get, b"");

        match start_with_timeout(
            &location,
            "/bin/sh",
            &req,
            "/cgi-bin/script.sh",
            &context(),
            Duration::from_millis(0),
        ) {
            StartOutcome::Started(mut process) => {
                std::thread::sleep(Duration::from_millis(5));
                assert!(is_expired(&process, Instant::now()));
                kill(&process);
                close_pipes(&mut process);
                unsafe {
                    let mut status = 0;
                    libc::waitpid(process.pid, &mut status, 0);
                }
            }
            StartOutcome::Failed(response) => panic!(
                "expected process to start, got: {}",
                String::from_utf8_lossy(&response.to_bytes())
            ),
        }
    }
}
