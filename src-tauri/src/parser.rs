use reqwest::header::{HeaderMap, HeaderValue, COOKIE, REFERER, USER_AGENT};
use serde_json::Value;
use std::fmt::{Display, Formatter};
use tokio::sync::Mutex;

use crate::settings::AppSettings;

pub const BILIBILI_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
const LIVE_HOST: &str = "live.bilibili.com";

#[derive(Debug, Clone)]
pub struct LiveInfo {
    pub platform: String,
    pub room_id: String,
    pub short_id: Option<String>,
    pub uid: i64,
    pub anchor_name: String,
    pub room_title: String,
    pub cover_url: String,
    pub avatar_url: String,
    pub is_live: bool,
}

#[derive(Debug, Clone)]
pub struct StreamCandidate {
    pub url: String,
}

#[derive(Debug, Clone)]
pub struct StreamSelection {
    pub requested_qn: i64,
    pub actual_qn: i64,
    pub stream_type: String,
    pub candidates: Vec<StreamCandidate>,
}

#[derive(Debug, Clone)]
pub struct ParseError {
    message: String,
    rate_limited: bool,
}

impl ParseError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            rate_limited: false,
        }
    }

    fn rate_limited(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            rate_limited: true,
        }
    }

    fn http(status: reqwest::StatusCode) -> Self {
        let message = format!("Bilibili API 返回错误: HTTP {}", status);
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS
            || status == reqwest::StatusCode::FORBIDDEN
        {
            Self::rate_limited(message)
        } else {
            Self::new(message)
        }
    }

    pub fn is_rate_limited(&self) -> bool {
        self.rate_limited
    }
}

impl Display for ParseError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ParseError {}

struct ClientSession {
    proxy: String,
    client: reqwest::Client,
}

pub struct BilibiliParser {
    session: Mutex<Option<ClientSession>>,
}

impl BilibiliParser {
    pub fn new() -> Self {
        Self {
            session: Mutex::new(None),
        }
    }

    async fn client(&self, settings: &AppSettings) -> Result<reqwest::Client, ParseError> {
        let mut session = self.session.lock().await;
        if session
            .as_ref()
            .is_none_or(|current| current.proxy != settings.proxy)
        {
            *session = Some(ClientSession {
                proxy: settings.proxy.clone(),
                client: build_client(settings)?,
            });
        }
        Ok(session
            .as_ref()
            .expect("session initialized")
            .client
            .clone())
    }

    pub async fn parse_bilibili_url(
        &self,
        input: &str,
        settings: &AppSettings,
    ) -> Result<LiveInfo, ParseError> {
        let client = self.client(settings).await?;
        let input_room_id = resolve_input_room_id(&client, input, settings).await?;
        let init_url = format!(
            "https://api.live.bilibili.com/room/v1/Room/room_init?id={}",
            input_room_id
        );
        let init = request_json(&client, &init_url, settings, None).await?;
        ensure_api_success(&init, "解析直播间")?;
        let data = init
            .get("data")
            .and_then(Value::as_object)
            .ok_or_else(|| ParseError::new("Bilibili 返回的房间数据为空"))?;

        if data.get("is_hidden").and_then(Value::as_bool) == Some(true) {
            return Err(ParseError::new("该直播间已隐藏，无法录制"));
        }
        if data.get("is_locked").and_then(Value::as_bool) == Some(true) {
            return Err(ParseError::new("该直播间已锁定，无法录制"));
        }
        if data.get("encrypted").and_then(Value::as_bool) == Some(true) {
            return Err(ParseError::new("暂不支持需要密码的直播间"));
        }

        let room_id = data
            .get("room_id")
            .and_then(Value::as_i64)
            .ok_or_else(|| ParseError::new("无法获取真实房间号"))?;
        let uid = data.get("uid").and_then(Value::as_i64).unwrap_or_default();
        let short_id = data
            .get("short_id")
            .and_then(Value::as_i64)
            .filter(|id| *id > 0)
            .map(|id| id.to_string());
        // Bilibili 的 2 表示轮播；本应用只把 1 视作真正直播。
        let is_live = is_real_live_status(data.get("live_status").and_then(Value::as_i64));

        let referer = format!("https://live.bilibili.com/{}", room_id);
        let info_url = format!(
            "https://api.live.bilibili.com/room/v1/Room/get_info?room_id={}&from=room",
            room_id
        );
        let info = request_json(&client, &info_url, settings, Some(&referer)).await?;
        ensure_api_success(&info, "读取直播间信息")?;
        let room_data = info.get("data").unwrap_or(&Value::Null);
        let room_title = json_string(room_data, "title");
        let cover_url = {
            let cover = json_string(room_data, "user_cover");
            if cover.is_empty() {
                json_string(room_data, "keyframe")
            } else {
                cover
            }
        };

        let (anchor_name, avatar_url) = if uid > 0 {
            let master_url = format!(
                "https://api.live.bilibili.com/live_user/v1/Master/info?uid={}",
                uid
            );
            match request_json(&client, &master_url, settings, None).await {
                Ok(master) if api_code(&master) == 0 => {
                    let master_info = master.pointer("/data/info").unwrap_or(&Value::Null);
                    (
                        json_string(master_info, "uname"),
                        json_string(master_info, "face"),
                    )
                }
                _ => (String::new(), String::new()),
            }
        } else {
            (String::new(), String::new())
        };

        Ok(LiveInfo {
            platform: "bilibili".to_string(),
            room_id: room_id.to_string(),
            short_id,
            uid,
            anchor_name,
            room_title,
            cover_url,
            avatar_url,
            is_live,
        })
    }

