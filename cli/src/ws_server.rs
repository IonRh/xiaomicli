use std::{net::SocketAddr, sync::Arc, time::Duration, sync::atomic::{AtomicUsize, Ordering}};

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use miai::{PlayState, Xiaoai};
use serde::{Deserialize, Serialize};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, RwLock, Semaphore};
use tokio::io::AsyncWriteExt;
use tokio_tungstenite::{accept_async, tungstenite::Message};
use tokio::time::{timeout, sleep, interval};

type ClientSender = futures_util::stream::SplitSink<tokio_tungstenite::WebSocketStream<TcpStream>, Message>;
type Clients = Arc<RwLock<Vec<Arc<Mutex<ClientSender>>>>>;

/// WebSocket 服务器配置
#[derive(Debug, Clone)]
pub struct WsConfig {
    pub port: u16,
    pub max_connections: usize,
    pub handshake_timeout: Duration,
    pub message_timeout: Duration,
    pub heartbeat_interval: Duration,
    pub idle_timeout: Duration,
}

/// WebSocket API 请求
#[derive(Debug, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
enum ApiRequest {
    Say {
        device_id: String,
        text: String,
    },
    Play {
        device_id: String,
        url: Option<String>,
    },
    Pause {
        device_id: String,
    },
    Stop {
        device_id: String,
    },
    Volume {
        device_id: String,
        volume: u32,
    },
    Ask {
        device_id: String,
        text: String,
    },
    Status {
        device_id: String,
    },
    GetDevices,
}

/// WebSocket API 响应
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ApiResponse {
    Success {
        code: i64,
        message: String,
        data: serde_json::Value,
    },
    Error {
        error: String,
    },
    Devices {
        devices: Vec<DeviceData>,
    },
    KeywordMatch {
        timestamp: i64,
        query: String,
        matched_keyword: String,
        device_id: String,
    },
}

#[derive(Debug, Serialize)]
struct DeviceData {
    device_id: String,
    name: String,
    hardware: String,
}

/// WebSocket 服务器
#[derive(Clone)]
pub struct WsServer {
    xiaoai: Arc<RwLock<Xiaoai>>,
    config: WsConfig,
    clients: Clients,
    connection_count: Arc<AtomicUsize>,
    connection_semaphore: Arc<Semaphore>,
}

