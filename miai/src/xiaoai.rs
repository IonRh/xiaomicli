use std::{
    collections::HashMap,
    io::{BufRead, Write},
    sync::Arc,
};

use cookie_store::serde::json::{load_all, save_incl_expired_and_nonpersistent};
use reqwest::{Client, Url};
use reqwest_cookie_store::CookieStoreMutex;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::trace;

use crate::{XiaoaiResponse, login::Login, util::random_id};

const API_SERVER: &str = "https://api2.mina.mi.com/";
const API_UA: &str = "MiHome/6.0.103 (com.xiaomi.mihome; build:6.0.103.1; iOS 14.4.0) Alamofire/6.0.103 MICO/iOSApp/appStore/6.0.103";

/// 提供小爱服务请求。
///
/// `Xiaoai` 代表着一个账号的登录状态，但如果需要重用的话，也无需再包一层
/// [`std::rc::Rc`] 或 [`Arc`]，`Xiaoai` 已经在内部使用 [`Arc`] 共享状态。
#[derive(Clone, Debug)]
pub struct Xiaoai {
    client: Client,
    cookie_store: Arc<CookieStoreMutex>,
    server: Url,
}

impl Xiaoai {
    /// 登录以调用小爱服务。
    pub async fn login(username: &str, password: &str) -> crate::Result<Self> {
        let login = Login::new(username, password)?;
        let login_response = login.login().await?;
        let auth_response = login.auth(login_response).await?;
        login.get_token(auth_response).await?;

        Self::from_login(login)
    }

    /// 从 [`Login`][`crate::login::Login`] 构造。
    pub fn from_login(login: Login) -> crate::Result<Self> {
        let cookie_store = login.into_cookie_store();
        let client = Client::builder()
            .user_agent(API_UA)
            .cookie_provider(cookie_store.clone())
            .build()?;

        Ok(Self {
            client,
            cookie_store,
            server: Url::parse(API_SERVER)?,
        })
    }

    /// 列出所有设备的信息。
    pub async fn device_info(&self) -> crate::Result<Vec<DeviceInfo>> {
        self.raw_device_info().await?.extract_data()
    }

    /// 同 [`Xiaoai::device_info`]，但返回原始的响应。
    pub async fn raw_device_info(&self) -> crate::Result<XiaoaiResponse> {
        let response = self.get("admin/v2/device_list?master=0").await?;
        trace!("获取到设备列表: {}", response.data);

        Ok(response)
    }

    /// 小爱服务的通用 GET 请求。
    ///
    /// API 服务器会和 `uri` 做 [`Url::join`]。
    pub async fn get(&self, uri: &str) -> crate::Result<XiaoaiResponse> {
        let request_id = random_request_id();
        let url =
            Url::parse_with_params(self.server.join(uri)?.as_str(), [("requestId", request_id)])?;
        let response = self
            .client
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .json::<XiaoaiResponse>()
            .await?
            .error_for_code()?;

        Ok(response)
    }

    /// 小爱服务的通用 POST 请求。
    ///
    /// 同 [`Xiaoai::get`]，但可以带表单数据。
    pub async fn post(
        &self,
        uri: &str,
        mut form: HashMap<&str, &str>,
    ) -> crate::Result<XiaoaiResponse> {
        let request_id = random_request_id();
        form.insert("requestId", &request_id);
        let url = self.server.join(uri)?;
        let response = self
            .client
            .post(url)
            .form(&form)
            .send()
            .await?
            .error_for_status()?
            .json::<XiaoaiResponse>()
            .await?
            .error_for_code()?;

        Ok(response)
    }

    /// 保存登录状态到 `writer`。
    ///
    /// 状态被保存为明文的 json，请注意安全性。参见
    /// [`cookie_store::serde::json::save_incl_expired_and_nonpersistent`]。
    ///
    /// # Panics
    ///
    /// 当内部发生锁中毒时会 panic。
    pub fn save<W: Write>(&self, writer: &mut W) -> cookie_store::Result<()> {
        save_incl_expired_and_nonpersistent(&self.cookie_store.lock().unwrap(), writer)
    }

    /// 从 `reader` 加载登录状态。
    ///
    /// **不会**验证登录状态的有效性，如果在请求时出错，请尝试重新
    /// [`login`][Xiaoai::login]。另请参见 [`cookie_store::serde::json::load_all`]。
    pub fn load<R: BufRead>(reader: R) -> cookie_store::Result<Self> {
        let cookie_store = Arc::new(CookieStoreMutex::new(load_all(reader)?));
        let client = Client::builder()
            .user_agent(API_UA)
            .cookie_provider(Arc::clone(&cookie_store))
            .build()?;

        Ok(Self {
            client,
            cookie_store,
            server: Url::parse(API_SERVER)?,
        })
    }

