mod support;

use std::time::{Duration, Instant};

use reqwest::Method;
use serde_json::{Value, json};
use support::*;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::timeout,
};

fn metadata_args() -> Value {
    json!({"urls":["https://EXAMPLE.com:443", "not-a-url", "https://example.com/b"], "ids":["https://example.com/", "https://example.com/c"], "includeText":false})
}

fn assert_metadata(value: &Value) {
    let items = value["results"].as_array().expect("results array");
    assert_eq!(items.len(), 4);
    for (index, url) in [
        "https://example.com/",
        "not-a-url",
        "https://example.com/b",
        "https://example.com/c",
    ]
    .iter()
    .enumerate()
    {
        let item = &items[index];
        assert_eq!(item["id"], *url);
        assert_eq!(item["url"], *url);
        assert_eq!(item["status"], if index == 1 { "error" } else { "ok" });
        assert_eq!(item["truncated"], false);
        assert_eq!(item["highlights"], json!([]));
        for absent in ["text", "snippet", "finalUrl", "contentOrigin"] {
            assert!(item.get(absent).is_none(), "unexpected {absent}: {item}");
        }
        if index == 1 {
            assert_error(item, "invalid_input");
        } else {
            assert!(item.get("error").is_none());
        }
    }
    assert!(
        value["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("not fetched"))
    );
}

fn assert_error(value: &Value, code: &str) {
    assert_eq!(value["error"]["code"], code, "{value}");
    assert!(!value["error"]["message"].as_str().unwrap().is_empty());
    assert!(value["error"].get("retryAfterSecs").is_none());
}

fn assert_tools(result: &Value) {
    let tools = result["tools"].as_array().unwrap();
    let mut names: Vec<_> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    names.sort_unstable();
    assert_eq!(names, ["contents", "search"]);
    for tool in tools {
        let schema = &tool["inputSchema"];
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["additionalProperties"], false);
        let properties = &schema["properties"];
        if tool["name"] == "search" {
            assert_eq!(schema["required"], json!(["query"]));
            assert_eq!(properties["query"]["type"], "string");
            assert_eq!(properties["type"]["type"], "string");
            assert_eq!(properties["type"]["default"], "auto");
            assert_eq!(properties["numResults"]["type"], "integer");
            assert_eq!(properties["numResults"]["default"], 10);
            for field in ["includeDomains", "excludeDomains"] {
                assert_eq!(properties[field]["type"], "array");
                assert_eq!(properties[field]["items"]["type"], "string");
                assert_eq!(properties[field]["default"], json!([]));
            }
            for field in ["startPublishedDate", "endPublishedDate"] {
                assert!(properties.get(field).is_some());
            }
        } else {
            for field in ["urls", "ids"] {
                assert_eq!(properties[field]["type"], "array");
                assert_eq!(properties[field]["items"]["type"], "string");
                assert_eq!(properties[field]["default"], json!([]));
            }
        }
        assert_eq!(
            properties["includeText"]["default"],
            tool["name"] == "contents"
        );
        assert_eq!(properties["includeHighlights"]["default"], false);
    }
}

async fn cli(args: &[&str], exit: i32, error: Option<&str>) -> Value {
    let mut command_args = vec!["--json"];
    command_args.extend_from_slice(args);
    let mut process = Process::spawn(&command_args, None, true);
    process.input.take();
    assert_eq!(process.wait(8).await.code(), Some(exit), "{args:?}");
    let errors = process.remaining_errors();
    let mut output = Vec::new();
    while let Ok(line) = process.output.try_recv() {
        output.extend(line);
    }
    match error {
        Some(code) => {
            assert!(output.is_empty(), "CLI errors must not use stdout");
            let value = serde_json::from_slice(&errors).unwrap();
            assert_error(&value, code);
            value
        }
        None => {
            assert!(errors.is_empty(), "{errors:?}");
            serde_json::from_slice(&output).unwrap()
        }
    }
}

