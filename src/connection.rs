use crate::cgi::{self, CgiProcess};
use crate::config::{Location, ServerConfig};
use crate::http::{self, Method, ParseOutcome, Request, Response};
use crate::{file_ops, router, static_files};
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::io::{AsRawFd, RawFd};
use std::time::{Duration, Instant};

/// What the caller (main.rs's event loop) needs to do to its own fd
/// bookkeeping after a `Connection` method call. Epoll registration
/// changes for the *client* socket happen inside `Connection` itself
/// (it holds `epoll_fd`); these variants only cover the extra pipe fds
/// that come and go with a CGI process, which the caller tracks in its
/// own fd -> connection routing table.
pub enum Outcome {
    Continue,
    RegisterCgi {
        stdout_fd: RawFd,
        stdin_fd: Option<RawFd>,
    },
    /// One CGI pipe fd (stdin) has just been closed mid-flight — the
    /// process is still running — and must be dropped from the caller's
    /// routing table immediately, not merely queued for later, since the
    /// OS is free to reuse that fd number as soon as the next `accept()`.
    UnregisterCgiFds(Vec<RawFd>),
    /// The CGI process has exited (stdout hit EOF): drop `closed_fds` from
    /// the routing table and queue `pid` for non-blocking reaping so it
    /// never becomes a zombie.
    CgiFinished {
        closed_fds: Vec<RawFd>,
        pid: libc::pid_t,
    },
    Close,
}

enum ConnState {
    ReadingRequest,
    Writing {
        buf: Vec<u8>,
        pos: usize,
        keep_alive: bool,
    },
    RunningCgi {
        cgi: CgiProcess,
        keep_alive: bool,
        meta: RequestMeta,
    },
}

/// The slice of a `Request` the access log needs, captured at dispatch time
/// so it's still around once a CGI response resolves — by then the original
/// `Request` (and its borrowed header strings) is long gone.
#[derive(Clone)]
struct RequestMeta {
    method: String,
    path: String,
    version: String,
    referer: Option<String>,
    user_agent: Option<String>,
}

impl RequestMeta {
    fn capture(request: &Request) -> Self {
        RequestMeta {
            method: request.method.as_str().to_string(),
            path: request.path.clone(),
            version: request.version.clone(),
            referer: request.header("referer").map(str::to_string),
            user_agent: request.header("user-agent").map(str::to_string),
        }
    }

    fn log(&self, peer: &SocketAddr, response: &Response) {
        crate::log::access(
            peer,
            &self.method,
            &self.path,
            &self.version,
            self.referer.as_deref(),
            self.user_agent.as_deref(),
            response.status(),
            response.body_len(),
        );
    }
}

enum RouteResult {
    Response(Response),
    Cgi(CgiProcess),
}

/// Methods this location actually accepts, including the implicit HEAD
/// that comes bundled with GET — shared by the `Allow` header on both a
/// 405 rejection and an OPTIONS response, so the two can't disagree.
fn allow_header_value(location: &Location) -> String {
    let mut methods: Vec<&str> = location.methods.iter().map(String::as_str).collect();
    if methods.contains(&"GET") && !methods.contains(&"HEAD") {
        methods.push("HEAD");
    }
    if !methods.contains(&"OPTIONS") {
        methods.push("OPTIONS");
    }
    methods.join(", ")
}

pub struct Connection {
    stream: TcpStream,
    fd: RawFd,
    epoll_fd: RawFd,
    peer_addr: SocketAddr,
    group_id: usize,
    read_buf: Vec<u8>,
    state: ConnState,
    last_activity: Instant,
}

impl Connection {
    pub fn accept(
        stream: TcpStream,
        peer_addr: SocketAddr,
        group_id: usize,
        epoll_fd: RawFd,
    ) -> io::Result<Connection> {
        stream.set_nonblocking(true)?;
        let fd = stream.as_raw_fd();
        let conn = Connection {
            stream,
            fd,
            epoll_fd,
            peer_addr,
            group_id,
            read_buf: Vec::new(),
            state: ConnState::ReadingRequest,
            last_activity: Instant::now(),
        };
        conn.epoll_add(fd, libc::EPOLLIN as u32);
        Ok(conn)
    }

