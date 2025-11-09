//! 小爱音箱对话监听与关键词检测模块。
//!
//! 实现了类似 mi-gpt 的动态间隔轮询和关键词匹配机制。

use std::collections::HashSet;
use std::time::Duration;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, trace, warn};

use crate::{Xiaoai, Conversation};

/// 关键词配置。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KeywordConfig {
    /// 关键词列表（支持多个触发词）
    pub keywords: Vec<String>,
    
    /// 匹配模式
    #[serde(default = "default_match_mode")]
    pub match_mode: MatchMode,
    
    /// 是否启用（可用于临时禁用某些关键词）
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    
    /// 关键词描述（用于日志和调试）
    #[serde(default)]
    pub description: String,
}

fn default_match_mode() -> MatchMode {
    MatchMode::StartsWith
}

fn default_enabled() -> bool {
    true
}

/// 匹配模式。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum MatchMode {
    /// 前缀匹配（推荐，准确度高）
    StartsWith,
    /// 包含匹配（可能误触）
    Contains,
    /// 精确匹配
    Exact,
}

/// 关键词监听器配置。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WatcherConfig {
    /// 关键词配置列表（支持两种格式）
    #[serde(deserialize_with = "deserialize_keywords")]
    pub keywords: Vec<KeywordConfig>,
    
    /// 初始轮询间隔（秒）
    #[serde(default = "default_initial_interval")]
    pub initial_interval: f64,
    
    /// 最小轮询间隔（秒）
    #[serde(default = "default_min_interval")]
    pub min_interval: f64,
    
    /// 最大轮询间隔（秒）
    #[serde(default = "default_max_interval")]
    pub max_interval: f64,
    
    /// 单次拉取的对话数量
    #[serde(default = "default_fetch_limit")]
    pub fetch_limit: u32,
    
    /// 是否在检测到关键词后暂停小爱回复
    #[serde(default = "default_block_xiaoai")]
    pub block_xiaoai_response: bool,
}

/// 自定义反序列化函数，支持字符串数组和配置对象数组两种格式
fn deserialize_keywords<'de, D>(deserializer: D) -> Result<Vec<KeywordConfig>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{Error, Unexpected};
    use serde_json::Value;
    
    let value = Value::deserialize(deserializer)?;
    
    match value {
        // 支持简单的字符串数组格式: ["keyword1", "keyword2"]
        Value::Array(arr) if arr.iter().all(|v| v.is_string()) => {
            let keywords: Vec<KeywordConfig> = arr
                .into_iter()
                .filter_map(|v| {
                    if let Value::String(s) = v {
                        Some(KeywordConfig {
                            keywords: vec![s],
                            match_mode: MatchMode::StartsWith,
                            enabled: true,
                            description: String::new(),
                        })
                    } else {
                        None
                    }
                })
                .collect();
            Ok(keywords)
        }
        // 支持配置对象数组格式
        Value::Array(arr) => {
            let configs: Result<Vec<KeywordConfig>, _> = arr
                .into_iter()
                .map(|v| serde_json::from_value(v))
                .collect();
            configs.map_err(Error::custom)
        }
        _ => Err(Error::invalid_type(
            Unexpected::Other("expected array"),
            &"array of strings or keyword configs",
        )),
    }
}

fn default_initial_interval() -> f64 { 1.0 }
fn default_min_interval() -> f64 { 0.5 }
fn default_max_interval() -> f64 { 3.0 }
fn default_fetch_limit() -> u32 { 5 }
fn default_block_xiaoai() -> bool { true }

impl Default for WatcherConfig {
    fn default() -> Self {
        Self {
            keywords: Vec::new(),
            initial_interval: default_initial_interval(),
            min_interval: default_min_interval(),
            max_interval: default_max_interval(),
            fetch_limit: default_fetch_limit(),
            block_xiaoai_response: default_block_xiaoai(),
        }
    }
}

/// 关键词匹配结果。
#[derive(Clone, Debug)]
pub struct KeywordMatch {
    /// 匹配到的关键词配置
    pub config: KeywordConfig,
    /// 匹配到的具体关键词
    pub matched_keyword: String,
    /// 触发的对话
    pub conversation: Conversation,
}

/// 小爱对话监听器。
pub struct ConversationWatcher {
    config: WatcherConfig,
    seen_timestamps: HashSet<i64>,
    current_interval: f64,
}

impl ConversationWatcher {
    /// 创建新的监听器。
    pub fn new(config: WatcherConfig) -> Self {
        Self {
            current_interval: config.initial_interval,
            config,
            seen_timestamps: HashSet::new(),
        }
    }