    /// 向小爱设备发送 OpenWrt UBUS 调用请求。
    pub async fn ubus_call(
        &self,
        device_id: &str,
        path: &str,
        method: &str,
        message: &str,
    ) -> crate::Result<XiaoaiResponse> {
        let form = HashMap::from([
            ("deviceId", device_id),
            ("method", method),
            ("path", path),
            ("message", message),
        ]);

        self.post("remote/ubus", form).await
    }

    /// 请求小爱设备播报文本。
    pub async fn tts(&self, device_id: &str, text: &str) -> crate::Result<XiaoaiResponse> {
        let message = json!({"text": text}).to_string();

        self.ubus_call(device_id, "mibrain", "text_to_speech", &message)
            .await
    }

    /// 请求小爱播放 `url`。
    pub async fn play_url(&self, device_id: &str, url: &str) -> crate::Result<XiaoaiResponse> {
        let message = json!({
            "url": url,
            // type 字段不仅能控制亮灯行为，还能控制暂停行为？
            // 比如在机型 L16A 上，设为 3 才能有完整的播放、暂停控制，但无法停止
            // 设为 0、1 可以播放、停止，但暂停后就无法恢复，设为 2 则无法暂停
            // 貌似每个机型都不太一样，参考 https://github.com/yihong0618/MiService/issues/30
            "type": 3,
            "media": "app_ios"
        })
        .to_string();

        self.ubus_call(device_id, "mediaplayer", "player_play_url", &message)
            .await
    }

    /// 请求小爱播放音乐。
    ///
    /// 和 [`Xiaoai::play_url`] 相比，此方法针对音频特化，能支持更多参数，但并非所有机型都支持。
    /// 目前尚不支持配置这些参数，仅用作播放音乐的另一种方案。
    pub async fn play_music(&self, device_id: &str, url: &str) -> crate::Result<XiaoaiResponse> {
        const AUDIO_ID: &str = "1582971365183456177";
        const ID: &str = "355454500";
        let message = json!({
            "startaudioid": AUDIO_ID,
            "music": {
                "payload": {
                    // 来自 miservice:
                    // If set to "MUSIC", the light will be on
                    // "audio_type": "MUSIC",
                    "audio_items": [
                        {
                            "item_id": {
                                "audio_id": AUDIO_ID,
                                "cp": {
                                    "album_id": "-1",
                                    "episode_index": 0,
                                    "id": ID,
                                    "name": "xiaowei",
                                },
                            },
                            "stream": {"url": url},
                        }
                    ],
                    "list_params": {
                        "listId": "-1",
                        "loadmore_offset": 0,
                        "origin": "xiaowei",
                        "type": "MUSIC",
                    },
                },
                "play_behavior": "REPLACE_ALL",
            }
        })
        .to_string();

        self.ubus_call(device_id, "mediaplayer", "player_play_music", &message)
            .await
    }

    /// 请求小爱调整音量。
    pub async fn set_volume(&self, device_id: &str, volume: u32) -> crate::Result<XiaoaiResponse> {
        let message = json!({
            "volume": volume,
            "media": "app_ios"
        })
        .to_string();

        self.ubus_call(device_id, "mediaplayer", "player_set_volume", &message)
            .await
    }

    /// 请求小爱执行文本。
    ///
    /// 效果和口头询问一样。
    pub async fn nlp(&self, device_id: &str, text: &str) -> crate::Result<XiaoaiResponse> {
        let message = json!({
            "tts": 1,
            "nlp": 1,
            "nlp_text": text
        })
        .to_string();

        self.ubus_call(device_id, "mibrain", "ai_service", &message)
            .await
    }

    /// 获取播放器的状态信息。
    ///
    /// 可能包含播放状态，音量和循环播放设置。
    pub async fn player_status(&self, device_id: &str) -> crate::Result<XiaoaiResponse> {
        let message = json!({"media": "app_ios"}).to_string();

        self.ubus_call(device_id, "mediaplayer", "player_get_play_status", &message)
            .await
    }

    /// 获取并解析播放器状态为结构化数据。
    ///
    /// 该方法会返回原始 JSON 数据以及一些常见字段（播放状态、音量、当前曲目信息、以及可能的最近对话文本）。
    /// 由于不同设备/固件返回结构可能不完全相同，解析采用宽松的搜索方式，尽量从返回的 JSON 中提取有用的字符串或数字字段。
    pub async fn player_status_parsed(&self, device_id: &str) -> crate::Result<PlayerStatus> {
        let resp = self.player_status(device_id).await?;
        
        // 解析 info 字段（如果它是一个 JSON 字符串）
        let mut data = resp.data;
        if let Some(info_str) = data.get("info").and_then(|v| v.as_str()) {
            // 尝试将 info 字符串解析为 JSON 对象
            if let Ok(info_json) = serde_json::from_str::<Value>(info_str) {
                // 用解析后的 JSON 替换原来的字符串
                if let Some(obj) = data.as_object_mut() {
                    obj.insert("info".to_string(), info_json);
                }
            }
        }
        
        Ok(PlayerStatus { raw: data })
    }

