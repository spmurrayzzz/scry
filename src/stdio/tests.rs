use std::task::Poll;

use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, duplex},
    time::{Instant, advance},
};

use super::*;

async fn assert_unread_output_deadline(request: Value) {
    let (mut client_input, input) = duplex(1024);
    let (output, mut client_output) = duplex(1);
    let mut frame = serde_json::to_vec(&request).unwrap();
    frame.push(b'\n');
    client_input.write_all(&frame).await.unwrap();
    let shutdown = CancellationToken::new();
    let mut session = Box::pin(serve_io(
        Scry::new(scry::Config::default()).unwrap(),
        shutdown.clone(),
        input,
        output,
    ));
    let started = Instant::now();
    assert!(futures_util::poll!(session.as_mut()).is_pending());
    advance(Duration::from_secs(9)).await;
    assert!(futures_util::poll!(session.as_mut()).is_pending());
    advance(Duration::from_millis(999)).await;
    assert!(futures_util::poll!(session.as_mut()).is_pending());
    advance(Duration::from_millis(1)).await;
    let Poll::Ready(result) = futures_util::poll!(session.as_mut()) else {
        panic!("MCP session did not stop at the write deadline");
    };
    let error = result.unwrap_err();
    assert_eq!(error.code, ErrorCode::Timeout);
    assert_eq!(
        error.message,
        "MCP stdout write deadline exceeded (10 seconds)"
    );
    assert_eq!(Instant::now() - started, Duration::from_secs(10));
    assert!(!shutdown.is_cancelled());
    let mut written = Vec::new();
    assert!(matches!(
        futures_util::poll!(std::pin::pin!(client_output.read_to_end(&mut written))),
        Poll::Ready(Ok(1))
    ));
    assert_eq!(written, b"{");
    let mut diagnostics = Vec::new();
    assert_eq!(
        crate::fail_operation(error, false, &mut diagnostics),
        std::process::ExitCode::from(1)
    );
    assert_eq!(
        diagnostics,
        b"error: MCP stdout write deadline exceeded (10 seconds)\n"
    );
}

#[tokio::test(start_paused = true)]
async fn unread_output_times_out_during_preinitialization_pings() {
    assert_unread_output_deadline(json!({"jsonrpc":"2.0","id":1,"method":"ping"})).await;
}

#[tokio::test(start_paused = true)]
async fn unread_output_times_out_during_initialization() {
    assert_unread_output_deadline(json!({
        "jsonrpc":"2.0",
        "id":1,
        "method":"initialize",
        "params":{
            "protocolVersion":"2025-11-25",
            "capabilities":{},
            "clientInfo":{"name":"write-deadline-test","version":"1"}
        }
    }))
    .await;
}