#[tokio::test]
async fn cli_metadata_success_partial_and_all_failed_exit_codes() {
    let value = cli(
        &[
            "contents",
            "https://EXAMPLE.com:443",
            "--id",
            "https://example.com/",
            "--no-text",
        ],
        0,
        None,
    )
    .await;
    assert_eq!(
        value["results"],
        json!([{"id":"https://example.com/","url":"https://example.com/","status":"ok","highlights":[],"truncated":false}])
    );
    assert!(!value["warnings"].as_array().unwrap().is_empty());
    let mixed = cli(
        &[
            "contents",
            "https://EXAMPLE.com:443",
            "not-a-url",
            "https://example.com/b",
            "--id",
            "https://example.com/",
            "--id",
            "https://example.com/c",
            "--no-text",
        ],
        2,
        None,
    )
    .await;
    assert_metadata(&mixed);
    let failed = cli(&["contents", "not-a-url", "--no-text"], 1, None).await;
    assert_eq!(failed["results"].as_array().unwrap().len(), 1);
    assert_eq!(failed["results"][0]["status"], "error");
    assert_error(&failed["results"][0], "invalid_input");
}

#[tokio::test]
async fn cli_invalid_search_uses_json_only_for_operation_errors() {
    cli(
        &["search", "unused", "--type", "neural"],
        2,
        Some("unsupported"),
    )
    .await;
    cli(&["search", ""], 2, Some("invalid_input")).await;
    cli(&["search", &"x".repeat(500)], 2, Some("invalid_input")).await;
    let mut process = Process::spawn(
        &["--json", "search", "unused", "--num-results", "0"],
        None,
        true,
    );
    process.input.take();
    assert_eq!(process.wait(8).await.code(), Some(2));
    let errors = String::from_utf8(process.remaining_errors()).unwrap();
    assert!(errors.starts_with("error: invalid value '0'"), "{errors}");
    assert!(errors.contains("--help"), "{errors}");
    assert!(process.output.try_recv().is_err());
}

#[tokio::test]
async fn stdio_raw_protocol_tools_contents_errors_and_clean_eof() {
    let mut process = stdio().await;
    process.send(rpc(2, "tools/list", json!({})));
    assert_tools(&process.receive(2).await);
    process.send(rpc(
        3,
        "tools/call",
        json!({"name":"contents","arguments":metadata_args()}),
    ));
    let result = process.receive(3).await;
    assert_eq!(result["isError"], false);
    assert_metadata(&result["structuredContent"]);
    let text: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_metadata(&text);
    for (id, arguments, code) in [
        (4, json!({"urls":[],"includeText":false}), "invalid_input"),
        (
            5,
            json!({"urls":["https://example.com"],"includeText":false,"typo":true}),
            "invalid_input",
        ),
    ] {
        process.send(rpc(
            id,
            "tools/call",
            json!({"name":"contents","arguments":arguments}),
        ));
        let result = process.receive(id).await;
        assert_eq!(result["isError"], true);
        assert_error(&result["structuredContent"], code);
    }
    process.input.take();
    assert!(process.wait(8).await.success());
    assert!(process.remaining_errors().is_empty());
    assert!(
        process.output.try_recv().is_err(),
        "unexpected stdout after EOF"
    );
}

