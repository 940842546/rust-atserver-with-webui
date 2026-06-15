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
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::{sleep, timeout, Instant};
use tokio_serial::{SerialPortBuilderExt, SerialStream};
use tokio_tungstenite::tungstenite::Message;

// ── 超时常量 ───────────────────────────────────────────────
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_millis(50);
const CMD_TIMEOUT: Duration = Duration::from_millis(3000);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
const MAX_HEARTBEAT_FAILURES: u32 = 3;
const RECONNECT_DELAY: Duration = Duration::from_secs(5);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

// ── 带时间戳的打印宏 ─────────────────────────────────────
macro_rules! log {
    ($($arg:tt)*) => {
        println!("[{}] {}", chrono::Local::now().format("%H:%M:%S%.3f"), format!($($arg)*))
    };
}
// ── 嵌入静态资源 ─────────────────────────────────────────
#[derive(RustEmbed)]
#[folder = "web/"]
struct Asset;

const DEFAULT_CONFIG_JSON: &str = r#"{
    "AT_CONFIG": {
        "TYPE": "NETWORK",
        "NETWORK": { "HOST": "192.168.8.1", "PORT": 20249, "TIMEOUT": 10 },
        "SERIAL": { "PORT": "COM6", "BAUDRATE": 115200, "TIMEOUT": 10 }
    },
    "WEBSOCKET_CONFIG": {
        "IPV4": { "HOST": "0.0.0.0", "PORT": 8765 }
    },
    "HTTP_CONFIG": {
        "HOST": "0.0.0.0",
        "PORT": 8008
    }
}"#;

// ── 配置结构体 ─────────────────────────────────────────────
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

// ── AT 连接抽象层 ──────────────────────────────────────────
#[async_trait]
trait ATConnection: Send {
    async fn connect(&mut self) -> Result<(), Box<dyn Error + Send + Sync>>;
    async fn send(&mut self, data: &[u8]) -> Result<usize, Box<dyn Error + Send + Sync>>;
    async fn receive(&mut self) -> Result<Vec<u8>, Box<dyn Error + Send + Sync>>;
    fn is_connected(&self) -> bool;
}

// ── 串口连接实现 ───────────────────────────────────────────
struct SerialATConn { config: SerialConfig, stream: Option<SerialStream> }
#[async_trait]
impl ATConnection for SerialATConn {
    async fn connect(&mut self) -> Result<(), Box<dyn Error + Send + Sync>> {
        let port = tokio_serial::new(&self.config.port, self.config.baudrate)
            .timeout(Duration::from_secs(self.config.timeout))
            .open_native_async()?;
        self.stream = Some(port);
        Ok(())
    }
    async fn send(&mut self, data: &[u8]) -> Result<usize, Box<dyn Error + Send + Sync>> {
        match &mut self.stream {
            Some(s) => Ok(timeout(WRITE_TIMEOUT, s.write(data)).await??),
            None => Err("Disconnected".into()),
        }
    }
    async fn receive(&mut self) -> Result<Vec<u8>, Box<dyn Error + Send + Sync>> {
        match &mut self.stream {
            Some(s) => {
                let mut buf = vec![0u8; 1024];
                match timeout(READ_TIMEOUT, s.read(&mut buf)).await {
                    Ok(Ok(n)) => { buf.truncate(n); Ok(buf) }
                    Ok(Err(e)) => Err(Box::new(e)),
                    Err(_) => Ok(vec![]), // 超时 = 无数据，不是错误
                }
            }
            None => Err("Disconnected".into()),
        }
    }
    fn is_connected(&self) -> bool { self.stream.is_some() }
}

// ── 网络连接实现 ───────────────────────────────────────────
struct NetworkATConn { config: NetworkConfig, stream: Option<TcpStream> }
#[async_trait]
impl ATConnection for NetworkATConn {
    async fn connect(&mut self) -> Result<(), Box<dyn Error + Send + Sync>> {
        let addr = format!("{}:{}", self.config.host, self.config.port);
        let stream = timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await??;
        self.stream = Some(stream);
        Ok(())
    }
    async fn send(&mut self, data: &[u8]) -> Result<usize, Box<dyn Error + Send + Sync>> {
        match &mut self.stream {
            Some(s) => Ok(timeout(WRITE_TIMEOUT, s.write(data)).await??),
            None => Err("Disconnected".into()),
        }
    }
    async fn receive(&mut self) -> Result<Vec<u8>, Box<dyn Error + Send + Sync>> {
        match &mut self.stream {
            Some(s) => {
                let mut buf = vec![0u8; 1024];
                match timeout(READ_TIMEOUT, s.read(&mut buf)).await {
                    Ok(Ok(n)) => { buf.truncate(n); Ok(buf) }
                    Ok(Err(e)) => Err(Box::new(e)),
                    Err(_) => Ok(vec![]), // 超时 = 无数据，不是错误
                }
            }
            None => Err("Disconnected".into()),
        }
    }
    fn is_connected(&self) -> bool { self.stream.is_some() }
}

