mod config;
mod http;
mod json;
mod log;
mod router;
mod static_files;

use config::{load_config, ServerConfig};
use http::{ParseOutcome, Request, Response};
use libc::{epoll_create1, epoll_ctl, epoll_event, epoll_wait, EPOLLIN, EPOLL_CTL_ADD};
use log::{blue, green};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::io::{AsRawFd, RawFd};

fn read_request(stream: &mut TcpStream) -> std::io::Result<Option<Request>> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];

    loop {
        match http::request::parse(&buffer) {
            ParseOutcome::Complete { request, .. } => return Ok(Some(request)),
            ParseOutcome::Invalid { status, message } => {
                let response = Response::error(status, &message);
                stream.write_all(&response.to_bytes())?;
                stream.flush()?;
                return Ok(None);
            }
            ParseOutcome::Incomplete => {
                let bytes_read = stream.read(&mut chunk)?;
                if bytes_read == 0 {
                    // Client closed the connection before sending a complete request.
                    return Ok(None);
                }
                buffer.extend_from_slice(&chunk[..bytes_read]);
            }
        }
    }
}

fn handle_client(mut stream: TcpStream, config: &ServerConfig) -> std::io::Result<()> {
    let request = match read_request(&mut stream)? {
        Some(request) => request,
        None => return Ok(()),
    };

    println!("Request: {} {}", request.method.as_str(), request.path);

    let response = match router::match_location(config, &request.path) {
        Some(location) => static_files::serve(location, &request.path),
        None => Response::error(404, "No location configured for this path"),
    };

    stream.write_all(&response.to_bytes())?;
    stream.flush()?;

    Ok(())
}

fn main() -> std::io::Result<()> {
    let config = load_config("config/config.json").expect("Failed to load config");

    let epoll_fd = unsafe { epoll_create1(0) };
    if epoll_fd == -1 {
        panic!("Failed to create epoll instance");
    }

    let mut listeners = HashMap::new();

    for server_config in &config.servers {
        let listener = TcpListener::bind(&server_config.address)?;
        listener.set_nonblocking(true)?;
        let fd = listener.as_raw_fd();

        let mut event = epoll_event {
            events: EPOLLIN as u32,
            u64: fd as u64,
        };

        unsafe {
            epoll_ctl(epoll_fd, EPOLL_CTL_ADD, fd, &mut event);
        }

        listeners.insert(fd, (listener, server_config));

        println!(
            "Server up and running on {}: {}",
            blue(&server_config.address),
            green("✓")
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
            if let Some((listener, config)) = listeners.get(&fd) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if let Err(e) = handle_client(stream, config) {
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
