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
  `Content-Length` bodies) and a response builder
- Location-based routing (longest-prefix match, nginx-style) and static file
  serving, with path canonicalization to block directory traversal

Not yet implemented: method enforcement (POST/DELETE), keep-alive and
chunked transfer encoding, name-based virtual hosts, CGI, uploads, directory
listing (`autoindex`), and load/stress-test hardening.

## Running

```sh
cargo run
```

The bundled `config/config.json` starts two servers demonstrating routing
across two ports:

```sh
curl http://127.0.0.1:8080/
curl http://127.0.0.1:8080/about
curl http://127.0.0.1:8081/contact
```

## Configuration

`config/config.json` defines one or more `servers`, each with an `address`
and a list of `locations`:

```json
{
  "path": "/about",
  "root": "www/site1",
  "index": "about.html",
  "methods": ["GET"],
  "autoindex": false
}
```

Requests are matched to the most specific (longest-prefix) location whose
`path` prefixes the request path, then served as a static file rooted at
`root` (falling back to `index` for directory requests).

## Testing

```sh
cargo test
cargo clippy -- -D warnings
cargo fmt --all -- --check
```

CI (`.github/workflows/rust.yml`) runs all three on every push and PR;
`.github/dependabot.yml` keeps `libc` and the workflow's pinned actions
current.

## License

GPL-2.0, see [LICENSE](LICENSE).