    pub fn fd(&self) -> RawFd {
        self.fd
    }

    fn epoll_add(&self, fd: RawFd, events: u32) {
        let mut event = libc::epoll_event {
            events,
            u64: fd as u64,
        };
        unsafe {
            libc::epoll_ctl(self.epoll_fd, libc::EPOLL_CTL_ADD, fd, &mut event);
        }
    }

    fn epoll_mod(&self, fd: RawFd, events: u32) {
        let mut event = libc::epoll_event {
            events,
            u64: fd as u64,
        };
        unsafe {
            libc::epoll_ctl(self.epoll_fd, libc::EPOLL_CTL_MOD, fd, &mut event);
        }
    }

    /// Called when epoll reports the client socket is readable.
    pub fn on_readable(&mut self, groups: &[Vec<ServerConfig>]) -> Outcome {
        let mut chunk = [0u8; 4096];
        match self.stream.read(&mut chunk) {
            Ok(0) => return Outcome::Close,
            Ok(n) => {
                self.read_buf.extend_from_slice(&chunk[..n]);
                self.last_activity = Instant::now();
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Outcome::Continue,
            Err(_) => return Outcome::Close,
        }

        if matches!(self.state, ConnState::ReadingRequest) {
            self.drive(groups)
        } else {
            // Mid-response or mid-CGI: buffer any extra bytes (e.g. a
            // pipelined next request) without acting on them yet.
            Outcome::Continue
        }
    }

    /// Called when epoll reports the client socket is writable.
    pub fn on_writable(&mut self, groups: &[Vec<ServerConfig>]) -> Outcome {
        if matches!(self.state, ConnState::Writing { .. }) {
            self.drive(groups)
        } else {
            Outcome::Continue
        }
    }

    /// Drives request parsing -> routing -> response writing -> (if
    /// keep-alive) back to parsing, until something needs to wait for
    /// more I/O, a CGI process starts, or the connection should close.
    fn drive(&mut self, groups: &[Vec<ServerConfig>]) -> Outcome {
        loop {
            match &self.state {
                ConnState::ReadingRequest => match http::request::parse(&self.read_buf) {
                    ParseOutcome::Incomplete => return Outcome::Continue,
                    ParseOutcome::Invalid { status, message } => {
                        let response = Response::error(status, &message);
                        crate::log::access(
                            &self.peer_addr,
                            "-",
                            "-",
                            "-",
                            None,
                            None,
                            response.status(),
                            response.body_len(),
                        );
                        self.read_buf.clear();
                        self.enter_writing(response, false);
                    }
                    ParseOutcome::Complete { request, consumed } => {
                        self.read_buf.drain(..consumed);
                        let keep_alive = request.keep_alive();
                        match self.route(&request, groups) {
                            RouteResult::Response(response) => {
                                RequestMeta::capture(&request).log(&self.peer_addr, &response);
                                self.enter_writing(response, keep_alive);
                            }
                            RouteResult::Cgi(process) => {
                                let meta = RequestMeta::capture(&request);
                                let stdout_fd = process.stdout_fd();
                                let stdin_fd = process.stdin_fd();
                                self.epoll_add(stdout_fd, libc::EPOLLIN as u32);
                                if let Some(stdin_fd) = stdin_fd {
                                    self.epoll_add(stdin_fd, libc::EPOLLOUT as u32);
                                }
                                self.state = ConnState::RunningCgi {
                                    cgi: process,
                                    keep_alive,
                                    meta,
                                };
                                return Outcome::RegisterCgi {
                                    stdout_fd,
                                    stdin_fd,
                                };
                            }
                        }
                    }
                },
                ConnState::Writing { .. } => match self.try_write() {
                    WriteResult::Pending => return Outcome::Continue,
                    WriteResult::Error => return Outcome::Close,
                    WriteResult::Done { keep_alive } => {
                        if !keep_alive {
                            return Outcome::Close;
                        }
                        self.state = ConnState::ReadingRequest;
                        self.epoll_mod(self.fd, libc::EPOLLIN as u32);
                        // Loop back: read_buf may already hold a pipelined
                        // next request.
                    }
                },
                ConnState::RunningCgi { .. } => return Outcome::Continue,
            }
        }
    }

    fn enter_writing(&mut self, response: Response, keep_alive: bool) {
        // Tell the client what we've already decided, rather than leaving
        // it to guess from how the socket behaves afterward — required for
        // HTTP/1.0 keep-alive (off by default; the client only reuses the
        // connection if we opt in here) and good practice for HTTP/1.1
        // close.
        let connection_value = if keep_alive { "keep-alive" } else { "close" };
        let response = response.header("Connection", connection_value);
        let buf = response.to_bytes();
        self.state = ConnState::Writing {
            buf,
            pos: 0,
            keep_alive,
        };
        self.epoll_mod(self.fd, libc::EPOLLOUT as u32);
    }

    fn try_write(&mut self) -> WriteResult {
        let ConnState::Writing {
            buf,
            pos,
            keep_alive,
        } = &mut self.state
        else {
            return WriteResult::Error;
        };
        match self.stream.write(&buf[*pos..]) {
            Ok(0) => WriteResult::Error,
            Ok(n) => {
                *pos += n;
                self.last_activity = Instant::now();
                if *pos >= buf.len() {
                    WriteResult::Done {
                        keep_alive: *keep_alive,
                    }
                } else {
                    WriteResult::Pending
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => WriteResult::Pending,
            Err(_) => WriteResult::Error,
        }
    }

    fn route(&self, request: &Request, groups: &[Vec<ServerConfig>]) -> RouteResult {
        let configs: Vec<&ServerConfig> = groups[self.group_id].iter().collect();
        let server = router::select_server(&configs, request.header("host"));

        let (server_host, server_port) = server
            .address
            .rsplit_once(':')
            .unwrap_or((server.address.as_str(), ""));
        let remote_addr = self.peer_addr.ip().to_string();
        let ctx = cgi::CgiContext {
            server_name: server.server_name.as_deref().unwrap_or(server_host),
            server_port,
            remote_addr: &remote_addr,
        };

        let location = match router::match_location(server, &request.path) {
            Some(location) => location,
            None => {
                return RouteResult::Response(Response::error(
                    404,
                    "No location configured for this path",
                ))
            }
        };

        // OPTIONS is a discovery method (RFC 7231 SS4.3.7): a client uses it
        // to ask what's allowed here, so it must work even on a location
        // whose configured `methods` doesn't list OPTIONS itself.
        if request.method == Method::Options {
            return RouteResult::Response(
                Response::new(204, "No Content").header("Allow", &allow_header_value(location)),
            );
        }

        // A server that supports GET on a location is required to support
        // HEAD there too (RFC 7231 SS4.3.2) — it isn't a separate config
        // knob, so a bare "GET" implicitly covers it.
        let method_allowed = location
            .methods
            .iter()
            .any(|allowed| allowed == request.method.as_str())
            || (request.method == Method::Head
                && location.methods.iter().any(|allowed| allowed == "GET"));
        if !method_allowed {
            return RouteResult::Response(
                Response::error(405, "Method Not Allowed")
                    .header("Allow", &allow_header_value(location)),
            );
        }

        if request.body.len() > location.client_max_body_size {
            return RouteResult::Response(Response::error(
                413,
                "Request body exceeds this location's configured limit",
            ));
        }

        if let Some(interpreter) = cgi::interpreter_for(location, &request.path) {
            return match cgi::start(location, &interpreter, request, &request.path, &ctx) {
                cgi::StartOutcome::Started(process) => RouteResult::Cgi(process),
                cgi::StartOutcome::Failed(response) => RouteResult::Response(response),
            };
        }

        let response = match request.method {
            Method::Get => static_files::serve(location, &request.path),
            Method::Head => static_files::serve(location, &request.path).without_body(),
            Method::Post => file_ops::create(location, request),
            Method::Delete => file_ops::delete(location, &request.path),
            _ => Response::error(501, "Not Implemented"),
        };
        RouteResult::Response(response)
    }

    /// Called when epoll reports activity on one of this connection's CGI
    /// pipe fds (not the client socket).
    pub fn on_cgi_event(&mut self, fd: RawFd, readable: bool, writable: bool) -> Outcome {
        let ConnState::RunningCgi {
            cgi: process,
            keep_alive,
            meta,
        } = &mut self.state
        else {
            return Outcome::Continue; // stale event after the process already finished
        };
        let keep_alive = *keep_alive;
        let meta = meta.clone();

        let pid = process.pid();
        let result = cgi::advance(process, fd, readable, writable);

        if let Some(response) = result.done {
            meta.log(&self.peer_addr, &response);
            self.enter_writing(response, keep_alive);
            return Outcome::CgiFinished {
                closed_fds: result.closed_fds,
                pid,
            };
        }

        if result.closed_fds.is_empty() {
            Outcome::Continue
        } else {
            Outcome::UnregisterCgiFds(result.closed_fds)
        }
    }

    pub fn cgi_deadline_passed(&self, now: Instant) -> bool {
        match &self.state {
            ConnState::RunningCgi { cgi, .. } => cgi::is_expired(cgi, now),
            _ => false,
        }
    }

    /// Kills a timed-out CGI process and starts writing a 504 back to the
    /// client. Returns the pipe fds the caller must drop from its routing
    /// table (epoll itself already stops watching them once closed).
    pub fn timeout_cgi(&mut self) -> (Vec<RawFd>, libc::pid_t) {
        let ConnState::RunningCgi {
            cgi: process,
            keep_alive,
            meta,
        } = &mut self.state
        else {
            return (Vec::new(), 0);
        };
        let keep_alive = *keep_alive;
        let meta = meta.clone();
        let pid = process.pid();
        let fds: Vec<RawFd> = std::iter::once(process.stdout_fd())
            .chain(process.stdin_fd())
            .collect();
        cgi::kill(process);
        cgi::close_pipes(process);
        let response = Response::error(504, "CGI script timed out");
        meta.log(&self.peer_addr, &response);
        self.enter_writing(response, keep_alive);
        (fds, pid)
    }

    /// True if this connection has been idle too long. CGI has its own
    /// deadline (`cgi_deadline_passed`) and is excluded here.
    pub fn idle_timed_out(&self, now: Instant, idle_timeout: Duration) -> bool {
        !matches!(self.state, ConnState::RunningCgi { .. })
            && now.saturating_duration_since(self.last_activity) > idle_timeout
    }

    /// Cleans up any in-flight CGI process regardless of why this
    /// connection is being torn down (idle timeout, I/O error, or the
    /// client disconnecting mid-CGI). Safe to call unconditionally. Returns
    /// pipe fds the caller should also remove from its routing table.
    pub fn abandon_cgi(&mut self) -> Option<(Vec<RawFd>, libc::pid_t)> {
        if let ConnState::RunningCgi { cgi: process, .. } = &mut self.state {
            let pid = process.pid();
            let fds: Vec<RawFd> = std::iter::once(process.stdout_fd())
                .chain(process.stdin_fd())
                .collect();
            cgi::kill(process);
            cgi::close_pipes(process);
            Some((fds, pid))
        } else {
            None
        }
    }
}

enum WriteResult {
    Pending,
    Done { keep_alive: bool },
    Error,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Location;
    use std::collections::HashMap;
    use std::net::TcpListener;

    fn test_epoll_fd() -> RawFd {
        unsafe { libc::epoll_create1(0) }
    }

    fn groups_with_one_static_site(root: &std::path::Path) -> Vec<Vec<ServerConfig>> {
        vec![vec![ServerConfig {
            address: "127.0.0.1:0".to_string(),
            server_name: None,
            locations: vec![Location {
                path: "/".to_string(),
                root: root.to_string_lossy().to_string(),
                index: Some("index.html".to_string()),
                methods: vec!["GET".to_string()],
                autoindex: false,
                cgi: HashMap::new(),
                client_max_body_size: crate::config::DEFAULT_MAX_BODY_SIZE,
            }],
        }]]
    }

    /// Sets up a real connected TCP pair (a Connection on one end, a plain
    /// TcpStream the test drives directly on the other), matching this
    /// repo's existing preference for exercising real OS resources over
    /// mocking. The client side gets a read timeout so a bug in the
    /// hand-driven state machine below fails the test loudly instead of
    /// hanging the suite.
    fn accept_pair(epoll_fd: RawFd) -> (Connection, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).unwrap();
        // Short timeout: just enough to let drive_until_response retry past
        // the inherent loopback-delivery race without ever blocking long.
        client
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let (server_stream, peer_addr) = listener.accept().unwrap();
        let conn = Connection::accept(server_stream, peer_addr, 0, epoll_fd).unwrap();
        (conn, client)
    }

    /// Drives `conn.on_readable` until a response has been written
    /// (`Outcome::Close`, or a keep-alive write completing — detected by
    /// successfully reading nonempty bytes off `client`). Bounded so a
    /// state-machine bug fails fast instead of hanging.
    ///
    /// Deliberately does **not** read to EOF on the `Close` path: `conn`
    /// (which owns the server-side socket) is only borrowed here, so it's
    /// still open and a `read_to_end` would block forever waiting for a
    /// close that can't happen until the caller drops `conn`. Callers that
    /// get `closed == true` back should `drop(conn)` and then read any
    /// remaining bytes themselves.
    fn drive_until_response(
        conn: &mut Connection,
        client: &mut TcpStream,
        groups: &[Vec<ServerConfig>],
    ) -> (Vec<u8>, bool) {
        for _ in 0..200 {
            match conn.on_readable(groups) {
                Outcome::Close => return (read_available(client), true),
                Outcome::Continue => {
                    let bytes = read_available(client);
                    if !bytes.is_empty() {
                        return (bytes, false);
                    }
                }
                _ => panic!("unexpected CGI outcome for a static request"),
            }
        }
        panic!("gave up waiting for a response after 200 drive attempts");
    }

    /// One logical read attempt, retrying briefly through the inherent
    /// loopback-delivery race between the server's write and the client's
    /// read syscall observing it. Does not wait for EOF.
    fn read_available(client: &mut TcpStream) -> Vec<u8> {
        for _ in 0..50 {
            let mut buf = [0u8; 4096];
            match client.read(&mut buf) {
                Ok(0) => return Vec::new(),
                Ok(n) => return buf[..n].to_vec(),
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                Err(e) => panic!("client read error: {}", e),
            }
        }
        Vec::new()
    }

    static SITE_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn temp_site() -> std::path::PathBuf {
        let unique = SITE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "localhost_connection_test_{}_{}",
            std::process::id(),
            unique
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("index.html"), b"hello").unwrap();
        dir
    }

    #[test]
    fn serves_a_simple_get_request() {
        let root = temp_site();
        let groups = groups_with_one_static_site(&root);
        let epoll_fd = test_epoll_fd();
        let (mut conn, mut client) = accept_pair(epoll_fd);

        client
            .write_all(b"GET / HTTP/1.1\r\nHost: a\r\nConnection: close\r\n\r\n")
            .unwrap();

        let (response, closed) = drive_until_response(&mut conn, &mut client, &groups);
        assert!(closed, "Connection: close should end with Outcome::Close");
        // Drop the server side now that we've confirmed it closed, so any
        // later assertions can't accidentally block on it.
        drop(conn);

        let text = String::from_utf8(response).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.ends_with("hello"));

        unsafe { libc::close(epoll_fd) };
    }