// ── 命令通道类型 ───────────────────────────────────────────
struct CmdRequest {
    command: String,
    reply_tx: oneshot::Sender<Result<String, String>>,
}

// ── ConnectionActor：独占 AT 连接 ──────────────────────────
struct ConnectionActor {
    conn: Box<dyn ATConnection>,
    conn_config: AtConfig,   // 保存配置用于断连后重建
    urc_tx: broadcast::Sender<String>,
    cmd_rx: mpsc::UnboundedReceiver<CmdRequest>,
    heartbeat_failures: u32,
}

impl ConnectionActor {
    /// Actor 主循环
    async fn run(mut self) {
        let mut last_heartbeat = Instant::now();
        loop {
            // ── 断连重连 ──────────────────────────────────
            if !self.conn.is_connected() {
                log!("[ACTOR] Connecting...");
                match timeout(CONNECT_TIMEOUT, self.conn.connect()).await {
                    Ok(Ok(())) => {
                        log!("[ACTOR] Module Connected.");
                        self.heartbeat_failures = 0;
                        last_heartbeat = Instant::now();
                        self.init_module().await;
                    }
                    Ok(Err(e)) => {
                        log!("[ACTOR] Connect failed: {}, retrying in {:?}...", e, RECONNECT_DELAY);
                        sleep(RECONNECT_DELAY).await;
                        continue;
                    }
                    Err(_) => {
                        log!("[ACTOR] Connect timed out, retrying in {:?}...", RECONNECT_DELAY);
                        sleep(RECONNECT_DELAY).await;
                        continue;
                    }
                }
            }

            // ── 处理所有待处理命令 ─────────────────────────
            loop {
                match self.cmd_rx.try_recv() {
                    Ok(req) => {
                        let result = self.send_command(req.command).await;
                        let _ = req.reply_tx.send(result);
                        // 继续处理下一个命令
                    }
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        log!("[ACTOR] Command channel closed, shutting down.");
                        return;
                    }
                }
            }

            // ── 读取 URC 数据 ─────────────────────────────
            match self.conn.receive().await {
                Ok(data) if !data.is_empty() => {
                    let text = String::from_utf8_lossy(&data).to_string();
                    for line in text.lines() {
                        let l = line.trim();
                        if !l.is_empty() && !l.to_lowercase().contains("ping") {
                            if l.contains('^') || l.contains('+') {
                                log!("[URC] <== {:?}", line);
                                let _ = self.urc_tx.send(line.to_string());
                            }
                        }
                    }
                }
                Err(e) => {
                    log!("[ACTOR] Read error: {}, disconnecting...", e);
                    self.conn = self.create_replacement_conn();
                    sleep(RECONNECT_DELAY).await;
                    continue;
                }
                _ => {
                    // 无数据（超时返回空Vec） — 继续
                }
            }

            // ── 心跳检查（基于时间间隔，不阻塞循环） ──────
            if self.heartbeat_failures >= MAX_HEARTBEAT_FAILURES {
                log!("[ACTOR] Heartbeat lost ({} failures), disconnecting...", self.heartbeat_failures);
                self.conn = self.create_replacement_conn();
                last_heartbeat = Instant::now();
                sleep(RECONNECT_DELAY).await;
                continue;
            }

            if last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL && self.conn.is_connected() {
                match self.do_heartbeat().await {
                    Ok(()) => {
                        self.heartbeat_failures = 0;
                    }
                    Err(e) => {
                        self.heartbeat_failures += 1;
                        log!("[ACTOR] Heartbeat failed ({}/{}): {}",
                            self.heartbeat_failures, MAX_HEARTBEAT_FAILURES, e);
                    }
                }
                last_heartbeat = Instant::now();
            }
        }
    }

    /// 执行一次心跳：发送 AT\r\n，预期收到 OK\r\n
    async fn do_heartbeat(&mut self) -> Result<(), String> {
        // 清理残留数据
        self.drain_buffer().await;

        // 发送 AT\r\n
        timeout(WRITE_TIMEOUT, self.conn.send(b"AT\r\n"))
            .await
            .map_err(|_| "send timeout".to_string())?
            .map_err(|e| format!("send error: {}", e))?;

        // 等待 OK\r\n 或 ERROR（最多 3 秒）
        let mut response = String::new();
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(3) {
            match timeout(READ_TIMEOUT, self.conn.receive()).await {
                Ok(Ok(data)) if !data.is_empty() => {
                    response.push_str(&String::from_utf8_lossy(&data));
                    if response.contains("OK\r\n") {
                        return Ok(());
                    }
                    if response.contains("ERROR") {
                        return Err("AT returned ERROR".into());
                    }
                }
                Ok(Err(e)) => return Err(format!("read error: {}", e)),
                _ => {} // 超时，继续等待
            }
        }
        Err("heartbeat response timeout".into())
    }

    /// 发送 AT 指令并等待响应（Actor 内部使用，独占连接）
    async fn send_command(&mut self, mut command: String) -> Result<String, String> {
        let original_cmd = command.trim().to_string();
        if !command.ends_with("\r\n") {
            command = command.trim_end().to_string();
            command.push_str("\r\n");
        }

        // 清理残留数据
        self.drain_buffer().await;

        log!("[DEBUG] ==> TX: {:?}", command);
        timeout(WRITE_TIMEOUT, self.conn.send(command.as_bytes()))
            .await
            .map_err(|_| "SEND_TIMEOUT".to_string())?
            .map_err(|e| format!("SEND_ERROR: {}", e))?;

        let mut raw_response = String::new();
        let start = Instant::now();

        while start.elapsed() < CMD_TIMEOUT {
            match timeout(READ_TIMEOUT, self.conn.receive()).await {
                Ok(Ok(data)) if !data.is_empty() => {
                    raw_response.push_str(&String::from_utf8_lossy(&data));
                    if raw_response.contains("OK\r\n") || raw_response.contains("ERROR") {
                        break;
                    }
                }
                Ok(Err(e)) => return Err(format!("READ_ERROR: {}", e)),
                _ => {} // 超时或无数据
            }
        }

        // 清理回显前缀和 ping 干扰
        let mut cleaned = raw_response.replace("ping", "").trim().to_string();
        if cleaned.trim_start().starts_with(&original_cmd) {
            if let Some(pos) = cleaned.find('\n') {
                cleaned = cleaned[(pos + 1)..].to_string();
            }
        }

        let result = cleaned.trim().to_string();
        log!("[DEBUG] <== RX: {:?}", result);

        if result.contains("ERROR") {
            return Err("ERROR".into());
        }
        if result.is_empty() && start.elapsed() >= CMD_TIMEOUT {
            return Err("TIMEOUT".into());
        }

        Ok(result)
    }

    /// 清空缓冲区残留数据
    async fn drain_buffer(&mut self) {
        loop {
            match timeout(Duration::from_millis(10), self.conn.receive()).await {
                Ok(Ok(d)) if !d.is_empty() => continue,
                _ => break,
            }
        }
    }

    /// 初始化模块（连接建立后）
    async fn init_module(&mut self) {
        let _ = self.send_command("ATE0".into()).await;
        let _ = self.send_command("AT+CNMI=2,1,0,2,0".into()).await;
        let _ = self.send_command("AT+CMGF=0".into()).await;
        let _ = self.send_command("AT+CLIP=1".into()).await;
    }

    /// 断开后创建新的连接对象（使用保存的配置）
    fn create_replacement_conn(&self) -> Box<dyn ATConnection> {
        if self.conn_config.conn_type == "NETWORK" {
            Box::new(NetworkATConn {
                config: self.conn_config.network.clone(),
                stream: None,
            })
        } else {
            Box::new(SerialATConn {
                config: self.conn_config.serial.clone(),
                stream: None,
            })
        }
    }
}

