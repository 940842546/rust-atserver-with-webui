use async_trait::async_trait;
use axum::{
    body::Body,
    http::{header, StatusCode, Uri},
    response::{IntoResponse, Response, Redirect},
    routing::get,
    Router,
};
use futures_util::{SinkExt, StreamExt};
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, broadcast};
use tokio::time::{sleep, timeout, Instant, interval};
use tokio_serial::{SerialPortBuilderExt, SerialStream};
use tokio_tungstenite::tungstenite::Message;

#[derive(RustEmbed)]
#[folder = "web/"]
struct Asset;

const DEFAULT_CONFIG_JSON: &str = r#"{
    "AT_CONFIG": {
        "TYPE": "NETWORK",
        "NETWORK": { "HOST": "192.168.8.1", "PORT": 20249, "TIMEOUT": 30 },
        "SERIAL": { "PORT": "COM6", "BAUDRATE": 115200, "TIMEOUT": 30 }
    },
    "WEBSOCKET_CONFIG": {
        "IPV4": { "HOST": "0.0.0.0", "PORT": 8765 }
    },
    "HTTP_CONFIG": {
        "HOST": "0.0.0.0",
        "PORT": 8008
    }
}"#;

#[derive(Debug, Serialize, Deserialize, Clone)]
struct Config {
    #[serde(rename = "AT_CONFIG")] at_config: AtConfig,
    #[serde(rename = "WEBSOCKET_CONFIG")] websocket_config: WsConfig,
    #[serde(rename = "HTTP_CONFIG")] http_config: HttpConfig,
}
#[derive(Debug, Serialize, Deserialize, Clone)]
struct HttpConfig { #[serde(rename = "HOST")] host: String, #[serde(rename = "PORT")] port: u16 }
#[derive(Debug, Serialize, Deserialize, Clone)]
struct AtConfig { #[serde(rename = "TYPE")] conn_type: String, #[serde(rename = "NETWORK")] network: NetworkConfig, #[serde(rename = "SERIAL")] serial: SerialConfig }
#[derive(Debug, Serialize, Deserialize, Clone)]
struct NetworkConfig { #[serde(rename = "HOST")] host: String, #[serde(rename = "PORT")] port: u16, #[serde(rename = "TIMEOUT")] timeout: u64 }
#[derive(Debug, Serialize, Deserialize, Clone)]
struct SerialConfig { #[serde(rename = "PORT")] port: String, #[serde(rename = "BAUDRATE")] baudrate: u32, #[serde(rename = "TIMEOUT")] timeout: u64 }
#[derive(Debug, Serialize, Deserialize, Clone)]
struct WsConfig { #[serde(rename = "IPV4")] ipv4: WsEndpoint }
#[derive(Debug, Serialize, Deserialize, Clone)]
struct WsEndpoint { #[serde(rename = "HOST")] host: String, #[serde(rename = "PORT")] port: u16 }

#[async_trait]
trait ATConnection: Send {
    async fn connect(&mut self) -> Result<(), Box<dyn Error + Send + Sync>>;
    async fn send(&mut self, data: &[u8]) -> Result<usize, Box<dyn Error + Send + Sync>>;
    async fn receive(&mut self) -> Result<Vec<u8>, Box<dyn Error + Send + Sync>>;
    fn is_connected(&self) -> bool;
}

struct SerialATConn { config: SerialConfig, stream: Option<SerialStream> }
#[async_trait]
impl ATConnection for SerialATConn {
    async fn connect(&mut self) -> Result<(), Box<dyn Error + Send + Sync>> {
        let port = tokio_serial::new(&self.config.port, self.config.baudrate).timeout(Duration::from_secs(self.config.timeout)).open_native_async()?;
        self.stream = Some(port); Ok(())
    }
    async fn send(&mut self, data: &[u8]) -> Result<usize, Box<dyn Error + Send + Sync>> {
        if let Some(s) = &mut self.stream { return Ok(s.write(data).await?); }
        Err("Disconnected".into())
    }
    async fn receive(&mut self) -> Result<Vec<u8>, Box<dyn Error + Send + Sync>> {
        if let Some(s) = &mut self.stream {
            let mut buf = vec![0u8; 1024];
            let n = timeout(Duration::from_millis(25), s.read(&mut buf)).await??;
            buf.truncate(n); return Ok(buf);
        }
        Err("Disconnected".into())
    }
    fn is_connected(&self) -> bool { self.stream.is_some() }
}

struct NetworkATConn { config: NetworkConfig, stream: Option<TcpStream> }
#[async_trait]
impl ATConnection for NetworkATConn {
    async fn connect(&mut self) -> Result<(), Box<dyn Error + Send + Sync>> {
        let addr = format!("{}:{}", self.config.host, self.config.port);
        let stream = timeout(Duration::from_secs(self.config.timeout), TcpStream::connect(addr)).await??;
        self.stream = Some(stream); Ok(())
    }
    async fn send(&mut self, data: &[u8]) -> Result<usize, Box<dyn Error + Send + Sync>> {
        if let Some(s) = &mut self.stream { return Ok(s.write(data).await?); }
        Err("Disconnected".into())
    }
    async fn receive(&mut self) -> Result<Vec<u8>, Box<dyn Error + Send + Sync>> {
        if let Some(s) = &mut self.stream {
            let mut buf = vec![0u8; 1024];
            let n = timeout(Duration::from_millis(25), s.read(&mut buf)).await??;
            buf.truncate(n); return Ok(buf);
        }
        Err("Disconnected".into())
    }
    fn is_connected(&self) -> bool { self.stream.is_some() }
}

struct ATClient {
    conn: Arc<Mutex<Box<dyn ATConnection>>>,
    urc_tx: broadcast::Sender<String>,
}

impl ATClient {
    fn new(conn: Box<dyn ATConnection>) -> Self {
        let (tx, _) = broadcast::channel(1024);
        Self { conn: Arc::new(Mutex::new(conn)), urc_tx: tx }
    }