impl WsServer {
    pub fn new(xiaoai: Xiaoai, config: WsConfig) -> Self {
        Self {
            xiaoai: Arc::new(RwLock::new(xiaoai)),
            connection_semaphore: Arc::new(Semaphore::new(config.max_connections)),
            config,
            clients: Arc::new(RwLock::new(Vec::new())),
            connection_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub async fn run_server(&self) -> Result<()> {
        let addr = SocketAddr::from(([0, 0, 0, 0], self.config.port));
        
        // 配置 TCP 监听器
        let listener = TcpListener::bind(&addr).await?;
        
        eprintln!("🚀 WebSocket 服务器已启动");
        eprintln!("监听地址: ws://{}", addr);
        eprintln!("最大连接数: {}", self.config.max_connections);
        eprintln!("握手超时: {:?}", self.config.handshake_timeout);
        eprintln!("消息处理超时: {:?}", self.config.message_timeout);
        eprintln!("心跳间隔: {:?}", self.config.heartbeat_interval);
        eprintln!("空闲超时: {:?}", self.config.idle_timeout);
        eprintln!("按 Ctrl+C 停止服务\n");

        loop {
            match listener.accept().await {
                Ok((mut stream, peer_addr)) => {
                    // 检查连接数限制
                    let current_connections = self.connection_count.load(Ordering::Relaxed);
                    if current_connections >= self.config.max_connections {
                        eprintln!("⚠️  连接数已达上限 ({}), 拒绝新连接 {}", self.config.max_connections, peer_addr);
                        let _ = stream.shutdown().await;
                        continue;
                    }
                    
                    // 尝试获取信号量（非阻塞）
                    if let Ok(permit) = self.connection_semaphore.clone().try_acquire_owned() {
                        let xiaoai = Arc::clone(&self.xiaoai);
                        let clients = Arc::clone(&self.clients);
                        let connection_count = Arc::clone(&self.connection_count);
                        let config = self.config.clone();
                        
                        // 增加连接计数
                        connection_count.fetch_add(1, Ordering::Relaxed);
                        
                        tokio::spawn(async move {
                            let _permit = permit; // 持有许可直到任务结束
                            
                            if let Err(e) = handle_connection_with_timeout(stream, peer_addr, xiaoai, clients, config).await {
                                eprintln!("⚠️  处理连接 {} 时出错: {}", peer_addr, e);
                            }
                            
                            // 减少连接计数
                            connection_count.fetch_sub(1, Ordering::Relaxed);
                            eprintln!("📊 当前连接数: {}", connection_count.load(Ordering::Relaxed));
                        });
                    } else {
                        eprintln!("⚠️  无法获取连接许可，拒绝连接 {}", peer_addr);
                        let _ = stream.shutdown().await;
                    }
                }
                Err(e) => {
                    eprintln!("⚠️  接受连接时出错: {}, 继续监听...", e);
                    // 短暂延迟避免忙循环
                    sleep(Duration::from_millis(50)).await;
                }
            }
        }
    }

    /// 运行关键词监听器
    pub async fn run_watcher(&self, device_id: String, hardware: String) -> Result<()> {
        self.start_keyword_watcher(device_id, hardware).await
    }

    /// 启动关键词监听（内部方法）
    async fn start_keyword_watcher(&self, device_id: String, hardware: String) -> Result<()> {
        use miai::ConversationWatcher;
        use tokio::time::Duration;
        
        let config_path = std::path::PathBuf::from("config.json");
        let clients = Arc::clone(&self.clients);
        let xiaoai = Arc::clone(&self.xiaoai);
        
        eprintln!("🎧 开始监听关键词...");
        eprintln!("设备 ID: {}", device_id);
        eprintln!("设备型号: {}", hardware);
        
        loop {
            // 尝试加载配置文件
            let mut watcher = match ConversationWatcher::from_json_file(&config_path) {
                Ok(watcher) => watcher,
                Err(e) => {
                    eprintln!("❌ 加载配置文件失败: {}, 5秒后重试...", e);
                    sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };
            
            let enabled_keywords: Vec<_> = watcher.get_enabled_keywords().collect();
            if enabled_keywords.is_empty() {
                eprintln!("⚠️  警告: 配置文件中没有启用的关键词");
            } else {
                eprintln!("📝 已启用的关键词:");
                for (i, kw) in enabled_keywords.iter().enumerate() {
                    eprintln!("  {}. {}", i + 1, kw);
                }
            }
            eprintln!("---\n");
            
            let device_id_clone = device_id.clone();
            
            // 开始监听，带错误处理
            let result = {
                let xiaoai_guard = xiaoai.read().await;
                let device_id_for_closure = device_id_clone.clone();
                let clients_for_closure = Arc::clone(&clients);
                
                watcher
                    .watch(&*xiaoai_guard, &device_id, &hardware, move |keyword_match| {
                        let device_id = device_id_for_closure.clone();
                        let clients = Arc::clone(&clients_for_closure);
                        
                        async move {
                            let response = ApiResponse::KeywordMatch {
                                timestamp: keyword_match.conversation.time,
                                query: keyword_match.conversation.query.clone(),
                                matched_keyword: keyword_match.matched_keyword.to_string(),
                                device_id,
                            };
                            
                            match serde_json::to_string(&response) {
                                Ok(response_text) => {
                                    broadcast_message(&clients, response_text).await;
                                }
                                Err(e) => {
                                    eprintln!("⚠️ 序列化响应失败: {}", e);
                                }
                            }
                            
                            Ok(())
                        }
                    })
                    .await
            };
            
            match result {
                Ok(_) => {
                    eprintln!("✅ 关键词监听正常退出");
                    break;
                }
                Err(e) => {
                    eprintln!("❌ 关键词监听出错: {}, 10秒后重试...", e);
                    sleep(Duration::from_secs(10)).await;
                    // 继续循环重试
                }
            }
        }
        
        Ok(())
    }


}

/// 向所有连接的客户端广播消息
async fn broadcast_message(clients: &Clients, message: String) {
    let clients_lock = clients.read().await;
    let mut disconnected = Vec::new();
    
    if clients_lock.is_empty() {
        // 没有连接的客户端，无需广播
        return;
    }
    
    eprintln!("📢 广播消息到 {} 个客户端", clients_lock.len());
    
    for (idx, client) in clients_lock.iter().enumerate() {
        // 使用 try_lock 避免阻塞其他客户端
        match client.try_lock() {
            Ok(mut sender) => {
                if let Err(e) = sender.send(Message::Text(message.clone())).await {
                    eprintln!("⚠️ 发送消息到客户端 {} 失败: {}", idx, e);
                    disconnected.push(idx);
                }
            }
            Err(_) => {
                eprintln!("⚠️ 无法获取客户端 {} 的锁，跳过此客户端", idx);
                // 不标记为断开连接，可能只是暂时繁忙
            }
        }
    }
    
    drop(clients_lock);
    
    // 清理断开连接的客户端
    if !disconnected.is_empty() {
        let mut clients_lock = clients.write().await;
        // 从后往前删除，避免索引偏移
        for idx in disconnected.iter().rev() {
            if *idx < clients_lock.len() {
                clients_lock.remove(*idx);
                eprintln!("🗑️ 移除断开的客户端 {}", idx);
            }
        }
        eprintln!("📊 当前连接数: {}", clients_lock.len());
    }
}

/// 带超时控制的连接处理（仅限握手阶段）
async fn handle_connection_with_timeout(
    stream: TcpStream,
    peer_addr: SocketAddr,
    xiaoai: Arc<RwLock<Xiaoai>>,
    clients: Clients,
    config: WsConfig,
) -> Result<()> {
    // 直接调用 handle_connection，不再包裹整个连接处理过程
    // 超时控制已经在 handle_connection 内部的握手阶段实现
    handle_connection(stream, peer_addr, xiaoai, clients, config).await
}

async fn handle_connection(
    stream: TcpStream,
    peer_addr: SocketAddr,
    xiaoai: Arc<RwLock<Xiaoai>>,
    clients: Clients,
    config: WsConfig,
) -> Result<()> {
    eprintln!("✅ 新连接: {}", peer_addr);
    
    // 设置TCP选项以提高连接稳定性
    if let Err(e) = stream.set_nodelay(true) {
        eprintln!("⚠️  设置 TCP_NODELAY 失败: {}", e);
    }
    
    // 缩短握手超时时间
    let ws_stream = match timeout(Duration::from_secs(5), accept_async(stream)).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => {
            eprintln!("❌ WebSocket 握手失败 {}: {}", peer_addr, e);
            return Err(e.into());
        }
        Err(_) => {
            eprintln!("❌ WebSocket 握手超时 {} (5s)", peer_addr);
            return Err(anyhow::anyhow!("WebSocket 握手超时"));
        }
    };
    
    let (ws_sender, mut ws_receiver) = ws_stream.split();
    
    let ws_sender = Arc::new(Mutex::new(ws_sender));
    
    // 将新客户端添加到客户端列表
    {
        let mut clients_lock = clients.write().await;
        clients_lock.push(Arc::clone(&ws_sender));
        eprintln!("当前连接数: {}", clients_lock.len());
    }
    
    // 启动心跳任务 - 使用更长的间隔避免过于频繁
    let ws_sender_heartbeat = Arc::clone(&ws_sender);
    let heartbeat_interval_duration = config.heartbeat_interval;
    let heartbeat_task = tokio::spawn(async move {
        let mut heartbeat_interval = interval(heartbeat_interval_duration);
        heartbeat_interval.tick().await; // 跳过第一次立即触发
        
        loop {
            heartbeat_interval.tick().await;
            
            // 使用较短的超时进行非阻塞心跳发送
            let send_result = timeout(Duration::from_secs(2), async {
                let mut sender = ws_sender_heartbeat.lock().await;
                sender.send(Message::Ping(vec![])).await
            }).await;
            
            match send_result {
                Ok(Ok(_)) => {
                    // 心跳发送成功
                }
                Ok(Err(_)) => {
                    // 心跳发送失败，连接断开
                    break;
                }
                Err(_) => {
                    // 心跳超时，可能连接有问题
                    eprintln!("⚠️  心跳发送超时: {}", peer_addr);
                    break;
                }
            }
        }
    });
    
    // 消息接收循环，使用较长的空闲超时
    let idle_timeout = config.idle_timeout;
    let message_timeout = config.message_timeout;
    loop {
        let msg_result = match timeout(idle_timeout, ws_receiver.next()).await {
            Ok(Some(result)) => result,
            Ok(None) => {
                eprintln!("📟 连接流结束: {}", peer_addr);
                break;
            }
            Err(_) => {
                // 5分钟无消息，发送 ping 检查连接状态
                eprintln!("⏱️  长时间无消息（{}秒），检查连接状态: {}", idle_timeout.as_secs(), peer_addr);
                
                // 使用超时的 ping 检查连接
                let ping_result = timeout(Duration::from_secs(5), async {
                    let mut sender = ws_sender.lock().await;
                    sender.send(Message::Ping(vec![])).await
                }).await;
                
                match ping_result {
                    Ok(Ok(_)) => {
                        eprintln!("✅ 连接检查通过: {}", peer_addr);
                        // ping 发送成功，继续等待
                        continue;
                    }
                    Ok(Err(e)) => {
                        eprintln!("❌ 发送心跳失败 {}: {}", peer_addr, e);
                        break;
                    }
                    Err(_) => {
                        eprintln!("❌ 心跳发送超时 {}", peer_addr);
                        break;
                    }
                }
            }
        };
        
        let msg = match msg_result {
            Ok(msg) => msg,
            Err(e) => {
                eprintln!("⚠️ 接收消息错误 {}: {}, 继续处理其他消息", peer_addr, e);
                continue;
            }
        };
        
        if msg.is_close() {
            eprintln!("❌ 连接关闭: {}", peer_addr);
            break;
        }
        
        // 处理 ping/pong 消息
        if msg.is_ping() {
            let mut sender = ws_sender.lock().await;
            if let Err(e) = sender.send(Message::Pong(msg.into_data())).await {
                eprintln!("⚠️  发送 pong 失败 {}: {}", peer_addr, e);
                break;
            }
            continue;
        }
        
        if msg.is_pong() {
            // 收到 pong，连接正常
            continue;
        }
        
        if !msg.is_text() {
            continue;
        }
        
        let text = match msg.to_text() {
            Ok(text) => text,
            Err(e) => {
                eprintln!("⚠️ 消息格式错误 {}: {}", peer_addr, e);
                continue;
            }
        };
        
        eprintln!("📨 收到消息 {}: {}", peer_addr, text);
        
        // 使用超时控制 API 请求处理，避免长时间阻塞
        let response = match serde_json::from_str::<ApiRequest>(text) {
            Ok(request) => {
                let ws_sender_clone = Arc::clone(&ws_sender);
                
                // 添加 API 请求处理超时
                match timeout(message_timeout, async {
                    let xiaoai_guard = xiaoai.read().await;
                    handle_request(request, &*xiaoai_guard, ws_sender_clone).await
                }).await {
                    Ok(response) => response,
                    Err(_) => {
                        eprintln!("⏱️  API 请求处理超时: {}", peer_addr);
                        ApiResponse::Error {
                            error: "请求处理超时，请稍后重试".to_string(),
                        }
                    }
                }
            }
            Err(e) => ApiResponse::Error {
                error: format!("无效的请求格式: {}", e),
            },
        };
        
        let response_text = match serde_json::to_string(&response) {
            Ok(text) => text,
            Err(e) => {
                eprintln!("⚠️ 序列化响应失败 {}: {}", peer_addr, e);
                continue;
            }
        };
        
        eprintln!("📤 发送响应 {}: {}", peer_addr, response_text);
        
        // 发送响应时也添加错误处理
        let send_result = {
            let mut sender = ws_sender.lock().await;
            sender.send(Message::Text(response_text)).await
        };
        
        if let Err(e) = send_result {
            eprintln!("⚠️ 发送响应失败 {}: {}, 连接可能已断开", peer_addr, e);
            break;
        }
    }
    
    // 停止心跳任务
    heartbeat_task.abort();
    
    // 从客户端列表中移除
    {
        let mut clients_lock = clients.write().await;
        clients_lock.retain(|client| !Arc::ptr_eq(client, &ws_sender));
        eprintln!("🚪 连接关闭: {}, 当前连接数: {}", peer_addr, clients_lock.len());
    }
    
    Ok(())
}

async fn handle_request(
    request: ApiRequest,
    xiaoai: &Xiaoai,
    _ws_sender: Arc<Mutex<futures_util::stream::SplitSink<tokio_tungstenite::WebSocketStream<TcpStream>, Message>>>,
) -> ApiResponse {
    // 为每个请求添加日志和错误处理
    let result = match &request {
        ApiRequest::Say { device_id, text } => {
            eprintln!("🗣️ 执行 TTS: 设备={}, 文本={}", device_id, text);
            xiaoai.tts(device_id, text).await
        }
        ApiRequest::Play { device_id, url } => {
            if let Some(url) = url {
                eprintln!("🎵 播放 URL: 设备={}, URL={}", device_id, url);
                xiaoai.play_url(device_id, url).await
            } else {
                eprintln!("▶️ 继续播放: 设备={}", device_id);
                xiaoai.set_play_state(device_id, PlayState::Play).await
            }
        }
        ApiRequest::Pause { device_id } => {
            eprintln!("⏸️ 暂停播放: 设备={}", device_id);
            xiaoai.set_play_state(device_id, PlayState::Pause).await
        }
        ApiRequest::Stop { device_id } => {
            eprintln!("⏹️ 停止播放: 设备={}", device_id);
            xiaoai.set_play_state(device_id, PlayState::Stop).await
        }
        ApiRequest::Volume { device_id, volume } => {
            eprintln!("🔊 调整音量: 设备={}, 音量={}", device_id, volume);
            xiaoai.set_volume(device_id, *volume).await
        }
        ApiRequest::Ask { device_id, text } => {
            eprintln!("❓ 询问小爱: 设备={}, 问题={}", device_id, text);
            xiaoai.nlp(device_id, text).await
        }
        ApiRequest::Status { device_id } => {
            eprintln!("📊 获取状态: 设备={}", device_id);
            match xiaoai.player_status_parsed(device_id).await {
                Ok(status) => {
                    eprintln!("✅ 状态获取成功");
                    return ApiResponse::Success {
                        code: 0,
                        message: "OK".to_string(),
                        data: status.raw,
                    };
                }
                Err(e) => {
                    eprintln!("❌ 获取状态失败: {}", e);
                    return ApiResponse::Error {
                        error: format!("获取状态失败: {}", e),
                    };
                }
            }
        }
        ApiRequest::GetDevices => {
            eprintln!("📱 获取设备列表");
            match xiaoai.device_info().await {
                Ok(devices) => {
                    eprintln!("✅ 设备列表获取成功，共 {} 个设备", devices.len());
                    let device_data = devices
                        .into_iter()
                        .map(|d| DeviceData {
                            device_id: d.device_id,
                            name: d.name,
                            hardware: d.hardware,
                        })
                        .collect();
                    
                    return ApiResponse::Devices {
                        devices: device_data,
                    };
                }
                Err(e) => {
                    eprintln!("❌ 获取设备列表失败: {}", e);
                    return ApiResponse::Error {
                        error: format!("获取设备列表失败: {}", e),
                    };
                }
            }
        }
    };
    
    match result {
        Ok(response) => {
            eprintln!("✅ API 请求成功: code={}, message={}", response.code, response.message);
            ApiResponse::Success {
                code: response.code,
                message: response.message,
                data: response.data,
            }
        }
        Err(e) => {
            eprintln!("❌ API 请求失败: {}", e);
            ApiResponse::Error {
                error: format!("{}", e),
            }
        }
    }
}


