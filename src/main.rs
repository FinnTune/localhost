mod cgi;
mod config;
mod connection;
mod file_ops;
mod fs_safety;
mod http;
mod json;
mod log;
mod multipart;
mod router;
mod static_files;

use config::{load_config, ServerConfig};
use connection::{Connection, Outcome};
use libc::{
    epoll_create1, epoll_ctl, epoll_event, epoll_wait, EPOLLERR, EPOLLHUP, EPOLLIN, EPOLLOUT,
    EPOLL_CTL_ADD,
};
use log::{blue, green};
use std::collections::HashMap;
use std::net::TcpListener;
use std::os::unix::io::{AsRawFd, RawFd};
use std::time::{Duration, Instant};

const IDLE_READ_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_EVENTS: usize = 128;
/// epoll_wait's timeout: bounds how stale idle/CGI-timeout sweeps can get
/// when there's no I/O activity to wake us up otherwise.
const SWEEP_INTERVAL_MS: i32 = 1000;
/// How long to keep politely asking a killed CGI child to be reaped before
/// giving up (it should die almost immediately after SIGKILL).
const REAP_PATIENCE: Duration = Duration::from_secs(2);

/// What kind of fd an epoll event belongs to, so the main loop knows how
/// to route it.
#[derive(Clone, Copy)]
enum FdRole {
    Listener(usize),
    Client,
    /// A CGI pipe fd (stdin or stdout); the client fd that owns it.
    CgiPipe(RawFd),
}

fn epoll_add(epoll_fd: RawFd, fd: RawFd, events: u32) {
    let mut event = epoll_event {
        events,
        u64: fd as u64,
    };
    unsafe {
        epoll_ctl(epoll_fd, EPOLL_CTL_ADD, fd, &mut event);
    }
}

/// Applies the bookkeeping side effects of a `connection::Outcome`: keeping
/// `connections`/`fd_roles` in sync, and queuing CGI pids for reaping.
/// Epoll registration for the client socket itself is handled inside
/// `Connection`; this only concerns the extra CGI pipe fds and the
/// connection's lifetime in our own maps.
fn apply_outcome(
    client_fd: RawFd,
    conn: Connection,
    outcome: Outcome,
    connections: &mut HashMap<RawFd, Connection>,
    fd_roles: &mut HashMap<RawFd, FdRole>,
    pending_reaps: &mut Vec<(libc::pid_t, Instant)>,
) {
    match outcome {
        Outcome::Continue => {
            connections.insert(client_fd, conn);
        }
        Outcome::RegisterCgi {
            stdout_fd,
            stdin_fd,
        } => {
            fd_roles.insert(stdout_fd, FdRole::CgiPipe(client_fd));
            if let Some(stdin_fd) = stdin_fd {
                fd_roles.insert(stdin_fd, FdRole::CgiPipe(client_fd));
            }
            connections.insert(client_fd, conn);
        }
        Outcome::UnregisterCgiFds(fds) => {
            for fd in fds {
                fd_roles.remove(&fd);
            }
            connections.insert(client_fd, conn);
        }
        Outcome::CgiFinished { closed_fds, pid } => {
            for fd in closed_fds {
                fd_roles.remove(&fd);
            }
            pending_reaps.push((pid, Instant::now() + REAP_PATIENCE));
            connections.insert(client_fd, conn);
        }
        Outcome::Close => {
            close_connection(client_fd, conn, fd_roles, pending_reaps);
        }
    }
}

/// Tears down a connection: abandons any live CGI process (killing it and
/// queuing its pid for reaping), drops its pipe fds from `fd_roles`, and
/// removes the client fd itself. The connection's `TcpStream` closes when
/// `conn` is dropped at the end of this function.
fn close_connection(
    client_fd: RawFd,
    mut conn: Connection,
    fd_roles: &mut HashMap<RawFd, FdRole>,
    pending_reaps: &mut Vec<(libc::pid_t, Instant)>,
) {
    if let Some((fds, pid)) = conn.abandon_cgi() {
        for fd in fds {
            fd_roles.remove(&fd);
        }
        pending_reaps.push((pid, Instant::now() + REAP_PATIENCE));
    }
    fd_roles.remove(&client_fd);
}

/// One non-blocking `waitpid(WNOHANG)` attempt per pending pid; anything
/// past its own deadline gets SIGKILLed again (harmless if already dead)
/// and stays queued for the next pass. Never blocks the event loop.
fn reap_pending(pending: &mut Vec<(libc::pid_t, Instant)>) {
    let now = Instant::now();
    pending.retain(|(pid, deadline)| {
        let mut status = 0;
        let result = unsafe { libc::waitpid(*pid, &mut status, libc::WNOHANG) };
        if result != 0 {
            return false; // reaped (or ECHILD - already gone); stop tracking
        }
        if now >= *deadline {
            unsafe { libc::kill(*pid, libc::SIGKILL) };
        }
        true
    });
}