    #[test]
    fn head_request_gets_get_headers_with_no_body() {
        let root = temp_site();
        let groups = groups_with_one_static_site(&root);
        let epoll_fd = test_epoll_fd();
        let (mut conn, mut client) = accept_pair(epoll_fd);

        client
            .write_all(b"HEAD / HTTP/1.1\r\nHost: a\r\nConnection: close\r\n\r\n")
            .unwrap();

        let (response, closed) = drive_until_response(&mut conn, &mut client, &groups);
        assert!(closed, "Connection: close should end with Outcome::Close");
        drop(conn);

        let text = String::from_utf8(response).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Content-Length: 5\r\n"), "{text}"); // "hello".len()
        assert!(
            text.ends_with("\r\n\r\n"),
            "HEAD response must not include a body: {text}"
        );

        unsafe { libc::close(epoll_fd) };
    }

    #[test]
    fn options_request_lists_allowed_methods_with_no_body() {
        let root = temp_site();
        let groups = groups_with_one_static_site(&root);
        let epoll_fd = test_epoll_fd();
        let (mut conn, mut client) = accept_pair(epoll_fd);

        client
            .write_all(b"OPTIONS / HTTP/1.1\r\nHost: a\r\nConnection: close\r\n\r\n")
            .unwrap();

        let (response, closed) = drive_until_response(&mut conn, &mut client, &groups);
        assert!(closed, "Connection: close should end with Outcome::Close");
        drop(conn);

        let text = String::from_utf8(response).unwrap();
        assert!(text.starts_with("HTTP/1.1 204 No Content\r\n"));
        assert!(text.contains("Allow: GET, HEAD, OPTIONS\r\n"), "{text}");
        assert!(
            text.ends_with("\r\n\r\n"),
            "OPTIONS response must not include a body: {text}"
        );

        unsafe { libc::close(epoll_fd) };
    }