    async fn send_command(&self, mut command: String) -> Result<String, Box<dyn Error + Send + Sync>> {
        let mut conn = self.conn.lock().await;
        let original_cmd = command.trim().to_string();
        if !command.ends_with("\r\n") {
            command = command.trim_end().to_string();
            command.push_str("\r\n");
        }

        // 1. 清理旧残留，防止 ping 干扰指令结果
        while let Ok(d) = timeout(Duration::from_millis(10), conn.receive()).await.unwrap_or(Ok(vec![])) {
            if d.is_empty() { break; }
        }

        println!("[DEBUG] ==> TX: {:?}", command);
        conn.send(command.as_bytes()).await?;

        let mut raw_response = String::new();
        let start = Instant::now();

        // 2. 超时设为 1000ms
        while start.elapsed() < Duration::from_millis(1000) {
            if let Ok(data) = conn.receive().await {
                if !data.is_empty() {
                    raw_response.push_str(&String::from_utf8_lossy(&data));
                    // 如果看到 OK 或 ERROR，说明指令响应结束
                    if raw_response.contains("OK\r\n") || raw_response.contains("ERROR") {
                        break;
                    }
                }
            }
            sleep(Duration::from_millis(10)).await;
        }

        let mut cleaned = raw_response.replace("ping", "").trim().to_string();
        if cleaned.trim_start().starts_with(&original_cmd) {
            if let Some(pos) = cleaned.find('\n') {
                cleaned = cleaned[(pos + 1)..].to_string();
            }
        }

        let result = cleaned.trim().to_string();
        println!("[DEBUG] <== RX: {:?}", result);

        // 如果结果包含 ERROR，返回 Err 分支
        if result.contains("ERROR") {
            return Err("ERROR".into());
        }

        if result.is_empty() && start.elapsed() >= Duration::from_millis(1000) {
            return Err("TIMEOUT".into());
        }

        Ok(result)
    }