/// Closes any connection idle too long, and times out any CGI process past
/// its own deadline (writing a 504 instead of dropping the connection).
fn sweep_timeouts(
    connections: &mut HashMap<RawFd, Connection>,
    fd_roles: &mut HashMap<RawFd, FdRole>,
    pending_reaps: &mut Vec<(libc::pid_t, Instant)>,
) {
    let now = Instant::now();

    let expired_cgi: Vec<RawFd> = connections
        .iter()
        .filter(|(_, conn)| conn.cgi_deadline_passed(now))
        .map(|(fd, _)| *fd)
        .collect();
    for fd in expired_cgi {
        if let Some(conn) = connections.get_mut(&fd) {
            let (fds, pid) = conn.timeout_cgi();
            for fd in fds {
                fd_roles.remove(&fd);
            }
            pending_reaps.push((pid, Instant::now() + REAP_PATIENCE));
        }
    }

    let idle: Vec<RawFd> = connections
        .iter()
        .filter(|(_, conn)| conn.idle_timed_out(now, IDLE_READ_TIMEOUT))
        .map(|(fd, _)| *fd)
        .collect();
    for fd in idle {
        if let Some(conn) = connections.remove(&fd) {
            close_connection(fd, conn, fd_roles, pending_reaps);
        }
    }
}

fn main() -> std::io::Result<()> {
    let config = match load_config("config/config.json") {
        Ok(config) => config,
        Err(e) => {
            eprintln!("Invalid configuration: {}", e);
            std::process::exit(1);
        }
    };

    let epoll_fd = unsafe { epoll_create1(0) };
    if epoll_fd == -1 {
        panic!("Failed to create epoll instance");
    }

    // Group server blocks by listening address: several blocks can share
    // one port and are disambiguated later by Host header (name-based
    // virtual hosting), so each unique address is bound only once. `groups`
    // is indexed by group_id, which `Connection` uses to look its servers
    // back up without needing a lifetime parameter tying it to `config`.
    let mut addresses: Vec<String> = Vec::new();
    let mut groups: Vec<Vec<ServerConfig>> = Vec::new();
    for server_config in config.servers {
        match addresses.iter().position(|a| a == &server_config.address) {
            Some(group_id) => groups[group_id].push(server_config),
            None => {
                addresses.push(server_config.address.clone());
                groups.push(vec![server_config]);
            }
        }
    }

    let mut fd_roles: HashMap<RawFd, FdRole> = HashMap::new();
    let mut listeners: Vec<TcpListener> = Vec::new();

    for (group_id, address) in addresses.iter().enumerate() {
        let listener = TcpListener::bind(address)?;
        listener.set_nonblocking(true)?;
        let fd = listener.as_raw_fd();
        epoll_add(epoll_fd, fd, EPOLLIN as u32);
        fd_roles.insert(fd, FdRole::Listener(group_id));

        let names: Vec<&str> = groups[group_id]
            .iter()
            .map(|c| c.server_name.as_deref().unwrap_or("default"))
            .collect();
        println!(
            "Server up and running on {}: {} ({})",
            blue(address),
            green("✓"),
            names.join(", ")
        );

        listeners.push(listener);
    }

    let mut connections: HashMap<RawFd, Connection> = HashMap::new();
    let mut pending_reaps: Vec<(libc::pid_t, Instant)> = Vec::new();

    loop {
        let mut events = [epoll_event { events: 0, u64: 0 }; MAX_EVENTS];
        let num_events = unsafe {
            epoll_wait(
                epoll_fd,
                events.as_mut_ptr(),
                MAX_EVENTS as i32,
                SWEEP_INTERVAL_MS,
            )
        };

        if num_events == -1 {
            eprintln!("Error in epoll wait");
            continue;
        }

        for event in events.iter().take(num_events as usize) {
            let fd = event.u64 as RawFd;
            // EPOLLHUP/EPOLLERR are always implicitly monitored and can
            // fire without EPOLLIN also set (e.g. a pipe's write end
            // closing) — treat them as "readable" too so the resulting
            // read() call is what turns that into a proper EOF/error
            // detection, rather than silently never noticing.
            let readable = event.events & (EPOLLIN as u32 | EPOLLHUP as u32 | EPOLLERR as u32) != 0;
            let writable = event.events & (EPOLLOUT as u32) != 0;

            match fd_roles.get(&fd).copied() {
                Some(FdRole::Listener(group_id)) => match listeners[group_id].accept() {
                    Ok((stream, peer_addr)) => {
                        match Connection::accept(stream, peer_addr, group_id, epoll_fd) {
                            Ok(conn) => {
                                let client_fd = conn.fd();
                                fd_roles.insert(client_fd, FdRole::Client);
                                connections.insert(client_fd, conn);
                            }
                            Err(e) => eprintln!("Failed to prepare connection: {}", e),
                        }
                    }
                    Err(e) => eprintln!("Failed to accept connection: {}", e),
                },
                Some(FdRole::Client) => {
                    if let Some(mut conn) = connections.remove(&fd) {
                        let outcome = if readable {
                            conn.on_readable(&groups)
                        } else {
                            conn.on_writable(&groups)
                        };
                        apply_outcome(
                            fd,
                            conn,
                            outcome,
                            &mut connections,
                            &mut fd_roles,
                            &mut pending_reaps,
                        );
                    }
                }
                Some(FdRole::CgiPipe(owner_fd)) => {
                    if let Some(mut conn) = connections.remove(&owner_fd) {
                        let outcome = conn.on_cgi_event(fd, readable, writable);
                        apply_outcome(
                            owner_fd,
                            conn,
                            outcome,
                            &mut connections,
                            &mut fd_roles,
                            &mut pending_reaps,
                        );
                    }
                }
                None => {} // stale event for an fd we've already cleaned up
            }
        }

        sweep_timeouts(&mut connections, &mut fd_roles, &mut pending_reaps);
        reap_pending(&mut pending_reaps);
    }
}
