use std::{borrow::Cow, fmt::Display, fs::File, io::BufReader, path::PathBuf};

use anyhow::{Context, ensure};
use clap::{Parser, Subcommand};
use inquire::{Confirm, Password, PasswordDisplayMode, Select, Text};
use miai::{DeviceInfo, PlayState, Xiaoai, ConversationWatcher};
use url::Url;
use serde::{Deserialize, Serialize};

mod ws_server;
use ws_server::{WsServer, WsConfig};
use std::time::Duration;

const DEFAULT_AUTH_FILE: &str = "xiaoai-auth.json";
const DEFAULT_CONFIG_FILE: &str = "config.json";

#[derive(Deserialize, Serialize, Clone)]
struct Config {
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
    #[serde(default = "default_ws_port")]
    ws_port: u16,
    #[serde(default)]
    check: bool,
    #[serde(default)]
    device_id: String,
    #[serde(default)]
    hardware: String,
    // WebSocket 超时配置（秒）
    #[serde(default = "default_max_connections")]
    max_connections: usize,
    #[serde(default = "default_handshake_timeout")]
    handshake_timeout: u64,
    #[serde(default = "default_message_timeout")]
    message_timeout: u64,
    #[serde(default = "default_heartbeat_interval")]
    heartbeat_interval: u64,
    #[serde(default = "default_idle_timeout")]
    idle_timeout: u64,
    #[serde(flatten)]
    watcher_config: serde_json::Value,
}

fn default_ws_port() -> u16 {
    8080
}

fn default_max_connections() -> usize {
    100
}

fn default_handshake_timeout() -> u64 {
    10
}

fn default_message_timeout() -> u64 {
    30
}

fn default_heartbeat_interval() -> u64 {
    30
}

fn default_idle_timeout() -> u64 {
    300
}