    /// 从 JSON 文件加载配置。
    pub fn from_json_file(path: impl AsRef<std::path::Path>) -> crate::Result<Self> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path)
            .map_err(|e| {
                // 将 IO 错误转换为 serde_json 错误
                serde_json::Error::io(e)
            })?;
        let config: WatcherConfig = serde_json::from_str(&content)?;
        Ok(Self::new(config))
    }

    /// 获取所有已启用的关键词列表（用于显示）。
    pub fn get_enabled_keywords(&self) -> impl Iterator<Item = &str> {
        self.config
            .keywords
            .iter()
            .filter(|kw| kw.enabled)
            .flat_map(|kw| kw.keywords.iter().map(|s| s.as_str()))
    }

    /// 启动监听循环。
    ///
    /// 此方法会持续运行，当检测到关键词时调用 `on_match` 回调。
    /// 
    /// # 参数
    /// - `xiaoai`: 小爱服务实例
    /// - `device_id`: 设备 ID
    /// - `hardware`: 设备型号
    /// - `on_match`: 关键词匹配回调函数
    pub async fn watch<F, Fut>(
        &mut self,
        xiaoai: &Xiaoai,
        device_id: &str,
        hardware: &str,
        mut on_match: F,
    ) -> crate::Result<()>
    where
        F: FnMut(KeywordMatch) -> Fut,
        Fut: std::future::Future<Output = crate::Result<()>>,
    {
        info!("🎧 开始监听小爱对话...");
        info!("设备 ID: {}", device_id);
        info!("设备型号: {}", hardware);
        info!("已加载 {} 个关键词配置", self.config.keywords.len());
        
        // 打印所有启用的关键词
        for (idx, kw_config) in self.config.keywords.iter().enumerate() {
            if kw_config.enabled {
                let keywords_str = kw_config.keywords.join(", ");
                info!(
                    "  [{}] {} ({}) - 模式: {:?}",
                    idx + 1,
                    kw_config.description.as_str(),
                    keywords_str,
                    kw_config.match_mode
                );
            }
        }
        
        info!("轮询配置: 初始={}s, 最小={}s, 最大={}s", 
              self.config.initial_interval,
              self.config.min_interval,
              self.config.max_interval);
        info!("按 Ctrl+C 停止监听\n");

        loop {
            // 拉取最新对话
            let conversations = xiaoai
                .get_conversations(device_id, hardware, Some(self.config.fetch_limit))
                .await?;

            // 过滤出新对话
            let new_conversations: Vec<_> = conversations
                .iter()
                .filter(|conv| !self.seen_timestamps.contains(&conv.time))
                .collect();

            if !new_conversations.is_empty() {
                trace!("检测到 {} 条新对话", new_conversations.len());
                
                // 加快检测频率
                self.current_interval = self.config.min_interval;

                // 处理新对话（从旧到新）
                for conv in new_conversations.iter().rev() {
                    self.seen_timestamps.insert(conv.time);
                    
                    // 检查是否匹配关键词
                    if let Some(keyword_match) = self.match_keywords(conv) {
                        info!("🔥 检测到关键词触发！");
                        info!("  查询: {}", conv.query);
                        info!("  匹配: {} ({})", 
                              keyword_match.matched_keyword,
                              keyword_match.config.description);
                        
                        // 阻断小爱回复（如果配置启用）
                        if self.config.block_xiaoai_response {
                            debug!("正在暂停小爱回复...");
                            if let Err(e) = xiaoai.set_play_state(device_id, crate::PlayState::Pause).await {
                                warn!("暂停小爱回复失败: {}", e);
                            }
                        }
                        
                        // 调用用户回调
                        on_match(keyword_match).await?;
                    } else {
                        trace!("对话未匹配关键词: {}", conv.query);
                    }
                }
            } else {
                // 无新消息，逐渐降低检测频率
                self.current_interval = (self.current_interval * 1.2).min(self.config.max_interval);
                trace!("无新消息，当前间隔: {:.2}s", self.current_interval);
            }

            // 等待下一次轮询
            tokio::time::sleep(Duration::from_secs_f64(self.current_interval)).await;
        }
    }

    /// 匹配关键词。
    fn match_keywords(&self, conversation: &Conversation) -> Option<KeywordMatch> {
        let query = conversation.query.as_str();
        
        for config in &self.config.keywords {
            if !config.enabled {
                continue;
            }
            
            for keyword in &config.keywords {
                let matched = match config.match_mode {
                    MatchMode::StartsWith => query.starts_with(keyword),
                    MatchMode::Contains => query.contains(keyword),
                    MatchMode::Exact => query == keyword,
                };
                
                if matched {
                    return Some(KeywordMatch {
                        config: config.clone(),
                        matched_keyword: keyword.clone(),
                        conversation: conversation.clone(),
                    });
                }
            }
        }
        
        None
    }

    /// 获取当前轮询间隔。
    pub fn current_interval(&self) -> f64 {
        self.current_interval
    }

    /// 获取已处理的对话数量。
    pub fn processed_count(&self) -> usize {
        self.seen_timestamps.len()
    }
}