    #[test]
    fn keeps_connection_alive_across_two_requests() {
        let root = temp_site();
        let groups = groups_with_one_static_site(&root);
        let epoll_fd = test_epoll_fd();
        let (mut conn, mut client) = accept_pair(epoll_fd);

        client
            .write_all(b"GET / HTTP/1.1\r\nHost: a\r\n\r\n")
            .unwrap();
        let (first_response, closed) = drive_until_response(&mut conn, &mut client, &groups);
        assert!(
            !closed,
            "keep-alive request should not close the connection"
        );
        assert!(String::from_utf8(first_response)
            .unwrap()
            .starts_with("HTTP/1.1 200 OK\r\n"));

        // Second request on the same connection.
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: a\r\nConnection: close\r\n\r\n")
            .unwrap();
        let (second_response, closed) = drive_until_response(&mut conn, &mut client, &groups);
        assert!(closed, "Connection: close should end with Outcome::Close");
        drop(conn);

        assert!(String::from_utf8(second_response)
            .unwrap()
            .starts_with("HTTP/1.1 200 OK\r\n"));

        unsafe { libc::close(epoll_fd) };
    }

    #[test]
    fn detects_idle_timeout() {
        let root = temp_site();
        let epoll_fd = test_epoll_fd();
        let (conn, _client) = accept_pair(epoll_fd);

        let now = Instant::now();
        assert!(!conn.idle_timed_out(now, Duration::from_secs(30)));

        let future = now + Duration::from_secs(31);
        assert!(conn.idle_timed_out(future, Duration::from_secs(30)));

        let _ = root;
        unsafe { libc::close(epoll_fd) };
    }

    #[test]
    fn malformed_request_gets_400_and_closes() {
        let root = temp_site();
        let groups = groups_with_one_static_site(&root);
        let epoll_fd = test_epoll_fd();
        let (mut conn, mut client) = accept_pair(epoll_fd);

        client.write_all(b"GET /\r\n\r\n").unwrap();
        let (response, closed) = drive_until_response(&mut conn, &mut client, &groups);
        assert!(closed, "a malformed request should end the connection");
        drop(conn);

        assert!(String::from_utf8(response)
            .unwrap()
            .starts_with("HTTP/1.1 400 Bad Request\r\n"));

        unsafe { libc::close(epoll_fd) };
    }
}
