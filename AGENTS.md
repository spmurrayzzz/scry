# scry

`scry` is a small self-hosted Rust service for agent web search and page reading. One binary provides a CLI, a REST API, and MCP over stdio or Streamable HTTP. Search finds URLs through DuckDuckGo HTML. Page reading gets public HTML pages and returns bounded text and highlights.

DuckDuckGo access is unofficial. The service can be blocked, challenged, or rate limited. Report these conditions as errors. Never report them as an empty result set.

The tool interface follows the Exa search and contents pattern. Exa, Firecrawl, DDGS, and SearXNG are not runtime dependencies. The project uses Rust edition 2024. HTTP retrieval uses Reqwest with Rustls. Do not add Python or Node.js sidecars.

## Layout

- `src/api.rs`: request, response, result, and error contracts. JSON fields use camelCase. The service rejects unknown fields.
- `src/config.rs`: `Config` and `Limits`, with the defaults and the validation.
- `src/context.rs`: request deadline and cancellation token.
- `src/policy.rs`: URL syntax checks, destination checks, and domain filters.
- `src/fetch.rs`: guarded HTTP retrieval. DNS answers are validated and pinned to the connection.
- `src/fetch/provider.rs`: DuckDuckGo-only request headers. It uses the shared Reqwest client, guarded retrieval path, and bounded HTML decoder.
- `src/extract.rs`: bounded HTML parsing, text rendering, highlights, and screen detection.
- `src/duckduckgo.rs`: provider result parsing and redirect unwrapping.
- `src/service.rs`: request validation, search and contents workflows, concurrency limits, and response bounding.
- `src/http.rs`: REST server.
- `src/mcp.rs`: shared MCP handler, tool schemas, and HTTP session management.
- `src/stdio.rs`: bounded stdio MCP transport and shutdown handling.
- `src/main.rs`: CLI, runtime lifecycle, and exit codes.
- `src/*/tests.rs`: module tests for parser, retrieval, policy, and service boundaries.
- `tests/library.rs`: public library contracts.
- `tests/interfaces.rs`: real CLI, REST, and MCP process tests.
- `tests/support/`: shared process and transport helpers.
- `tests/duckduckgo_live.rs`: ignored, opt-in probe through production `Scry::search` and `Scry::contents`, with one query and one returned page.

## Agent contract

### search

- `query`: required. Use 1 to 499 characters.
- `type`: `auto` or `keyword`. The default is `auto`. Both run ordinary keyword search. Reject all other values. Never describe keyword search as neural retrieval.
- `numResults`: 1 to 20. The default is 10.
- `includeDomains` and `excludeDomains`: hostnames or IP literals. The service applies these filters locally with hostname label boundaries, and it applies them again after redirects. It also adds include domains to the provider query as `site:` operators when the expanded query fits the limit. If the query does not fit, the service sends the base query and adds a warning.
- `startPublishedDate` and `endPublishedDate`: rejected as unsupported. Never invent publication dates or authors.
- `includeText`: retrieves each selected page and adds extracted text.
- `includeHighlights`: adds query-relevant excerpts. Highlights can use provider snippets without page retrieval. Text requires page retrieval.

### contents

- `urls` and `ids` are source URL lists. An ID is a normalized source URL, not an opaque provider ID.
- Supply at least one URL or one ID. The combined count is 1 to 20.
- The service removes duplicates and keeps the first position.
- An invalid URL becomes one item error. The rest of the batch continues.
- `includeText` defaults to true. If you do not request text or highlights, the service validates the URLs and does not retrieve them. A warning reports this behavior.

### results

- The envelope contains `results` and `warnings`.
- The output keeps the request order. Successful items stay in a batch when other items fail.
- `id` and `url` contain the normalized source URL. `finalUrl` appears after a redirect.
- `status` is `ok`, `partial`, or `error`.
- `contentOrigin` is `provider_snippet` or `live`.
- `snippet` contains provider text. `text` and `highlights` come from retrieved pages.
- `truncated` marks shortened or omitted content.
- `error` contains `code`, `message`, and optional `retryAfterSecs`.

Error codes: `invalid_input`, `unsupported`, `blocked_destination`, `domain_filtered`, `dns_failed`, `upstream_blocked`, `rate_limited`, `upstream_failed`, `timeout`, `cancelled`, `too_large`, `unsupported_content`, `extraction_failed`, `overloaded`, `internal`.

## Interfaces

### CLI

```text
scry search QUERY [--num-results N] [--type auto|keyword] [--include-domain D]... [--exclude-domain D]... [--include-text] [--include-highlights]
scry contents URL... [--id SOURCE_URL]... [--no-text] [--include-highlights]
scry serve [--bind IP:PORT] [--allow-host HOST[:PORT]]...
scry mcp
```