    pub async fn get_stream_selection(
        &self,
        room_id: &str,
        settings: &AppSettings,
    ) -> Result<StreamSelection, ParseError> {
        let client = self.client(settings).await?;
        let requested_qn = normalize_quality(&settings.quality);
        let play_url = format!(
            "https://api.live.bilibili.com/xlive/web-room/v2/index/getRoomPlayInfo?room_id={}&protocol=0,1&format=0,1,2&codec=0&qn={}&platform=web&ptype=8",
            room_id, requested_qn
        );
        let response = request_json(
            &client,
            &play_url,
            settings,
            Some(&format!("https://live.bilibili.com/{}", room_id)),
        )
        .await?;
        ensure_api_success(&response, "获取直播流")?;
        if response
            .pointer("/data/live_status")
            .and_then(Value::as_i64)
            != Some(1)
        {
            return Err(ParseError::new("主播未开播"));
        }
        select_streams(&response, requested_qn)
    }
}

fn build_client(settings: &AppSettings) -> Result<reqwest::Client, ParseError> {
    let mut builder = reqwest::Client::builder()
        .user_agent(BILIBILI_UA)
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::limited(5));
    if !settings.proxy.trim().is_empty() {
        let proxy = reqwest::Proxy::all(settings.proxy.trim())
            .map_err(|error| ParseError::new(format!("代理配置无效: {}", error)))?;
        builder = builder.proxy(proxy);
    }
    builder
        .build()
        .map_err(|error| ParseError::new(format!("创建 HTTP 客户端失败: {}", error)))
}

async fn resolve_input_room_id(
    client: &reqwest::Client,
    input: &str,
    settings: &AppSettings,
) -> Result<String, ParseError> {
    let input = input.trim();
    if input.chars().all(|character| character.is_ascii_digit()) && !input.is_empty() {
        return Ok(input.to_string());
    }
    let candidate = if input.starts_with("live.bilibili.com/") || input.starts_with("b23.tv/") {
        format!("https://{}", input)
    } else {
        input.to_string()
    };
    let mut url = reqwest::Url::parse(&candidate)
        .map_err(|_| ParseError::new("请输入 Bilibili 直播间链接或数字房间号"))?;

    if matches!(url.host_str(), Some("b23.tv") | Some("www.b23.tv")) {
        let mut request = client.get(url.clone()).header(USER_AGENT, BILIBILI_UA);
        if !settings.cookie.trim().is_empty() {
            request = request.header(COOKIE, settings.cookie.trim());
        }
        let response = request
            .send()
            .await
            .map_err(|error| ParseError::new(format!("解析 Bilibili 分享链接失败: {}", error)))?;
        if !response.status().is_success() {
            return Err(ParseError::http(response.status()));
        }
        url = response.url().clone();
    }
    if url.host_str() != Some(LIVE_HOST) {
        return Err(ParseError::new("链接不是 Bilibili 直播间地址"));
    }
    extract_numeric_path_segment(&url).ok_or_else(|| ParseError::new("无法从链接中解析直播间号"))
}

fn extract_numeric_path_segment(url: &reqwest::Url) -> Option<String> {
    url.path_segments()?
        .find(|segment| !segment.is_empty() && segment.chars().all(|c| c.is_ascii_digit()))
        .map(ToString::to_string)
}

async fn request_json(
    client: &reqwest::Client,
    url: &str,
    settings: &AppSettings,
    referer: Option<&str>,
) -> Result<Value, ParseError> {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(BILIBILI_UA));
    if let Some(referer) = referer {
        headers.insert(
            REFERER,
            HeaderValue::from_str(referer).map_err(|_| ParseError::new("构建 Referer 失败"))?,
        );
    }
    if !settings.cookie.trim().is_empty() {
        headers.insert(
            COOKIE,
            HeaderValue::from_str(settings.cookie.trim())
                .map_err(|_| ParseError::new("Cookie 包含非法字符"))?,
        );
    }
    let response = client
        .get(url)
        .headers(headers)
        .send()
        .await
        .map_err(|error| ParseError::new(format!("请求 Bilibili API 失败: {}", error)))?;
    if !response.status().is_success() {
        return Err(ParseError::http(response.status()));
    }
    response
        .json::<Value>()
        .await
        .map_err(|error| ParseError::new(format!("解析 Bilibili API 响应失败: {}", error)))
}

