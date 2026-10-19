use std::{
    io::{BufRead, BufReader, Read, Write},
    net::SocketAddr,
    process::{Child, Command, ExitStatus, Stdio},
    sync::mpsc::{self, Sender},
    thread::{self, JoinHandle},
    time::Duration,
};

use reqwest::{Client, RequestBuilder, Response};
use serde_json::{Value, json};
use tokio::{sync::mpsc as async_mpsc, time::timeout};

pub const LIMIT: usize = 256 * 1024;
pub const TOKEN: &str = "insecure-integration-test-token";
pub const VERSION: &str = "2025-11-25";

pub struct Process {
    pub child: Child,
    pub input: Option<Sender<Vec<u8>>>,
    pub output: async_mpsc::UnboundedReceiver<Vec<u8>>,
    pub errors: async_mpsc::UnboundedReceiver<Vec<u8>>,
    workers: Vec<JoinHandle<()>>,
}

fn reader(
    pipe: impl Read + Send + 'static,
) -> (async_mpsc::UnboundedReceiver<Vec<u8>>, JoinHandle<()>) {
    let (tx, rx) = async_mpsc::unbounded_channel();
    let worker = thread::spawn(move || {
        let mut reader = BufReader::new(pipe.take((LIMIT + 1) as u64));
        let mut total = 0;
        loop {
            let mut line = Vec::new();
            if reader.read_until(b'\n', &mut line).unwrap() == 0 {
                break;
            }
            total += line.len();
            assert!(total <= LIMIT, "process output exceeded {LIMIT} bytes");
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    (rx, worker)
}

impl Process {
    pub fn spawn(args: &[&str], token: Option<&str>, stdout: Option<Stdio>) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_scry"));
        command.args(args).env_remove("SCRY_AUTH_TOKEN");
        if let Some(token) = token {
            command.env("SCRY_AUTH_TOKEN", token);
        }
        let read_stdout = stdout.is_none();
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(stdout.unwrap_or_else(Stdio::piped))
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let writer = thread::spawn(move || {
            while let Ok(bytes) = rx.recv() {
                if stdin
                    .write_all(&bytes)
                    .and_then(|()| stdin.flush())
                    .is_err()
                {
                    break;
                }
            }
        });
        let (errors, error_worker) = reader(child.stderr.take().unwrap());
        let mut workers = vec![writer, error_worker];
        let output = if read_stdout {
            let (output, worker) = reader(child.stdout.take().unwrap());
            workers.push(worker);
            output
        } else {
            async_mpsc::unbounded_channel().1
        };
        Self {
            child,
            input: Some(tx),
            output,
            errors,
            workers,
        }
    }

    pub fn send(&self, value: Value) {
        let mut bytes = serde_json::to_vec(&value).unwrap();
        bytes.push(b'\n');
        self.input.as_ref().unwrap().send(bytes).unwrap();
    }

    pub async fn receive(&mut self, id: i64) -> Value {
        let bytes = timeout(Duration::from_secs(8), self.output.recv())
            .await
            .expect("MCP response deadline")
            .expect("MCP stdout closed");
        let value: Value =
            serde_json::from_slice(&bytes).expect("stdout must contain only JSON-RPC");
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["id"], id);
        assert!(value.get("error").is_none(), "{value}");
        value["result"].clone()
    }

    pub async fn wait(&mut self, seconds: u64) -> ExitStatus {
        let status = timeout(Duration::from_secs(seconds), async {
            loop {
                if let Some(status) = self.child.try_wait().unwrap() {
                    break status;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("child did not exit before deadline");
        self.input.take();
        let mut workers_ok = true;
        for worker in self.workers.drain(..) {
            workers_ok &= worker.join().is_ok();
        }
        assert!(workers_ok, "process I/O worker failed");
        status
    }

    pub fn remaining_errors(&mut self) -> Vec<u8> {
        let mut bytes = Vec::new();
        while let Ok(line) = self.errors.try_recv() {
            bytes.extend(line);
        }
        bytes
    }

    #[cfg(unix)]
    pub fn terminate(&self) {
        rustix::process::kill_process(
            rustix::process::Pid::from_raw(self.child.id() as i32).unwrap(),
            rustix::process::Signal::TERM,
        )
        .unwrap();
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.input.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

pub fn initialize() -> Value {
    json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":VERSION,"capabilities":{},"clientInfo":{"name":"independent-process-test","version":"1"}}})
}

pub fn initialized() -> Value {
    json!({"jsonrpc":"2.0","method":"notifications/initialized"})
}

pub fn rpc(id: i64, method: &str, params: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
}

pub fn assert_handshake(result: &Value) {
    assert_eq!(result["protocolVersion"], VERSION);
    assert_eq!(result["serverInfo"]["name"], "scry");
    assert_eq!(result["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));
    assert!(result["capabilities"]["tools"].is_object());
}

pub async fn stdio() -> Process {
    let mut process = Process::spawn(&["mcp"], None, None);
    process.send(initialize());
    assert_handshake(&process.receive(1).await);
    process.send(initialized());
    process
}

pub struct Server {
    pub process: Process,
    pub client: Client,
    pub address: SocketAddr,
}

impl Server {
    pub async fn start(token: Option<&str>) -> Self {
        let mut process = Process::spawn(
            &[
                "serve",
                "--bind",
                "127.0.0.1:0",
                "--allow-host",
                "allowed.test",
            ],
            token,
            None,
        );
        let line = timeout(Duration::from_secs(8), process.errors.recv())
            .await
            .expect("server startup deadline")
            .expect("server exited before startup");
        let line = std::str::from_utf8(&line).unwrap();
        let address = line
            .strip_prefix("scry listening on ")
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let client = Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(8))
            .build()
            .unwrap();
        Self {
            process,
            client,
            address,
        }
    }

    pub fn request(&self, method: reqwest::Method, path: &str) -> RequestBuilder {
        self.client
            .request(method, format!("http://{}{path}", self.address))
    }

    pub fn mcp(&self, session: Option<&str>, message: &Value) -> RequestBuilder {
        let request = self
            .request(reqwest::Method::POST, "/mcp")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", VERSION)
            .json(message);
        match session {
            Some(id) => request.header("mcp-session-id", id),
            None => request,
        }
    }

    #[cfg(unix)]
    pub async fn stop(&mut self) {
        self.process.terminate();
        assert!(self.process.wait(8).await.success());
        assert!(self.process.remaining_errors().is_empty());
        assert!(self.process.output.try_recv().is_err());
    }
}

pub async fn body(mut response: Response) -> Vec<u8> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.unwrap() {
        assert!(
            bytes.len() + chunk.len() <= LIMIT,
            "HTTP output exceeded limit"
        );
        bytes.extend_from_slice(&chunk);
    }
    bytes
}

pub async fn json_body(response: Response) -> Value {
    serde_json::from_slice(&body(response).await).unwrap()
}

pub async fn sse(mut response: Response, id: i64) -> Value {
    assert_eq!(response.status(), 200);
    assert!(
        response.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    let mut buffer = Vec::new();
    let mut total = 0;
    while let Some(chunk) = response.chunk().await.unwrap() {
        total += chunk.len();
        assert!(total <= LIMIT, "SSE output exceeded limit");
        buffer.extend_from_slice(&chunk);
        loop {
            let boundary = buffer
                .windows(2)
                .position(|w| w == b"\n\n")
                .map(|i| i + 2)
                .or_else(|| {
                    buffer
                        .windows(4)
                        .position(|w| w == b"\r\n\r\n")
                        .map(|i| i + 4)
                });
            let Some(end) = boundary else { break };
            let event = String::from_utf8(buffer.drain(..end).collect()).unwrap();
            let data = event
                .lines()
                .filter_map(|line| {
                    line.strip_prefix("data:")
                        .map(|s| s.strip_prefix(' ').unwrap_or(s))
                })
                .collect::<Vec<_>>()
                .join("\n");
            if data.is_empty() {
                continue;
            }
            let message: Value = serde_json::from_str(&data).unwrap();
            assert_eq!(message["jsonrpc"], "2.0");
            assert_eq!(message["id"], id);
            assert!(message.get("error").is_none(), "{message}");
            return message["result"].clone();
        }
    }
    panic!("SSE ended without response {id}");
}