    /// 设置播放器的播放状态。
    pub async fn set_play_state(
        &self,
        device_id: &str,
        state: PlayState,
    ) -> crate::Result<XiaoaiResponse> {
        let action = match state {
            PlayState::Play => "play",
            PlayState::Pause => "pause",
            PlayState::Stop => "stop",
            PlayState::Toggle => "toggle",
        };
        let message = json!({"action": action, "media": "app_ios"}).to_string();

        self.ubus_call(device_id, "mediaplayer", "player_play_operation", &message)
            .await
    }

    /// 获取小爱音箱最近收到的消息和对话记录（旧方法 - 使用 ubus API）。
    ///
    /// 该方法使用 ubus 调用获取 NLP 结果，但由于小米服务器的数据保留时间极短，
    /// 通常会返回空结果。建议使用 `get_conversations` 方法作为替代。
    #[deprecated(note = "建议使用 get_conversations 方法，该方法使用更可靠的 conversation API")]
    pub async fn get_messages(&self, device_id: &str) -> crate::Result<Vec<MessageRecord>> {
        let resp = self.ubus_call(device_id, "mibrain", "nlp_result_get", "{}").await?;
        
        // 解析响应数据
        let data = &resp.data;
        trace!("获取消息响应: {}", data);
        
        let info = data["info"].as_str().unwrap_or("{}");
        trace!("info 字段: {}", info);
        
        let result: Value = serde_json::from_str(info)?;
        let result_array = result["result"].as_array();
        
        if result_array.is_none() {
            trace!("result 字段不是数组或不存在");
            return Ok(Vec::new());
        }

        let mut messages = Vec::new();
        for item in result_array.unwrap() {
            if !item.get("nlp").and_then(|v| v.as_str()).is_some() {
                trace!("跳过无效 item: {}", item);
                continue;
            }

            let nlp_str = item["nlp"].as_str().unwrap();
            let nlp: Value = serde_json::from_str(nlp_str)?;
            
            let request_id = nlp["meta"]["request_id"].as_str().unwrap_or("").to_string();
            let timestamp_ms = nlp["meta"]["timestamp"].as_i64().unwrap_or(0);
            
            let answers = nlp["response"]["answer"].as_array();
            if answers.is_none() {
                trace!("没有 answer 数组");
                continue;
            }

            let mut answer_records = Vec::new();
            for answer in answers.unwrap() {
                let domain = answer["domain"].as_str().unwrap_or("").to_string();
                let action = answer["action"].as_str().unwrap_or("").to_string();
                let content = answer["content"]["to_speak"].as_str().unwrap_or("").to_string();
                let question = answer["intention"]["query"].as_str().unwrap_or("").to_string();
                
                answer_records.push(AnswerRecord {
                    domain,
                    action,
                    content,
                    question,
                });
            }

            messages.push(MessageRecord {
                request_id,
                timestamp_ms,
                answers: answer_records,
            });
        }

        Ok(messages)
    }