#[tokio::test]
async fn stdio_rejects_malformed_utf8_and_oversized_frames() {
    for (frame, message) in [
        (b"{broken}\n".to_vec(), "Malformed MCP input frame"),
        (vec![0xff, b'\n'], "Cannot read a UTF-8 MCP input frame"),
        (
            vec![b'x'; 65538],
            "MCP input frame exceeds max_request_bytes",
        ),
    ] {
        let mut process = stdio().await;
        process.input.as_ref().unwrap().send(frame).unwrap();
        assert!(!process.wait(8).await.success());
        assert_eq!(
            String::from_utf8(process.remaining_errors()).unwrap(),
            format!("error: {message}\n"),
        );
        assert!(process.output.try_recv().is_err());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn stdio_sigterm_exits_with_stdin_still_open() {
    let mut process = stdio().await;
    assert!(process.input.is_some());
    process.terminate();
    assert!(process.wait(8).await.success());
    assert!(process.remaining_errors().is_empty());
    assert!(process.output.try_recv().is_err());
}

#[tokio::test]
async fn stdio_unread_os_pipe_times_out_during_preinitialization_pings() {
    let mut process = Process::spawn(&["mcp"], None, false);
    #[cfg(target_os = "linux")]
    let pipe_capacity =
        rustix::pipe::fcntl_getpipe_size(process.child.stdout.as_ref().unwrap()).unwrap();
    #[cfg(not(target_os = "linux"))]
    let pipe_capacity = 2 * 1024 * 1024;
    let response_bytes = pipe_capacity + 2 * 1024 * 1024;
    let start = Instant::now();
    let mut frames = Vec::new();
    for id in 0..(response_bytes / 400 + 1) {
        serde_json::to_writer(
            &mut frames,
            &json!({"jsonrpc":"2.0","id":format!("{id:0400}"),"method":"ping"}),
        )
        .unwrap();
        frames.push(b'\n');
    }
    process.input.as_ref().unwrap().send(frames).unwrap();
    assert_eq!(process.wait(20).await.code(), Some(1));
    assert!(
        start.elapsed() >= Duration::from_secs(10),
        "did not exercise the production write deadline"
    );
    assert_eq!(
        String::from_utf8(process.remaining_errors()).unwrap(),
        "error: MCP stdout write deadline exceeded (10 seconds)\n",
    );
}

#[tokio::test]
async fn http_rest_metadata_validation_body_limits_and_recovery() {
    let server = Server::start(None).await;
    let response = server
        .request(Method::POST, "/contents")
        .json(&metadata_args())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    assert_metadata(&json_body(response).await);
    for (path, payload, status, code) in [
        ("/search", json!({"query":""}), 200, "invalid_input"),
        (
            "/search",
            json!({"query":"unused","type":"neural"}),
            200,
            "unsupported",
        ),
        (
            "/contents",
            json!({"includeText":false,"typo":true}),
            400,
            "invalid_input",
        ),
    ] {
        let response = server
            .request(Method::POST, path)
            .json(&payload)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), status);
        assert_error(&json_body(response).await, code);
    }
    for (payload, content_type, status, code) in [
        ("{".to_owned(), "application/json", 400, "invalid_input"),
        ("{}".to_owned(), "text/plain", 415, "invalid_input"),
        (" ".repeat(65537), "application/json", 413, "too_large"),
    ] {
        let response = server
            .request(Method::POST, "/contents")
            .header("content-type", content_type)
            .body(payload)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), status);
        assert_error(&json_body(response).await, code);
    }
    let response = timeout(Duration::from_secs(8), async {
        let mut socket = TcpStream::connect(server.address).await.unwrap();
        let payload = " ".repeat(65537);
        let request = format!(
            "POST /contents HTTP/1.1\r\nHost: {}\r\n\
             Content-Type: application/json\r\nTransfer-Encoding: chunked\r\n\
             Connection: close\r\n\r\n{:x}\r\n{payload}",
            server.address,
            payload.len()
        );
        socket.write_all(request.as_bytes()).await.unwrap();
        let mut response = String::new();
        socket
            .take((LIMIT + 1) as u64)
            .read_to_string(&mut response)
            .await
            .unwrap();
        assert!(response.len() <= LIMIT, "HTTP output exceeded limit");
        response
    })
    .await
    .expect("chunked rejection deadline");
    let (headers, body) = response.split_once("\r\n\r\n").unwrap();
    assert!(headers.starts_with("HTTP/1.1 413 "), "{headers}");
    assert_error(&serde_json::from_str(body).unwrap(), "too_large");
    let response = server.request(Method::GET, "/health").send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        json_body(response).await,
        json!({"status":"ok","version":env!("CARGO_PKG_VERSION")})
    );
}

