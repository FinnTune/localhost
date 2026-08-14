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
- Access logging in Combined Log Format (the format nginx/Apache use) to
  stdout, one line per response — including CGI and error responses
- Spec-compliant response headers: `Date` and `Server` on every response,
  and an explicit `Connection: keep-alive`/`close` reflecting what the
  server actually decided (rather than leaving HTTP/1.0 clients to guess),
  plus `505 HTTP Version Not Supported` for anything other than 1.0/1.1.
  `Date`/access-log timestamps are hand-formatted from `libc::gmtime_r`/
  `localtime_r`'s raw fields rather than `strftime`'s locale-dependent
  `%a`/`%b`, so month/weekday names can't silently break the wire format
  on a non-English `LC_TIME`

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
scripts/smoke.sh
```

`cargo test` exercises modules directly; `scripts/smoke.sh` is the
complement — it boots the real binary and drives it over real sockets with
curl (static files, 404, method-not-allowed, CGI, a full upload/GET/DELETE
round trip, malformed input), catching integration-level regressions unit
tests can't see. Takes a few seconds.

CI (`.github/workflows/rust.yml`) runs all four on every push and PR;
`.github/dependabot.yml` keeps `libc` and the workflow's pinned actions
current.

## Stress testing

```sh
scripts/stress.sh
```

Builds the server, starts it (or reuses an already-running instance if
it's genuinely `target/debug/localhost` — it checks the port owner's
`/proc/<pid>/exe` before reusing it, rather than assuming anything that
answers HTTP on that port is safe to test against), then runs everything
that used to be a manual checklist:

- a `siege`-driven concurrent load test against a mix of static and CGI
  endpoints (falls back to a burst of parallel `curl`s if `siege` isn't
  installed), asserting 100% availability
- an fd-count diff across the run to catch resource leaks
- a `ps aux | grep defunct` check for zombie CGI children
- **the concurrency proof**: holds one connection open and idle, then
  fires a second client concurrently — it must be served immediately
  rather than waiting out the idle timeout, which is the whole point of
  the `epoll`-based design (see Architecture above)
- a malformed-input pass (garbage bytes, an oversized header past the 8KB
  limit, a `Content-Length` far larger than the actual body) confirming
  the server keeps answering `200` to well-formed requests afterward
  instead of crashing or wedging

Exits non-zero if anything fails. `HOST`/`PORT`/`SIEGE_CONCURRENCY`/
`SIEGE_TIME` are overridable via env vars if you point it at a different
config.

## License

GPL-2.0, see [LICENSE](LICENSE).
