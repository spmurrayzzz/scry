mod http;
mod mcp;

use std::{io::Write, net::SocketAddr, process::ExitCode, time::Duration};

use clap::{Args, Parser, Subcommand};
use scry::{
    Config, ContentsRequest, Error, ErrorCode, ErrorResponse, ItemStatus, Response, Scry,
    SearchRequest,
};
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(
    name = "scry",
    version,
    about = "Search the web and read public HTML pages",
    after_help = "Output: JSON on stdout, structured errors on stderr. Exit codes: 0 success, 1 failure (including all URLs failed), 2 partial results or invalid/unsupported arguments, 130 interrupted. DuckDuckGo access is unofficial and can fail or be blocked."
)]
struct Cli {
    #[arg(
        long,
        global = true,
        value_name = "SECONDS",
        value_parser = clap::value_parser!(u64).range(1..=300),
        help = "Request deadline, from 1 to 300 seconds"
    )]
    timeout: Option<u64>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    #[command(about = "Search DuckDuckGo; auto and keyword use ordinary keyword search")]
    Search(SearchArgs),
    #[command(about = "Read public URLs directly; IDs must also be source URLs")]
    Contents(ContentsArgs),
    #[command(
        about = "Serve REST and session-based Streamable HTTP MCP",
        after_help = "Routes: POST /search, POST /contents, GET /health, /mcp. Set SCRY_AUTH_TOKEN to require Bearer authentication on every route. A non-loopback bind requires this environment variable. Host values must match the allowlist exactly. A bare host does not allow any explicit port. Every Origin header is rejected. REST operations use body-owned futures so disconnects cancel work. Once a REST operation starts, HTTP 200 contains either a Response or an ErrorResponse {\"error\":{\"code\":...,\"message\":...}}. Check the envelope and each result status. Input and access errors use HTTP error codes. MCP supports GET/SSE and cancellation notifications through protocol 2025-11-25. Connections and MCP sessions are each limited to 32. Headers, request bodies, and stalled writes have 10-second limits. Shutdown allows a 5-second drain. Core JSON responses are limited to 1 MiB; MCP text plus structured content allows up to three times that budget plus 1024 bytes. Use a trusted reverse proxy for TLS."
    )]
    Serve(ServeArgs),
}

#[derive(Args)]
struct SearchArgs {
    #[arg(value_name = "QUERY")]
    query: String,
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u8).range(1..=20))]
    num_results: u8,
    #[arg(
        long = "type",
        default_value = "auto",
        help = "Search mode: auto or keyword; other modes are rejected"
    )]
    search_type: String,
    #[arg(long = "include-domain", value_name = "DOMAIN", action = clap::ArgAction::Append)]
    include_domains: Vec<String>,
    #[arg(long = "exclude-domain", value_name = "DOMAIN", action = clap::ArgAction::Append)]
    exclude_domains: Vec<String>,
    #[arg(long)]
    include_text: bool,
    #[arg(long)]
    include_highlights: bool,
    #[arg(long, help = "Strict publication-date filter; currently unsupported")]
    start_published_date: Option<String>,
    #[arg(long, help = "Strict publication-date filter; currently unsupported")]
    end_published_date: Option<String>,
}

#[derive(Args)]
struct ContentsArgs {
    #[arg(value_name = "URL", required_unless_present = "ids", num_args = 1..)]
    urls: Vec<String>,
    #[arg(long = "id", value_name = "SOURCE_URL", action = clap::ArgAction::Append)]
    ids: Vec<String>,
    #[arg(long)]
    no_text: bool,
    #[arg(long)]
    include_highlights: bool,
}