#[tokio::test]
async fn http_auth_host_and_origin_enforced_on_all_routes() {
    let server = Server::start(Some(TOKEN)).await;
    for (method, path) in [
        (Method::GET, "/health"),
        (Method::POST, "/search"),
        (Method::POST, "/contents"),
        (Method::POST, "/mcp"),
        (Method::GET, "/mcp"),
        (Method::DELETE, "/mcp"),
        (Method::GET, "/missing"),
    ] {
        for token in [None, Some("wrong-insecure-test-token")] {
            let mut request = server.request(method.clone(), path);
            if let Some(token) = token {
                request = request.bearer_auth(token);
            }
            let response = request.send().await.unwrap();
            assert_eq!(response.status(), 401, "{method} {path}");
            assert_eq!(response.headers()["www-authenticate"], "Bearer");
            assert_error(&json_body(response).await, "invalid_input");
        }
        for (header, value) in [
            ("origin", "http://allowed.test"),
            ("host", "not-allowed.test"),
        ] {
            let response = server
                .request(method.clone(), path)
                .bearer_auth(TOKEN)
                .header(header, value)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 403, "{method} {path}: {header}");
            assert_error(&json_body(response).await, "invalid_input");
        }
    }
    for (host, status) in [
        ("allowed.test".to_owned(), 200),
        (format!("allowed.test:{}", server.address.port()), 403),
        ("not-allowed.test".to_owned(), 403),
    ] {
        let response = server
            .request(Method::GET, "/health")
            .bearer_auth(TOKEN)
            .header("host", host)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), status);
        let value = json_body(response).await;
        if status == 200 {
            assert_eq!(value["status"], "ok");
        } else {
            assert_error(&value, "invalid_input");
        }
    }
    let response = server
        .request(Method::POST, "/contents")
        .bearer_auth(TOKEN)
        .json(&metadata_args())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_metadata(&json_body(response).await);
}

#[tokio::test]
async fn http_raw_mcp_sse_session_lifecycle_and_semantic_results() {
    let server = Server::start(Some(TOKEN)).await;
    let response = server
        .mcp(None, &initialize())
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    let session = response
        .headers()
        .get("mcp-session-id")
        .expect("session ID on initialize")
        .to_str()
        .unwrap()
        .to_owned();
    assert!(!session.is_empty());
    assert_handshake(&sse(response, 1).await);
    let response = server
        .mcp(Some(&session), &initialized())
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 202);
    assert!(body(response).await.is_empty());
    let response = server
        .mcp(Some(&session), &rpc(2, "tools/list", json!({})))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_tools(&sse(response, 2).await);
    for (id, args, error) in [
        (3, metadata_args(), false),
        (4, json!({"includeText":false}), true),
    ] {
        let response = server
            .mcp(
                Some(&session),
                &rpc(
                    id,
                    "tools/call",
                    json!({"name":"contents","arguments":args}),
                ),
            )
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap();
        let result = sse(response, id).await;
        if error {
            assert_eq!(result["isError"], true);
            assert_error(&result["structuredContent"], "invalid_input");
        } else {
            assert_eq!(result["isError"], false);
            assert_metadata(&result["structuredContent"]);
        }
    }
    let response = server
        .request(Method::DELETE, "/mcp")
        .bearer_auth(TOKEN)
        .header("mcp-session-id", &session)
        .header("mcp-protocol-version", VERSION)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 202);
    body(response).await;
    for id in [&session, "unknown-test-session"] {
        let response = server
            .mcp(Some(id), &rpc(5, "tools/list", json!({})))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            404,
            "closed or unknown session must not be recreated"
        );
        body(response).await;
    }
    #[cfg(unix)]
    {
        let mut server = server;
        server.stop().await;
    }
}
