mod config;
mod file_ops;
mod fs_safety;
mod http;
mod json;
mod log;
mod router;
mod static_files;

use config::{load_config, Location, ServerConfig};
use http::{Method, ParseOutcome, Request, Response};
use libc::{epoll_create1, epoll_ctl, epoll_event, epoll_wait, EPOLLIN, EPOLL_CTL_ADD};
use log::{blue, green};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::io::{AsRawFd, RawFd};
use std::time::Duration;

const IDLE_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Parses the next request off `buffer` (which may already hold pipelined
/// bytes left over from a previous request on this connection), reading
/// more from the socket as needed. Returns the request plus whatever bytes
/// after it haven't been consumed yet, so the caller can feed them straight
/// back in for the next request on a persistent connection.
fn read_request(
    stream: &mut TcpStream,
    mut buffer: Vec<u8>,
) -> std::io::Result<Option<(Request, Vec<u8>)>> {
    let mut chunk = [0u8; 4096];

    loop {
        match http::request::parse(&buffer) {
            ParseOutcome::Complete { request, consumed } => {
                let remaining = buffer[consumed..].to_vec();
                return Ok(Some((request, remaining)));
            }
            ParseOutcome::Invalid { status, message } => {
                let response = Response::error(status, &message);
                stream.write_all(&response.to_bytes())?;
                stream.flush()?;
                return Ok(None);
            }
            ParseOutcome::Incomplete => match stream.read(&mut chunk) {
                Ok(0) => return Ok(None), // client closed before a complete request arrived
                Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    return Ok(None); // idle too long; drop the connection
                }
                Err(e) => return Err(e),
            },
        }
    }
}

fn dispatch(location: &Location, request: &Request) -> Response {
    if !location
        .methods
        .iter()
        .any(|allowed| allowed == request.method.as_str())
    {
        return Response::error(405, "Method Not Allowed")
            .header("Allow", &location.methods.join(", "));
    }

    match request.method {
        Method::Get => static_files::serve(location, &request.path),
        Method::Post => file_ops::create(location, &request.path, &request.body),
        Method::Delete => file_ops::delete(location, &request.path),
        _ => Response::error(501, "Not Implemented"),
    }
}

fn handle_client(mut stream: TcpStream, configs: &[&ServerConfig]) -> std::io::Result<()> {
    stream.set_read_timeout(Some(IDLE_READ_TIMEOUT))?;
    let mut leftover = Vec::new();

    loop {
        let (request, remaining) = match read_request(&mut stream, leftover)? {
            Some(pair) => pair,
            None => return Ok(()),
        };
        leftover = remaining;

        println!("Request: {} {}", request.method.as_str(), request.path);

        let server = router::select_server(configs, request.header("host"));
        let response = match router::match_location(server, &request.path) {
            Some(location) => dispatch(location, &request),
            None => Response::error(404, "No location configured for this path"),
        };

        let keep_alive = request.keep_alive();
        stream.write_all(&response.to_bytes())?;
        stream.flush()?;

        if !keep_alive {
            return Ok(());
        }
    }
}

fn main() -> std::io::Result<()> {
    let config = load_config("config/config.json").expect("Failed to load config");

    let epoll_fd = unsafe { epoll_create1(0) };
    if epoll_fd == -1 {
        panic!("Failed to create epoll instance");
    }

    // Group server blocks by listening address: several blocks can share one
    // port and are disambiguated later by Host header (name-based virtual
    // hosting), so we bind each unique address only once.
    let mut groups: HashMap<&str, Vec<&ServerConfig>> = HashMap::new();
    for server_config in &config.servers {
        groups
            .entry(server_config.address.as_str())
            .or_default()
            .push(server_config);
    }

    let mut listeners = HashMap::new();

    for (address, server_configs) in groups {
        let listener = TcpListener::bind(address)?;
        listener.set_nonblocking(true)?;
        let fd = listener.as_raw_fd();

        let mut event = epoll_event {
            events: EPOLLIN as u32,
            u64: fd as u64,
        };

        unsafe {
            epoll_ctl(epoll_fd, EPOLL_CTL_ADD, fd, &mut event);
        }

        let names: Vec<&str> = server_configs
            .iter()
            .map(|c| c.server_name.as_deref().unwrap_or("default"))
            .collect();

        listeners.insert(fd, (listener, server_configs));

        println!(
            "Server up and running on {}: {} ({})",
            blue(address),
            green("✓"),
            names.join(", ")
        );
    }

    loop {
        let mut events = [epoll_event { events: 0, u64: 0 }; 10];
        let num_events = unsafe { epoll_wait(epoll_fd, events.as_mut_ptr(), 10, -1) };

        if num_events == -1 {
            eprintln!("Error in epoll wait");
            continue;
        }

        for event in events.iter().take(num_events as usize) {
            let fd = event.u64 as RawFd;
            if let Some((listener, configs)) = listeners.get(&fd) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if let Err(e) = handle_client(stream, configs) {
                            eprintln!("Failed to handle client: {}", e);
                        } else {
                            println!("Handled client");
                        }
                    }
                    Err(e) => eprintln!("Failed to accept connection: {}", e),
                }
            }
        }
    }
}