impl Config {
    fn to_ws_config(&self) -> WsConfig {
        WsConfig {
            port: self.ws_port,
            max_connections: self.max_connections,
            handshake_timeout: Duration::from_secs(self.handshake_timeout),
            message_timeout: Duration::from_secs(self.message_timeout),
            heartbeat_interval: Duration::from_secs(self.heartbeat_interval),
            idle_timeout: Duration::from_secs(self.idle_timeout),
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if let Commands::Login = cli.command {
        // 尝试从配置文件读取用户名和密码
        let (username, password) = if cli.config_file.exists() {
            let config_file = File::open(&cli.config_file)?;
            let config: Config = serde_json::from_reader(BufReader::new(config_file))?;
            
            if !config.username.is_empty() && !config.password.is_empty() {
                eprintln!("使用配置文件中的凭据登录...");
                (config.username, config.password)
            } else {
                // 配置文件存在但凭据为空，提示用户输入
                let username = Text::new("账号:").prompt()?;
                let password = Password::new("密码:")
                    .with_display_toggle_enabled()
                    .with_display_mode(PasswordDisplayMode::Masked)
                    .without_confirmation()
                    .with_help_message("CTRL + R 显示/隐藏密码")
                    .prompt()?;
                (username, password)
            }
        } else {
            // 配置文件不存在，提示用户输入
            let username = Text::new("账号:").prompt()?;
            let password = Password::new("密码:")
                .with_display_toggle_enabled()
                .with_display_mode(PasswordDisplayMode::Masked)
                .without_confirmation()
                .with_help_message("CTRL + R 显示/隐藏密码")
                .prompt()?;
            (username, password)
        };
        
        let xiaoai = Xiaoai::login(&username, &password).await?;

        let can_save = if cli.auth_file.exists() {
            Confirm::new(&format!("{} 已存在，是否覆盖?", cli.auth_file.display())).prompt()?
        } else {
            true
        };

        if can_save {
            let mut file = File::create(cli.auth_file)?;
            xiaoai.save(&mut file).map_err(anyhow::Error::from_boxed)?;
        }
        return Ok(());
    }

    // 以下命令需要登录
    let xiaoai = cli.xiaoai()?;
    if let Commands::Device = cli.command {
        let device_info = xiaoai.device_info().await?;
        for info in device_info {
            println!("{}", DisplayDeviceInfo(info));
        }
        return Ok(());
    }

    // Wsapi 命令 - 启动 WebSocket API 服务器
    if let Commands::Wsapi = cli.command {
        eprintln!("🌐 启动 WebSocket API 服务器...");
        
        // 加载配置，增加错误处理
        let config = match File::open(&cli.config_file) {
            Ok(config_file) => {
                match serde_json::from_reader::<_, Config>(BufReader::new(config_file)) {
                    Ok(config) => config,
                    Err(e) => {
                        eprintln!("❌ 配置文件格式错误: {}", e);
                        eprintln!("请检查 {} 的格式", cli.config_file.display());
                        return Err(e.into());
                    }
                }
            }
            Err(e) => {
                eprintln!("❌ 无法打开配置文件: {}", e);
                eprintln!("请确保 {} 存在", cli.config_file.display());
                return Err(e.into());
            }
        };
        
        // 如果启用了 check，获取或验证设备信息
        if config.check {
            eprintln!("🔍 关键词监听已启用");
            
            let (device_id, hardware) = match get_device_info(&xiaoai, &config).await {
                Ok(info) => info,
                Err(e) => {
                    eprintln!("❌ 获取设备信息失败，但服务器仍将启动: {}", e);
                    eprintln!("💡 提示: 可以在配置文件中手动设置 device_id 和 hardware");
                    // 使用空字符串，让关键词监听在运行时再次尝试
                    (String::new(), String::new())
                }
            };
            
            // 创建 WebSocket 服务器，传入配置
            let ws_config = config.to_ws_config();
            let server = WsServer::new(xiaoai.clone(), ws_config);
            let server_watch = server.clone();
            
            eprintln!("🚀 启动服务器和关键词监听...");
            
            // 使用 select! 同时运行服务器和监听器，但不让任何一个的错误影响另一个
            tokio::select! {
                result = server.run_server() => {
                    match result {
                        Ok(_) => eprintln!("✅ WebSocket 服务器正常退出"),
                        Err(e) => eprintln!("❌ WebSocket 服务器出错: {}", e),
                    }
                },
                result = server_watch.run_watcher(device_id, hardware) => {
                    match result {
                        Ok(_) => eprintln!("✅ 关键词监听正常退出"),
                        Err(e) => eprintln!("❌ 关键词监听出错: {}", e),
                    }
                },
            }
        } else {
            eprintln!("🚀 启动纯 WebSocket API 服务器（无关键词监听）...");
            
            let ws_config = config.to_ws_config();
            let server = WsServer::new(xiaoai.clone(), ws_config);
            match server.run_server().await {
                Ok(_) => eprintln!("✅ WebSocket 服务器正常退出"),
                Err(e) => {
                    eprintln!("❌ WebSocket 服务器出错: {}", e);
                    return Err(e);
                }
            }
        }
        
        return Ok(());
    }

    // 以下命令需要设备 ID
    let device_id = cli.device_id(&xiaoai).await?;
    let response = match &cli.command {
        Commands::Say { text } => xiaoai.tts(&device_id, text).await?,
        Commands::Play { url } => {
            if let Some(url) = url {
                xiaoai.play_url(&device_id, url.as_str()).await?
            } else {
                xiaoai.set_play_state(&device_id, PlayState::Play).await?
            }
        }
        Commands::Volume { volume } => xiaoai.set_volume(&device_id, *volume).await?,
        Commands::Ask { text } => xiaoai.nlp(&device_id, text).await?,
        Commands::Pause => xiaoai.set_play_state(&device_id, PlayState::Pause).await?,
        Commands::Stop => xiaoai.set_play_state(&device_id, PlayState::Stop).await?,
        Commands::Status => {
            let status = xiaoai.player_status_parsed(&device_id).await?;
            // status.raw 已经是 serde_json::Value 类型
            println!("{}", serde_json::to_string_pretty(&status.raw)?);
            return Ok(());
        }
        Commands::Check => {
            // 获取设备信息
            let devices = xiaoai.device_info().await?;
            let device_info = devices.iter().find(|d| d.device_id == device_id);
            let hardware = device_info
                .map(|d| d.hardware.as_str())
                .unwrap_or("LX06");
            
            // 输出初始化信息到 stderr，避免干扰 JSON 输出
            eprintln!("🎧 开始监听音箱关键词...");
            eprintln!("设备: {}", device_info.map(|d| d.name.as_str()).unwrap_or("未知"));
            eprintln!("硬件型号: {}", hardware);
            eprintln!("配置文件: {}", cli.config_file.display());
            eprintln!("按 Ctrl+C 停止监听\n");
            
            // 加载关键词配置
            let mut watcher = ConversationWatcher::from_json_file(&cli.config_file)
                .with_context(|| format!("加载配置文件 {} 失败", cli.config_file.display()))?;
            
            // 输出已启用的关键词到 stderr
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
            
            // 克隆 device_id 以便在闭包中使用
            let device_id_clone = device_id.to_string();
            
            // 启动监听
            watcher.watch(&xiaoai, &device_id, hardware, move |keyword_match| {
                let device_id = device_id_clone.clone();
                async move {
                    // 输出匹配信息为 JSON
                    let output = serde_json::json!({
                        "timestamp": keyword_match.conversation.time,
                        "query": keyword_match.conversation.query,
                        "matched_keyword": keyword_match.matched_keyword,
                        "device_id": device_id,
                    });
                    
                    println!("{}", serde_json::to_string(&output)?);
                    
                    Ok(())
                }
            }).await?;
            
            return Ok(());
        }
        _ => unreachable!("所有命令都应该被处理"),
    };
    println!("code: {}", response.code);
    println!("message: {}", response.message);
    println!("data: {}", response.data);

    Ok(())
}

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// 指定认证文件
    #[arg(long, default_value = DEFAULT_AUTH_FILE)]
    auth_file: PathBuf,

    /// 指定配置文件
    #[arg(short, long, default_value = DEFAULT_CONFIG_FILE)]
    config_file: PathBuf,

    /// 指定设备 ID
    #[arg(short, long)]
    device_id: Option<String>,
}

#[derive(Subcommand)]
enum Commands {
    /// 登录以获得认证
    Login,
    /// 列出设备
    Device,
    /// 播报文本
    Say { text: String },
    /// 播放
    Play {
        /// 可选的音乐链接
        url: Option<Url>,
    },
    /// 暂停
    Pause,
    /// 停止
    Stop,
    /// 调整音量
    Volume { volume: u32 },
    /// 询问
    Ask { text: String },
    /// 获取播放状态与最近对话文本
    Status,
    /// 监听关键词并触发回调（使用配置文件）
    Check,
    /// 启动 WebSocket API 服务器
    Wsapi,
}

impl Cli {
    fn xiaoai(&self) -> anyhow::Result<Xiaoai> {
        let file = File::open(&self.auth_file)
            .with_context(|| format!("需要可用的认证文件 {}", self.auth_file.display()))?;

        Xiaoai::load(BufReader::new(file))
            .map_err(anyhow::Error::from_boxed)
            .with_context(|| format!("加载认证文件 {} 失败", self.auth_file.display()))
    }