// ── 静态文件服务 ───────────────────────────────────────────
async fn static_handler(uri: Uri) -> impl IntoResponse {
    let path = uri.path().trim_start_matches("/5700/").trim_start_matches('/');
    let target_path = if path.is_empty() || path.ends_with('/') { format!("{}index.html", path) } else { path.to_string() };
    match Asset::get(&target_path) {
        Some(content) => {
            let mime = mime_guess::from_path(&target_path).first_or_octet_stream();
            Response::builder().header(header::CONTENT_TYPE, mime.as_ref()).body(Body::from(content.data)).unwrap()
        }
        None => {
            if let Some(index) = Asset::get("index.html") {
                Response::builder().header(header::CONTENT_TYPE, "text/html").body(Body::from(index.data)).unwrap()
            } else {
                (StatusCode::NOT_FOUND, "404 Not Found").into_response()
            }
        }
    }
}

// ── 主入口 ─────────────────────────────────────────────────
#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let config_path = "config.json";
    let config_str = if !Path::new(config_path).exists() {
        tokio::fs::write(config_path, DEFAULT_CONFIG_JSON).await?;
        DEFAULT_CONFIG_JSON.to_string()
    } else {
        tokio::fs::read_to_string(config_path).await?
    };
    let config: Config = serde_json::from_str(&config_str)?;

    let at_conn: Box<dyn ATConnection> = if config.at_config.conn_type == "NETWORK" {
        Box::new(NetworkATConn { config: config.at_config.network.clone(), stream: None })
    } else {
        Box::new(SerialATConn { config: config.at_config.serial.clone(), stream: None })
    };

    let (urc_tx, _) = broadcast::channel(1024);
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

    // 启动 Actor 任务（独占 AT 连接）
    let actor = ConnectionActor {
        conn: at_conn,
        conn_config: config.at_config.clone(),
        urc_tx: urc_tx.clone(),
        cmd_rx,
        heartbeat_failures: 0,
    };
    tokio::spawn(async move { actor.run().await });

    // WebSocket 服务
    let ws_v4_addr = format!("{}:{}", config.websocket_config.ipv4.host, config.websocket_config.ipv4.port);
    let ws_l4 = TcpListener::bind(&ws_v4_addr).await?;

    let urc_tx_ws = urc_tx.clone();
    let cmd_tx_ws = cmd_tx.clone();
    let ws_task = async move {
        while let Ok((stream, _addr)) = ws_l4.accept().await {
            let urc_tx = urc_tx_ws.clone();
            let cmd_tx = cmd_tx_ws.clone();
            tokio::spawn(ws_handler(stream, urc_tx, cmd_tx));
        }
    };

    // HTTP 服务
    let http_addr = format!("{}:{}", config.http_config.host, config.http_config.port);
    let http_l = TcpListener::bind(&http_addr).await?;
    let app = Router::new()
        .route("/", get(|| async { Redirect::permanent("/5700/") }))
        .fallback(static_handler);

    log!("--------------------------------------");
    log!("WebUI Server : http://{}", http_addr);
    log!("WebSocket IPv4: ws://{}", ws_v4_addr);
    log!("--------------------------------------");

    tokio::join!(ws_task, async { axum::serve(http_l, app).await.unwrap() });

    Ok(())
}

