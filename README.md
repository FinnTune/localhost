# localhost

A HTTP/1.1 server built from scratch in Rust: raw TCP sockets, a hand-rolled
request parser, and an `epoll`-based event loop, with no web framework and
(intentionally) almost no external crates. `libc` is the only dependency,
needed for `epoll_create1`/`epoll_ctl`/`epoll_wait`, which aren't in the
standard library. Config parsing, HTTP parsing, and response building are all
implemented directly in this repo rather than pulled in from `serde`,
`hyper`, etc.

## Status

Implemented so far:
- Non-blocking TCP listeners multiplexed on one `epoll` instance
- A hand-rolled JSON parser/value type for reading `config/config.json`
- An incremental HTTP/1.1 request parser (tolerates partial reads,
  `Content-Length` and chunked bodies) and a response builder
- Location-based routing (longest-prefix match, nginx-style) and static file
  serving, with path canonicalization to block directory traversal
- GET/POST/DELETE with per-location method enforcement (`405` + `Allow`),
  POST writing uploaded bodies to disk, DELETE removing files
- Persistent (keep-alive) connections honoring `Connection: close` and the
  HTTP/1.0-vs-1.1 default, plus an idle-read timeout so an abandoned
  connection doesn't hang around forever
- Name-based virtual hosts: several server blocks can share one listening
  port, disambiguated by the `Host` header, with the first block for that
  address acting as the default when there's no match
- CGI execution via raw `fork`/`execv`/pipes (no subprocess crate): request
  body and script stdout are pumped concurrently over non-blocking pipes to
  avoid deadlocking on a full pipe buffer, full CGI/1.1 environment
  variables, a `Status:` response header override, a 5s execution timeout
  (`504`), and `waitpid` reaping so finished scripts never become zombies
- Real (`multipart/form-data`) file uploads: the filename comes from the
  part's `Content-Disposition` header rather than the URL, with directory
  components stripped so it can't be used for traversal; a hand-rolled
  parser handles binary content containing raw `\r\n` bytes
- `autoindex`: a generated HTML directory listing when there's no servable
  index file and the location opts in
- Per-location `client_max_body_size` (`413` when exceeded), independent
  of the parser's hard 10MB safety ceiling that applies regardless of config
- Fully event-driven connection handling: client sockets and CGI pipe fds
  all share one `epoll` instance (see Architecture below), so a slow or
  idle client can no longer block any other client

All nine planned phases are done.

## Architecture

One thread, one `epoll` instance, multiplexing everything: listening
sockets, every accepted client connection, and every CGI child's stdin/stdout
pipes. This is the classic C10K-style design (what nginx's worker loop and
Node's event loop are both built on), and it's the reason a slow client or a
slow CGI script can't starve anyone else — nothing ever blocks waiting on
one peer while others are ready.

`src/connection.rs` holds a `Connection` per accepted socket as a small state
machine (`ReadingRequest` / `Writing` / `RunningCgi`), driven by non-blocking
`read()`/`write()` calls in response to `epoll` events rather than the
blocking-with-a-timeout reads earlier phases used. `src/cgi.rs` mirrors this:
`cgi::start` forks and returns immediately, and `cgi::advance` does one
non-blocking read/write step per pipe-fd event instead of blocking the whole
server in its own internal `poll()` loop. `src/main.rs` stays thin — an
fd-to-role routing table (`Listener` / `Client` / `CgiPipe`) dispatching
events, a non-blocking `waitpid` sweep so finished CGI children never become
zombies, and a periodic idle/CGI-timeout sweep bounded by `epoll_wait`'s own
timeout parameter.

One deliberate simplification: local disk I/O (serving a static file,
writing an upload) still happens synchronously inline when a request
resolves. That's outside the scope of the C10K problem this design solves —
it targets slow/idle *network peers* and *CGI subprocesses*, not local
filesystem latency, which is negligible on any reasonable disk.

## Running

```sh
cargo run
```

The bundled `config/config.json` starts two servers demonstrating routing
across two ports, plus a second name-based virtual host sharing port 8080:

```sh
curl http://127.0.0.1:8080/
curl http://127.0.0.1:8080/about
curl http://127.0.0.1:8081/contact
curl http://127.0.0.1:8080/ -H "Host: beta.localhost"
curl "http://127.0.0.1:8080/cgi-bin/hello.sh?foo=bar"
curl -F "file=@somefile.png" http://127.0.0.1:8080/upload
curl http://127.0.0.1:8080/upload/
```

## Configuration

`config/config.json` defines one or more `servers`, each with an `address`,
an optional `server_name`, and a list of `locations`:

```json
{
  "address": "127.0.0.1:8080",
  "server_name": "beta.localhost",
  "locations": [
    { "path": "/about", "root": "www/site1", "index": "about.html", "methods": ["GET"], "autoindex": false }
  ]
}
```

Several server blocks can share one `address`; the `Host` header (port
suffix stripped) picks between them by matching `server_name`, falling
back to the first block declared for that address if there's no header or
no match. Within a chosen server, requests are matched to the most
specific (longest-prefix) location whose `path` prefixes the request path,
then served as a static file rooted at `root` (falling back to `index` for
directory requests, or an `autoindex` listing if enabled and there's no
index) — unless the request path's extension is a key in that location's
`cgi` map, in which case it's executed by the mapped interpreter instead
(e.g. `"cgi": { "sh": "/bin/sh" }` runs `*.sh` files under that location
through `/bin/sh`). `client_max_body_size` (bytes, default 10MB) rejects
oversized request bodies with `413` before any handler runs.

## Testing

```sh
cargo test
cargo clippy -- -D warnings
cargo fmt --all -- --check
```

CI (`.github/workflows/rust.yml`) runs all three on every push and PR;
`.github/dependabot.yml` keeps `libc` and the workflow's pinned actions
current.

## Stress testing

With the server running (`cargo run`), a concurrent load test against a mix
of static and CGI endpoints:

```sh
printf '%s\n' \
  'http://127.0.0.1:8080/' \
  'http://127.0.0.1:8080/about' \
  'http://127.0.0.1:8080/cgi-bin/hello.sh?x=1' > /tmp/urls.txt
siege -c 25 -t 30S -f /tmp/urls.txt
```

Verify no resource leaks across the run:

```sh
ls /proc/$(pgrep -f target/debug/localhost)/fd | wc -l   # fd count, before vs. after
ps aux | grep '[d]efunct'                                  # zombie CGI children (should be empty)
```

To confirm concurrency actually works (not just throughput under a
well-behaved load), hold one connection open and idle, then fire a second
client — it should succeed immediately rather than waiting out the 30s idle
timeout:

```sh
(exec 3<>/dev/tcp/127.0.0.1/8080; sleep 8) &
curl -w '%{time_total}\n' http://127.0.0.1:8080/
```

Malformed/adversarial input (garbage bytes, no CRLF, oversized headers, a
false `Content-Length` far larger than the actual body) should never crash
the server or cause it to stop responding to well-formed requests
afterward — send it raw over `/dev/tcp` and confirm a subsequent `curl`
still gets a `200`.

## License

GPL-2.0, see [LICENSE](LICENSE).
