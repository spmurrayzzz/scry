# scry

Web search and HTML extraction for agents. Rust library, CLI, REST API, and MCP over stdio or Streamable HTTP.

Search uses DuckDuckGo HTML. Page retrieval and Markdown extraction run locally. DuckDuckGo access is unofficial and can be blocked or rate limited.

## CLI

```sh
make install

scry search "reqwest ClientBuilder" --num-results 5 --include-text
scry contents https://docs.rs/reqwest/latest/reqwest/
scry serve
```

Search and contents write plain text to stdout by default. Use `--json` for structured results and operation errors. Errors go to stderr. Exit codes: `0` success, `1` failure, `2` partial results or invalid/unsupported arguments, `130` interrupt.

## MCP and HTTP

With `scry` on PATH, pi can launch the stdio server:

```json
{
  "mcpServers": {
    "scry": {
      "command": "scry",
      "args": ["mcp"]
    }
  }
}
```

`scry serve` binds to `127.0.0.1:8787` by default. REST routes are `POST /search`, `POST /contents`, and `GET /health`. Streamable HTTP MCP is at `/mcp`.

Set `SCRY_AUTH_TOKEN` to require Bearer authentication on every HTTP route. Non-loopback binds require it. Use `--allow-host HOST[:PORT]` for an exact proxy authority. Origin headers are rejected. Prefer loopback behind Tailscale Serve.

Once a REST operation starts, HTTP 200 can contain either results or an error envelope. Inspect the envelope and each item's `status`, `error`, and `truncated` fields.

## Library

Inside a Tokio runtime:

```rust
use scry::{Config, Scry, SearchRequest};

let scry = Scry::new(Config::default())?;
let response = scry.search(SearchRequest {
    query: "reqwest ClientBuilder".into(),
    include_text: true,
    ..Default::default()
}).await?;
```

Reuse `Scry` across calls. `contents` accepts `ContentsRequest`. The `_with_context` methods accept explicit deadlines and cancellation through `RequestContext`.

See [request and response types](src/api.rs) and [configurable limits](src/config.rs).

## Constraints

- Result IDs are normalized source URLs. Batches preserve order and per-URL failures.
- Retrieval accepts public HTTP(S) destinations on standard ports. DNS addresses are validated and pinned. Redirects receive the same checks.
- HTML/XHTML only. No JavaScript rendering, authenticated browsing, or PDF extraction.
- `auto` and `keyword` both use keyword search. Neural search and publication-date filters are unsupported.

The full contract is in [AGENTS.md](AGENTS.md).

## Development

`make` builds debug, `make release` builds optimized, and `make check` runs formatting checks, Clippy, and the default tests.

```sh
make test
```

The default suite uses local inputs and loopback processes. The separate live probe runs one production search and reads one returned page:

```sh
make test-live
```