/// WebSocket 连接处理
async fn ws_handler(
    stream: TcpStream,
    urc_tx: broadcast::Sender<String>,
    cmd_tx: mpsc::UnboundedSender<CmdRequest>,
) -> Option<()> {
    let ws_stream = tokio_tungstenite::accept_async(stream).await.ok()?;
    let (mut ws_tx, mut ws_rx) = ws_stream.split();
    let mut urc_rx = urc_tx.subscribe();

    loop {
        tokio::select! {
            urc_res = urc_rx.recv() => {
                if let Ok(msg) = urc_res {
                    let payload = serde_json::json!({ "type": "raw_data", "data": msg });
                    if let Ok(json_str) = serde_json::to_string(&payload) {
                        if ws_tx.send(Message::Text(json_str)).await.is_err() { break; }
                    }
                }
            }
            msg = ws_rx.next() => {
                match msg {
                    Some(Ok(Message::Text(cmd))) => {
                        let (reply_tx, reply_rx) = oneshot::channel();
                        let req = CmdRequest { command: cmd, reply_tx };
                        if cmd_tx.send(req).is_err() {
                            // Actor 已关闭
                            break;
                        }
                        let res = match reply_rx.await {
                            Ok(Ok(r)) => serde_json::json!({ "success": true, "data": r, "error": null }),
                            Ok(Err(e)) => serde_json::json!({ "success": false, "data": null, "error": e }),
                            Err(_) => serde_json::json!({ "success": false, "data": null, "error": "ACTOR_GONE" }),
                        };
                        if ws_tx.send(Message::Text(serde_json::to_string(&res).unwrap())).await.is_err() {
                            break;
                        }
                    }
                    _ => break, // 客户端断开或非文本消息
                }
            }
        }
    }
    Some(())
}
