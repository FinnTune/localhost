use colored::*;
use nix::sys::epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::io::{AsRawFd, RawFd};
use std::str;

#[derive(Serialize, Deserialize, Debug)]
struct ServerConfig {
    address: String,
    endpoints: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug)]
struct Config {
    servers: Vec<ServerConfig>,
}

fn load_config(file_path: &str) -> serde_json::Result<Config> {
    let config_str = fs::read_to_string(file_path).expect("Failed to read configuration file.");
    serde_json::from_str(&config_str)
}
fn handle_client(mut stream: TcpStream, _config: &ServerConfig) -> std::io::Result<()> {
    let mut buffer = [0; 1024];
    let bytes_read = stream.read(&mut buffer)?;
    if bytes_read == 0 {
        // Handle the case where the connection was closed or no data was read
        // This could be returning an error or simply exiting the function
        return Ok(()); // Example handling
    }
    println!(
        "Request: {}",
        str::from_utf8(&buffer[..bytes_read]).unwrap()
    );

    let response = "HTTP/1.1 200 OK\r\n\r\n";
    stream.write_all(response.as_bytes())?;
    stream.flush()?;

    Ok(())
}

fn main() -> std::io::Result<()> {
    let config = load_config("config/config.json").expect("Failed to load config");
    let epoll_fd = Epoll::new(EpollCreateFlags::EPOLL_CLOEXEC).unwrap();
    let mut listeners = HashMap::new();
    let mut _events: Vec<EpollEvent> = Vec::new();

    // Set up listeners for each server configuration
    for server_config in &config.servers {
        let listener = TcpListener::bind(&server_config.address)?;
        listener.set_nonblocking(true)?;
        let fd = listener.as_raw_fd();

        let event = EpollEvent::new(EpollFlags::EPOLLIN, fd as u64);
        epoll_fd.add(&listener, event).unwrap();
        listeners.insert(fd, (listener, server_config));

        // Print statement with ANSI coloration
        println!(
            "Server up and running on {}: {}",
            server_config.address.blue(),
            "✓".green()
        );
    }

    loop {
        let mut events_buffer = [EpollEvent::empty(); 10];
        let num_events = epoll_fd.wait(&mut events_buffer, -1).unwrap();

        for event in &events_buffer[..num_events] {
            let fd = event.data() as RawFd;
            if let Some((listener, config)) = listeners.get(&fd) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if let Err(e) = handle_client(stream, config) {
                            eprintln!("Failed to handle client: {}", e);
                        }
                    }
                    Err(e) => eprintln!("Failed to accept connection: {}", e),
                }
            }
        }
    }
}