    /// 获取用户指定的设备 ID。
    ///
    /// 如果用户没有在命令行指定，则会向服务器请求设备列表。
    /// 如果请求结果只有一个设备，会自动选择这个唯一的设备。
    /// 如果请求结果存在多个设备，则会让用户自行选择。
    async fn device_id(&'_ self, xiaoai: &Xiaoai) -> anyhow::Result<Cow<'_, str>> {
        if let Some(device_id) = &self.device_id {
            return Ok(device_id.into());
        }

        let info = xiaoai.device_info().await.context("获取设备列表失败")?;
        ensure!(!info.is_empty(), "无可用设备，需要在小米音箱 APP 中绑定");
        if info.len() == 1 {
            return Ok(info[0].device_id.clone().into());
        }

        let options = info.into_iter().map(DisplayDeviceInfo).collect();
        let ans = Select::new("目标设备?", options).prompt()?;

        Ok(ans.0.device_id.into())
    }
}

struct DisplayDeviceInfo(DeviceInfo);

impl Display for DisplayDeviceInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "名称: {}", self.0.name)?;
        writeln!(f, "设备 ID: {}", self.0.device_id)?;
        writeln!(f, "机型: {}", self.0.hardware)
    }
}

/// 获取设备信息
async fn get_device_info(
    xiaoai: &Xiaoai,
    config: &Config,
) -> anyhow::Result<(String, String)> {
    if !config.device_id.is_empty() && !config.hardware.is_empty() {
        return Ok((config.device_id.clone(), config.hardware.clone()));
    }

    eprintln!("📱 未配置设备信息，正在自动获取...");
    
    let devices = xiaoai.device_info().await.context("获取设备列表失败")?;
    ensure!(!devices.is_empty(), "无可用设备，需要在小米音箱 APP 中绑定");
    
    if devices.len() == 1 {
        let device = &devices[0];
        eprintln!("✅ 自动选择唯一设备: {} ({})", device.name, device.hardware);
        Ok((device.device_id.clone(), device.hardware.clone()))
    } else {
        eprintln!("📋 找到 {} 个设备:", devices.len());
        for (i, device) in devices.iter().enumerate() {
            eprintln!("  {}. {} - {} ({})", i + 1, device.name, device.device_id, device.hardware);
        }
        
        // 使用第一个设备
        let device = &devices[0];
        eprintln!("✅ 自动选择第一个设备: {} ({})", device.name, device.hardware);
        eprintln!("💡 提示: 可以在 config.json 中设置 device_id 和 hardware 来指定设备");
        Ok((device.device_id.clone(), device.hardware.clone()))
    }
}