- `--timeout SECONDS` is global. Use 1 to 300 seconds. It sets the request deadline, not the stdio write deadline.
- `search` and `contents` write plain text to stdout by default. Operation errors and warnings go to stderr.
- `--json` is global. It selects JSON results on stdout and `ErrorResponse` operation errors on stderr. Warnings stay in the JSON response. Argument errors, help, and version output always use plain text. MCP stdout always uses JSON-RPC.
- Search and contents exit codes: 0 for success, 1 for failure (including all items failed), 2 for partial results or invalid/unsupported arguments, and 130 for an interrupt.
- Orderly shutdown of `serve` or `mcp` exits successfully. Transport failures use stderr diagnostics and a nonzero exit code.

### REST

- `POST /search` and `POST /contents` accept only `application/json`.
- `GET /health` returns the status and the version.
- `/mcp` serves MCP.
- The default bind is `127.0.0.1:8787`.
- The Host header must match the allowlist exactly. A bare host entry does not allow an explicit port. The service rejects every Origin header.
- `SCRY_AUTH_TOKEN` requires Bearer authentication on every route. A non-loopback bind requires this variable.
- Every response has `no-store` and `nosniff`.
- After an operation starts, HTTP 200 contains a `Response` or an `ErrorResponse`. Check the envelope and each item status. Input errors and access errors use HTTP status codes.
- Header reads, request bodies, and stalled writes have a limit of 10 seconds each. Shutdown drains for up to 5 seconds.

### MCP

- Both transports reuse the same `search` and `contents` handler and tool schemas. Request fields match REST.
- Both support protocol versions up to 2025-11-25 and propagate cancellation to the shared service.
- The service rejects `server/discover` and per-request protocol metadata as unsupported.
- The MCP response budget is three times the core response budget plus 1024 bytes.

#### Stdio

- `scry mcp` exchanges newline-delimited JSON-RPC on stdin and stdout. It starts no HTTP listener and requires no authentication token.
- Keep stdout exclusively for MCP messages. Send logs and transport diagnostics to stderr.
- Input frames are limited by `max_request_bytes`, which defaults to 64 KiB. Malformed, invalid UTF-8, and oversized frames stop the transport.
- At most 32 request IDs and 32 notifications can be pending. Duplicate active or cancelled request IDs are rejected.
- Completed requests release their ID slots. Cancelled IDs retain a slot until process exit.
- Every stdout send has a 10-second deadline, including initialization responses. A timeout cancels the session and reports a diagnostic on stderr.
- EOF, SIGINT, and SIGTERM stop the server. After cancellation, session cleanup allows one second and runtime shutdown allows 250 ms.
- Do not remove the bounded runtime shutdown. Tokio stdio workers can remain blocked when the client leaves a pipe open.

With `scry` on PATH, pi can launch it directly:

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

An absolute executable path can replace `scry`. Do not put machine-specific paths in repository configuration.

#### Streamable HTTP

- `scry serve` exposes MCP at `/mcp` with legacy session mode, SSE responses, and cancellation notifications.
- HTTP sessions, streams, and connections are each limited to 32.
- HTTP Host, Origin, and authentication rules apply to this transport, not to stdio.

## Retrieval safety

`src/policy.rs` and `src/fetch.rs` enforce these controls:

- Only HTTP and HTTPS with the standard port. No credentials, control characters, whitespace, or backslash.
- Blocked destinations include loopback, private, link-local, reserved, multicast, cloud metadata, tailnet, and special-use names and ranges. IPv6 equivalents are included.
- DNS uses system resolvers only. It ignores the hosts file and disables caching. The service validates the answers and pins them to a per-request client. The connection target cannot change after validation.
- The service validates every redirect again. It rejects HTTPS to HTTP downgrades and cross-origin form redirects. It limits the redirect count and requires one valid Location header.
- Only identity content encoding and HTML or XHTML content types are accepted. Character sets come from a BOM, the header, or a bounded meta sniff. Decoding is bounded. An incomplete response with a declared length is an error.
- Requests send no cookies, authorization, or proxy credentials. Page retrieval uses Reqwest, HTTP/1.1, connection close, and no Referer.
- Search requests to `https://html.duckduckgo.com` use the shared Reqwest/Rustls HTTP/1.1 client with a fixed Chrome 149 User-Agent, HTML Accept header, explicit English language header, and navigation headers with `Sec-Fetch-Site: none`. They send no Referer and do not emulate browser TLS or HTTP/2 behavior. These headers do not apply to `contents` or result-page retrieval. Redirects outside this origin use the ordinary page request headers. Both request paths disable automatic redirects, retries, proxies, and decompression. Provider responses use the same download and decoding limits.
- Upstream statuses map to explicit errors: 202, 401, and 403 to `upstream_blocked`, 429 to `rate_limited`, and other non-200 statuses to `upstream_failed`. Valid `Retry-After` headers on 202, 401, 403, 429, and 503 set `retryAfterSecs`.
- Retrieved content is untrusted data. The service never executes page scripts and never loads external resources.
- Provider requests have a minimum one-second interval between starts. Requests wait for this interval when their deadline permits. The provider lock remains held through challenge detection.
- Blocked and rate-limited provider responses start a 60-second cooldown. Each subsequent blocked recovery attempt doubles this delay, up to 900 seconds. This policy also applies to HTML challenges returned with HTTP 200. A longer upstream `Retry-After` takes precedence. A successful discovery resets the backoff. These delays are local policy, not published DuckDuckGo limits.
- An active failure cooldown rejects searches immediately with `rate_limited` and the remaining rounded-up seconds. The original failure keeps its error code and includes the effective `retryAfterSecs`. No automatic recovery request is sent. Cooldown state is shared by clones of one `Scry` instance, including REST and MCP sessions in one server. Separate instances, processes, and restarts do not share this state.

Page reading returns an explicit error for an access challenge, an access-denied screen, a login screen, an empty page, and an unsupported format. It never returns these cases as content.

## Limits

The defaults are in `src/config.rs`. `Limits::validate` rejects zero and enforces each maximum. The search workflow never sends more than 499 query characters to the provider.

| Limit | Default | Maximum |
| --- | --- | --- |
| Query characters | 499 | 16384 |
| URLs per request | 20 | 100 |
| Domain filters | 20 | 100 |
| URL bytes | 8192 | 65536 |
| Request body bytes | 65536 | 1048576 |
| Download bytes | 2097152 | 67108864 |
| Decoded bytes | 4194304 | 134217728 |
| Text characters | 50000 | 1000000 |
| Response bytes | 1048576 | 16777216 |
| Redirects | 5 | 20 |
| HTML tokens | 100000 | 2000000 |
| HTML depth | 128 | 1024 |
| Concurrent requests | 8 | 1024 |
| Concurrent fetches | 8 | 1024 |
| Concurrent parsers | 2 | 128 |
| Request timeout (s) | 30 | 300 |
| Fetch timeout (s) | 15 | 120 |
| Connect timeout (s) | 5 | 30 |
| DNS timeout (s) | 3 | 30 |

When a response exceeds the response limit, the service drops `text` fields first and marks them truncated. Then it trims highlights and snippets. It adds a warning. Finalization has a one-second budget. If the budget expires, the result is `too_large`.

## Checks

Run this command before you commit:

```sh
make check
```

The Makefile runs the formatting check, Clippy with warnings denied, and tests in sequence. It never runs the live probe by default.

The default suite covers public contracts, real interface processes, and module boundaries. It must not depend on the public web.

Use Tokio's paused clock for deadline assertions. The stdio write deadline tests use bounded in-memory streams through the production session loop. A separate Unix process test checks shutdown with a blocked OS stdout writer.

Assert actual behavior and semantic output. Do not generate expected output with the system under test. Use small inline HTML to isolate parser rules. These fixtures do not establish compatibility with the live provider. Do not fake upstream success or relax security policy for tests.

The live DuckDuckGo probe stays ignored unless explicitly requested:

```sh
make test-live
```

One successful request does not prove throughput, availability, or search quality.

## Working agreements

- Keep this AGENTS.md file up to date. It is the source of truth for the agent contract and the service design.
- Implement only the requested change. Prefer small direct modules over plugin frameworks and speculative abstractions.
- Keep provider parsing separate from page retrieval and extraction. Do not build a general provider framework.
- DuckDuckGo HTML is the current search backend. Keep one search backend. Ask before you change the source.
- Defer these items until a demonstrated need and approval exist:
  - browser automation and authenticated browsing
  - PDFs and media extraction
  - site crawls and background queues
  - persistent caching
  - search indexes, vector databases, embeddings, and LLM summaries
  - proxy rotation, CAPTCHA solvers, and hidden provider fallback
- Ask before you replace installed agent tools or change global agent configuration.
- Tailscale is the preferred access method. Keep the listener private, for example loopback behind Tailscale Serve.
- The deployment platform and capacity are not selected. Measure resource use before you size a host. Do not provision infrastructure without approval.
- Keep secrets, private infrastructure details, local operator paths, and operational records out of the repository.