    async fn init_module(&self) {
        let _ = self.send_command("ATE0".into()).await;
        let _ = self.send_command("AT+CNMI=2,1,0,2,0".into()).await;
        let _ = self.send_command("AT+CMGF=0".into()).await;
        let _ = self.send_command("AT+CLIP=1".into()).await;
    }
}

async fn static_handler(uri: Uri) -> impl IntoResponse {
    let path = uri.path().trim_start_matches("/5700/").trim_start_matches('/');
    let target_path = if path.is_empty() || path.ends_with('/') { format!("{}index.html", path) } else { path.to_string() };
    match Asset::get(&target_path) {
        Some(content) => {
            let mime = mime_guess::from_path(&target_path).first_or_octet_stream();
            Response::builder().header(header::CONTENT_TYPE, mime.as_ref()).body(Body::from(content.data)).unwrap()
        }
        None => {
            if let Some(index) = Asset::get("index.html") { Response::builder().header(header::CONTENT_TYPE, "text/html").body(Body::from(index.data)).unwrap() }
            else { (StatusCode::NOT_FOUND, "404 Not Found").into_response() }
        }
    }
}

#[cfg(target_os = "windows")]
fn open_browser(port: u16) {
    let url = format!("http://127.0.0.1:{}", port);
    let _ = std::process::Command::new("cmd")
        .args(&["/C", "start", &url])
        .spawn();
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let config_path = "config.json";
    let config_str = if !Path::new(config_path).exists() {
        // 默认创建配置文件
        tokio::fs::write(config_path, DEFAULT_CONFIG_JSON).await?; DEFAULT_CONFIG_JSON.to_string()
        // // 默认不创建配置文件
        // DEFAULT_CONFIG_JSON.to_string()
    } else { tokio::fs::read_to_string(config_path).await? };
    let config: Config = serde_json::from_str(&config_str)?;

    let at_conn: Box<dyn ATConnection> = if config.at_config.conn_type == "NETWORK" {
        Box::new(NetworkATConn { config: config.at_config.network.clone(), stream: None })
    } else { Box::new(SerialATConn { config: config.at_config.serial.clone(), stream: None }) };

    let at_client = Arc::new(ATClient::new(at_conn));

    // 心跳任务：独立发送，不占锁进行读取循环
    let c_heartbeat = at_client.clone();
    tokio::spawn(async move {
        let mut heartbeat_timer = interval(Duration::from_secs(30));
        loop {
            heartbeat_timer.tick().await;
            {
                let mut conn = c_heartbeat.conn.lock().await;
                if conn.is_connected() {
                    let _ = conn.send(b"ping\r\n").await;
                }
            }
        }
    });

    // URC 捕获任务：持续输出控制台数据
    let c_monitor = at_client.clone();
    tokio::spawn(async move {
        loop {
            let mut has_data = false;
            {
                let mut conn = c_monitor.conn.lock().await;
                if !conn.is_connected() {
                    if let Ok(_) = conn.connect().await {
                        println!("Module Connected.");
                        drop(conn);
                        let c_init = c_monitor.clone();
                        tokio::spawn(async move { c_init.init_module().await });
                    }
                } else {
                    if let Ok(data) = conn.receive().await {
                        if !data.is_empty() {
                            has_data = true;
                            let text = String::from_utf8_lossy(&data).to_string();
                            for line in text.lines() {
                                let l = line.trim();
                                if !l.is_empty() && !l.to_lowercase().contains("ping") {
                                    if l.contains("^") || l.contains("+") {
                                        println!("[URC DETECTED] <== {:?}", line);
                                        let _ = c_monitor.urc_tx.send(line.to_string());
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if !has_data { sleep(Duration::from_millis(20)).await; }
        }
    });

    let ws_handler = |stream: TcpStream, _addr: std::net::SocketAddr, client: Arc<ATClient>| async move {
        let ws_stream = tokio_tungstenite::accept_async(stream).await.ok()?;
        let (mut ws_tx, mut ws_rx) = ws_stream.split();
        let mut urc_rx = client.urc_tx.subscribe();

        loop {
            tokio::select! {
                urc_res = urc_rx.recv() => {
                    if let Ok(msg) = urc_res {
                        let payload = serde_json::json!({ "type": "raw_data", "data": msg });
                        if let Ok(json_str) = serde_json::to_string(&payload) {
                            if let Err(_) = ws_tx.send(Message::Text(json_str)).await { break; }
                        }
                    }
                }
                msg = ws_rx.next() => {
                    if let Some(Ok(Message::Text(cmd))) = msg {
                        let res = match client.send_command(cmd).await {
                            Ok(r) => serde_json::json!({ "success": true, "data": r, "error": null }),
                            Err(e) => serde_json::json!({ "success": false, "data": null, "error": e.to_string() }),
                        };
                        let _ = ws_tx.send(Message::Text(serde_json::to_string(&res).unwrap())).await;
                    } else { break; }
                }
            }
        }
        Some(())
    };

    let app = Router::new().route("/", get(|| async { Redirect::permanent("/5700/") })).fallback(static_handler);
    let ws_v4_addr = format!("{}:{}", config.websocket_config.ipv4.host, config.websocket_config.ipv4.port);
    let http_addr = format!("{}:{}", config.http_config.host, config.http_config.port);
    let ws_l4 = TcpListener::bind(&ws_v4_addr).await?;
    let http_l = TcpListener::bind(&http_addr).await?;

    println!("--------------------------------------");
    println!("WebUI Server : http://{}", http_addr);
    println!("WebSocket IPv4: ws://{}", ws_v4_addr);
    println!("--------------------------------------");

    // // windows自动打开网页
    // #[cfg(target_os = "windows")]
    // open_browser(config.http_config.port);

    let c_v4 = at_client.clone();
    tokio::join!(
        async { while let Ok((s, a)) = ws_l4.accept().await { tokio::spawn(ws_handler(s, a, c_v4.clone())); } },
        async { axum::serve(http_l, app).await.unwrap(); }
    );
    Ok(())
}