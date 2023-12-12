use colored::*;
use libc::{epoll_create1, epoll_ctl, epoll_event, epoll_wait, EPOLL_CTL_ADD, EPOLLIN};
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
        "Request: \n{}",
        str::from_utf8(&buffer[..bytes_read]).unwrap()
    );

    let response = "HTTP/1.1 200 OK\r\n\r\n";
    stream.write_all(response.as_bytes())?;
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
            server_config.address.blue(),
            "✓".green()
        );
    }

    loop {
        let mut events = [epoll_event { events: 0, u64: 0 }; 10];
        let num_events = unsafe { epoll_wait(epoll_fd, events.as_mut_ptr(), 10, -1) };

        if num_events == -1 {
            eprintln!("Error in epoll wait");
            continue;
        }

        for i in 0..num_events as usize {
            let fd = events[i].u64 as RawFd;
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
