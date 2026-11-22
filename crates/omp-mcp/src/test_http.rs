//! Minimal in-process HTTP/1.1 server for unit tests (no external network).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub headers: BTreeMap<String, String>,
    pub body: String,
    pub base: String,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

pub struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Reply {
    pub fn status(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: String::new(),
        }
    }

    pub fn json(status: u16, value: Value) -> Self {
        Self::status(status)
            .header("content-type", "application/json")
            .body(value.to_string())
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    pub fn body(mut self, body: String) -> Self {
        self.body = body;
        self
    }
}

pub struct MockServer {
    pub base: String,
    requests: Arc<Mutex<Vec<Request>>>,
}

impl MockServer {
    pub async fn start<F>(handler: F) -> Self
    where
        F: Fn(&Request) -> Reply + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let handler = Arc::new(handler);
        let (log, base_inner) = (requests.clone(), base.clone());
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let (log, handler, base) = (log.clone(), handler.clone(), base_inner.clone());
                tokio::spawn(async move {
                    let Some(request) = read_request(&mut socket, base).await else {
                        return;
                    };
                    let reply = handler(&request);
                    log.lock().unwrap().push(request);
                    let mut out = format!(
                        "HTTP/1.1 {} X\r\ncontent-length: {}\r\nconnection: close\r\n",
                        reply.status,
                        reply.body.len()
                    );
                    for (name, value) in &reply.headers {
                        out.push_str(&format!("{name}: {value}\r\n"));
                    }
                    out.push_str("\r\n");
                    out.push_str(&reply.body);
                    let _ = socket.write_all(out.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        Self { base, requests }
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }

    pub fn paths(&self) -> Vec<String> {
        self.requests().into_iter().map(|r| r.path).collect()
    }
}

async fn read_request(socket: &mut tokio::net::TcpStream, base: String) -> Option<Request> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let n = socket.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let mut start = lines.next()?.split_whitespace();
    let method = start.next()?.to_string();
    let path = start.next()?.to_string();
    let headers: BTreeMap<String, String> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < length {
        let n = socket.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Some(Request {
        method,
        path,
        headers,
        body: String::from_utf8_lossy(&body).to_string(),
        base,
    })
}