fn api_code(value: &Value) -> i64 {
    value.get("code").and_then(Value::as_i64).unwrap_or(-1)
}

fn ensure_api_success(value: &Value, operation: &str) -> Result<(), ParseError> {
    let code = api_code(value);
    if code == 0 {
        return Ok(());
    }
    let message = value
        .get("message")
        .or_else(|| value.get("msg"))
        .and_then(Value::as_str)
        .unwrap_or("未知错误");
    let full = format!("{}失败: {} ({})", operation, message, code);
    if matches!(code, -352 | -412 | -509) {
        Err(ParseError::rate_limited(full))
    } else {
        Err(ParseError::new(full))
    }
}

fn json_string(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn normalize_quality(quality: &str) -> i64 {
    let parsed = quality.parse::<i64>().unwrap_or(10000);
    if matches!(parsed, 10000 | 400 | 250 | 150 | 80) {
        parsed
    } else {
        10000
    }
}

fn is_real_live_status(status: Option<i64>) -> bool {
    status == Some(1)
}

fn select_streams(response: &Value, requested_qn: i64) -> Result<StreamSelection, ParseError> {
    let streams = response
        .pointer("/data/playurl_info/playurl/stream")
        .and_then(Value::as_array)
        .ok_or_else(|| ParseError::new("Bilibili 未返回可用直播流"))?;
    let priorities = [("http_stream", "flv", "FLV"), ("http_hls", "ts", "HLS-TS")];

    for (protocol_name, format_name, stream_type) in priorities {
        let mut candidates = Vec::new();
        let mut actual_qn = 0;
        for stream in streams {
            if stream.get("protocol_name").and_then(Value::as_str) != Some(protocol_name) {
                continue;
            }
            for format in stream
                .get("format")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if format.get("format_name").and_then(Value::as_str) != Some(format_name) {
                    continue;
                }
                for codec in format
                    .get("codec")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if codec.get("codec_name").and_then(Value::as_str) != Some("avc") {
                        continue;
                    }
                    actual_qn = codec
                        .get("current_qn")
                        .and_then(Value::as_i64)
                        .unwrap_or(requested_qn);
                    let base_url = codec
                        .get("base_url")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    for url_info in codec
                        .get("url_info")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        let host = url_info
                            .get("host")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let extra = url_info
                            .get("extra")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if !host.is_empty() && !base_url.is_empty() {
                            candidates.push(StreamCandidate {
                                url: format!("{}{}{}", host, base_url, extra),
                            });
                        }
                    }
                }
            }
        }
        if !candidates.is_empty() {
            return Ok(StreamSelection {
                requested_qn,
                actual_qn,
                stream_type: stream_type.to_string(),
                candidates,
            });
        }
    }
    Err(ParseError::new("未找到兼容的 AVC 直播流"))
}

#[cfg(test)]
mod tests {
    use super::{
        extract_numeric_path_segment, is_real_live_status, normalize_quality, select_streams,
    };
    use serde_json::json;

    #[test]
    fn extracts_room_id_from_supported_live_urls() {
        for (url, expected) in [
            ("https://live.bilibili.com/6", "6"),
            ("https://live.bilibili.com/blanc/12345?live_from=1", "12345"),
        ] {
            let url = reqwest::Url::parse(url).unwrap();
            assert_eq!(
                extract_numeric_path_segment(&url).as_deref(),
                Some(expected)
            );
        }
    }

    #[test]
    fn quality_uses_known_qn_values_only() {
        assert_eq!(normalize_quality("400"), 400);
        assert_eq!(normalize_quality("invalid"), 10000);
    }

    #[test]
    fn stream_selection_prefers_flv_avc_and_preserves_cdn_order() {
        let response = json!({
            "data": {"playurl_info": {"playurl": {"stream": [{
                "protocol_name": "http_stream",
                "format": [{"format_name": "flv", "codec": [{
                    "codec_name": "avc", "current_qn": 250,
                    "base_url": "/live.flv?",
                    "url_info": [
                        {"host": "https://cdn-a.example", "extra": "token=a"},
                        {"host": "https://cdn-b.example", "extra": "token=b"}
                    ]
                }]}]
            }]}}}
        });
        let selected = select_streams(&response, 10000).unwrap();
        assert_eq!(selected.actual_qn, 250);
        assert_eq!(selected.stream_type, "FLV");
        assert_eq!(selected.candidates.len(), 2);
        assert!(selected.candidates[0]
            .url
            .starts_with("https://cdn-a.example"));
    }

    #[test]
    fn missing_playurl_is_not_recordable() {
        let response = json!({"data": {"live_status": 2, "playurl_info": null}});
        assert!(select_streams(&response, 10000).is_err());
    }

    #[test]
    fn only_real_live_status_is_recordable() {
        assert!(!is_real_live_status(Some(0)));
        assert!(is_real_live_status(Some(1)));
        assert!(!is_real_live_status(Some(2)));
        assert!(!is_real_live_status(None));
    }
}