    /// 获取小爱音箱的对话记录（推荐方法 - 使用 conversation API）。
    ///
    /// 该方法使用与 xiaomusic 相同的 API，能够更可靠地获取最近的对话记录。
    /// 可以指定 `limit` 参数来控制返回的对话数量（默认为 2）。
    /// 
    /// # 参数
    /// - `device_id`: 设备 ID
    /// - `hardware`: 设备型号（例如 "L05C", "L06A" 等）
    /// - `limit`: 返回的对话数量限制，默认为 2
    pub async fn get_conversations(
        &self,
        device_id: &str,
        hardware: &str,
        limit: Option<u32>,
    ) -> crate::Result<Vec<Conversation>> {
        let limit = limit.unwrap_or(2);
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();

        // 使用 xiaomusic 使用的 conversation API
        let url = format!(
            "https://userprofile.mina.mi.com/device_profile/v2/conversation?source=dialogu&hardware={}&timestamp={}&limit={}",
            hardware, timestamp, limit
        );

        // 从 cookie_store 中提取必要的 cookie 信息
        let cookie_store = self.cookie_store.lock().unwrap();
        let api_url = Url::parse(self.server.as_str())?;
        
        let mut service_token = String::new();
        let mut user_id = String::new();
        
        // 从 API 服务器的 cookie 中提取信息
        for cookie in cookie_store.matches(&api_url) {
            match cookie.name() {
                "serviceToken" => service_token = cookie.value().to_string(),
                "userId" => user_id = cookie.value().to_string(),
                _ => {}
            }
        }
        drop(cookie_store);
        
        trace!("使用 deviceId={}, userId={}, serviceToken 长度={}", device_id, user_id, service_token.len());
        
        // 构造 cookie 字符串
        let cookie_str = format!(
            "deviceId={}; serviceToken={}; userId={}",
            device_id, service_token, user_id
        );

        let http_resp = self
            .client
            .get(&url)
            .header("Cookie", cookie_str)
            .send()
            .await?;

        let status = http_resp.status();
        trace!("Conversation API HTTP状态: {}", status);
        
        if !status.is_success() {
            let body = http_resp.text().await?;
            trace!("Conversation API 错误响应: {}", body);
            return Err(crate::Error::Api(XiaoaiResponse {
                code: status.as_u16() as i64,
                message: format!("HTTP {}: {}", status, body),
                data: serde_json::Value::Null,
            }));
        }

        let resp = http_resp.json::<ConversationResponse>().await?;

        if resp.code != 0 {
            // 构造一个 XiaoaiResponse 用于返回错误
            let error_resp = XiaoaiResponse {
                code: resp.code as i64,
                message: format!("Conversation API 返回错误码: {}", resp.code),
                data: resp.data.clone(),
            };
            return Err(crate::Error::Api(error_resp));
        }

        // 解析 data 字段（可能是字符串形式的 JSON）
        let data_str = if let Some(data) = resp.data.as_str() {
            data
        } else if let Some(_data) = resp.data.as_object() {
            // 如果已经是对象，转回字符串再解析（保持一致性）
            &serde_json::to_string(&resp.data)?
        } else {
            return Ok(Vec::new());
        };

        let conversation_data: ConversationData = serde_json::from_str(data_str)?;
        
        if conversation_data.records.is_empty() {
            trace!("没有对话记录");
            return Ok(Vec::new());
        }

        Ok(conversation_data.records)
    }
}

/// 表示播放器的播放状态。
#[derive(Clone, Debug)]
pub enum PlayState {
    Play,
    Pause,
    Stop,
    /// 在播放和暂停之间切换。
    Toggle,
}

/// 小爱设备信息。
#[derive(Clone, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DeviceInfo {
    /// 设备 ID。
    ///
    /// 每个与设备相关的请求都会用 ID 指明对象。
    #[serde(rename = "deviceID")]
    pub device_id: String,

    /// 设备名称。
    pub name: String,

    /// 机型。
    pub hardware: String,
}

fn random_request_id() -> String {
    let mut request_id = random_id(30);
    request_id.insert_str(0, "app_ios_");

    request_id
}

/// 播放器状态的宽松表示。保留原始返回的 JSON 在 `raw` 字段中，
/// 并提供一些方便读取的可选字段。
#[derive(Clone, Debug, Deserialize)]
pub struct PlayerStatus {
    /// 原始返回的 data 字段（通常是 JSON 对象）
    #[serde(flatten)]
    pub raw: Value,
}

/// 小爱音箱的消息记录。
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageRecord {
    /// 请求 ID
    pub request_id: String,
    
    /// 时间戳（毫秒）
    pub timestamp_ms: i64,
    
    /// 回答列表
    pub answers: Vec<AnswerRecord>,
}

/// 小爱的回答记录。
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnswerRecord {
    /// 领域
    pub domain: String,
    
    /// 动作
    pub action: String,
    
    /// 回答内容
    pub content: String,
    
    /// 用户问题
    pub question: String,
}

/// Conversation API 的响应结构
#[derive(Clone, Debug, Deserialize)]
pub struct ConversationResponse {
    pub code: i32,
    pub data: Value,
}

/// 对话数据的包装结构
#[derive(Clone, Debug, Deserialize)]
pub struct ConversationData {
    pub records: Vec<Conversation>,
}

/// 单条对话记录
#[derive(Clone, Debug, Deserialize)]
pub struct Conversation {
    /// 时间戳（秒）
    pub time: i64,
    
    /// 用户的查询/问题
    #[serde(default)]
    pub query: String,
    
    /// 小爱的回答（可能有多个）
    #[serde(default)]
    pub answers: Vec<ConversationAnswer>,
}

/// 对话中的单个回答
#[derive(Clone, Debug, Deserialize)]
pub struct ConversationAnswer {
    /// TTS 信息（语音合成的文本）
    #[serde(default)]
    pub tts: Option<TtsInfo>,
}

/// TTS 文本信息
#[derive(Clone, Debug, Deserialize)]
pub struct TtsInfo {
    /// 要播报的文本内容
    #[serde(default)]
    pub text: String,
}