#[derive(Args)]
struct ServeArgs {
    #[arg(long, default_value = "127.0.0.1:8787", value_name = "IP:PORT")]
    bind: SocketAddr,
    #[arg(long = "allow-host", value_name = "HOST[:PORT]", action = clap::ArgAction::Append, help = "Add an exact trusted HOST[:PORT] authority; a bare host allows no explicit port")]
    allow_hosts: Vec<String>,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) if !error.use_stderr() => {
            return match error.print() {
                Ok(()) => ExitCode::SUCCESS,
                Err(_) => fail(
                    Error::new(ErrorCode::Internal, "Cannot write help output"),
                    1,
                ),
            };
        }
        Err(error) => return fail(Error::new(ErrorCode::InvalidInput, error.to_string()), 2),
    };
    let mut config = Config::default();
    if let Some(timeout) = cli.timeout {
        config.limits.request_timeout_secs = timeout;
    }
    let service = match Scry::new(config) {
        Ok(service) => service,
        Err(error) => return fail(error, 1),
    };
    let shutdown = CancellationToken::new();
    let signal = shutdown.clone();
    let signal_task = tokio::spawn(async move {
        let result = shutdown_signal()
            .await
            .map_err(|_| Error::new(ErrorCode::Internal, "Cannot receive shutdown signals"));
        signal.cancel();
        result
    });
    let result = match cli.command {
        Command::Search(args) => {
            let request = SearchRequest {
                query: args.query,
                search_type: args.search_type,
                num_results: usize::from(args.num_results),
                include_domains: args.include_domains,
                exclude_domains: args.exclude_domains,
                start_published_date: args.start_published_date,
                end_published_date: args.end_published_date,
                include_text: args.include_text,
                include_highlights: args.include_highlights,
            };
            let ctx = request_context(&service, shutdown.child_token());
            let _guard = ctx.cancellation.clone().drop_guard();
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => Err(Error::cancelled()),
                result = service.search_with_context(request, ctx) => result.map(Some),
            }
        }
        Command::Contents(args) => {
            let request = ContentsRequest {
                urls: args.urls,
                ids: args.ids,
                include_text: !args.no_text,
                include_highlights: args.include_highlights,
            };
            let ctx = request_context(&service, shutdown.child_token());
            let _guard = ctx.cancellation.clone().drop_guard();
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => Err(Error::cancelled()),
                result = service.contents_with_context(request, ctx) => result.map(Some),
            }
        }
        Command::Serve(args) => http::serve(service, args.bind, args.allow_hosts, shutdown.clone())
            .await
            .map(|()| None),
    };
    signal_task.abort();
    if let Ok(Err(error)) = signal_task.await {
        return fail(error, 1);
    }
    match result {
        Ok(Some(response)) => {
            let code = if !response.results.is_empty()
                && response
                    .results
                    .iter()
                    .all(|item| item.status == ItemStatus::Error)
            {
                1
            } else if response
                .results
                .iter()
                .any(|item| item.status != ItemStatus::Ok)
            {
                2
            } else {
                0
            };
            match write_json(std::io::stdout().lock(), &response) {
                Ok(()) => ExitCode::from(code),
                Err(_) => fail(
                    Error::new(ErrorCode::Internal, "Cannot write JSON output"),
                    1,
                ),
            }
        }
        Ok(None) => ExitCode::SUCCESS,
        Err(error) => {
            let code = match error.code {
                ErrorCode::Cancelled => 130,
                ErrorCode::InvalidInput | ErrorCode::Unsupported => 2,
                _ => 1,
            };
            fail(error, code)
        }
    }
}

async fn shutdown_signal() -> Result<(), std::io::Error> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await
}

fn request_context(service: &Scry, token: CancellationToken) -> scry::RequestContext {
    scry::RequestContext::with_token(
        Duration::from_secs(service.config().limits.request_timeout_secs),
        token,
    )
}

fn write_json(mut writer: impl Write, value: &impl serde::Serialize) -> Result<(), std::io::Error> {
    serde_json::to_writer(&mut writer, value).map_err(std::io::Error::other)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

fn fail(error: Error, code: u8) -> ExitCode {
    let _ = write_json(std::io::stderr().lock(), &ErrorResponse { error });
    ExitCode::from(code)
}

fn response_bytes(result: Result<Response, Error>, maximum: usize) -> Vec<u8> {
    match result {
        Ok(response) => match serde_json::to_vec(&response) {
            Ok(bytes) if bytes.len() <= maximum => bytes,
            Ok(_) => error_bytes(Error::new(
                ErrorCode::TooLarge,
                "Response exceeds the output budget",
            )),
            Err(_) => error_bytes(Error::new(ErrorCode::Internal, "Cannot encode response")),
        },
        Err(error) => error_bytes(error),
    }
}

fn error_bytes(error: Error) -> Vec<u8> {
    serde_json::to_vec(&ErrorResponse { error })
        .expect("ErrorResponse contains only JSON-compatible fields")
}
