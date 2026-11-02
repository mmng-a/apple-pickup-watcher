//! 对 Apple 在线商店公开接口的访问。
//!
//! 这里只做「发请求 + 解析响应 + 分类错误」三件事，不含调度或界面逻辑，
//! 因此可以脱离应用单独测试。

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};
use tokio::time::Instant;

use crate::model::{Availability, Region, UnknownReason};
use crate::request_log::RequestLog;

/// 请求失败的分类。
///
/// 调度层据此决定是退避、告警还是直接放弃；界面层据此告诉用户到底出了什么事。
/// 每一类都能转成一个 [`UnknownReason`]，从而保证「失败」这件事在整条链路上
/// 一路都是「未知」，任何一环都无法把它悄悄降级成「无货」。
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// 请求被 Apple 边缘节点拦截，而不是门店真的没货。
    ///
    /// 上游项目正是死在这里：它使用的 `/shop/fulfillment-messages` 现在对任意
    /// 请求恒定返回 HTTP 541 加一个 128002 字节的「Page Not Found」HTML 拦截页
    /// （中国大陆站与美国站响应完全一致，且同一时刻 apple.com.cn 首页正常返回
    /// 200，可排除 IP 封禁）。上游把这个错误当成「无货」处理，于是所有用户看到
    /// 的都是一屏永远不会变的「无货」。
    #[error("Appleがリクエストを拒否しました：{0}")]
    Blocked(String),

    /// 触发了频率限制，应当退避后重试。
    #[error("リクエスト頻度が制限されました：{0}")]
    RateLimited(String),

    /// 响应能解析成 JSON，但结构与预期不符，通常意味着 Apple 又改了接口。
    #[error("APIの応答形式が想定と異なります：フィールド {field} の値は {raw:?} です")]
    SchemaDrift { field: String, raw: String },

    /// Apple 明确返回了一条业务错误信息。
    #[error("Appleがエラーを返しました：{0}")]
    Apple(String),

    /// 网络层面的失败。
    #[error("通信に失敗しました：{0}")]
    Transport(String),
}

impl ApiError {
    /// 转成状态机能直接采用的未知原因。
    ///
    /// 这个转换是单向且全覆盖的：**没有任何一条 `ApiError` 能变成
    /// `InStock` 或 `OutOfStock`**。上游那个致命缺陷在这里从类型上就写不出来。
    pub fn into_unknown_reason(self) -> UnknownReason {
        match self {
            Self::Blocked(detail) => UnknownReason::Blocked { detail },
            Self::RateLimited(_) => UnknownReason::RateLimited,
            Self::SchemaDrift { field, raw } => UnknownReason::SchemaDrift { field, raw },
            Self::Apple(message) => UnknownReason::AppleError { message },
            Self::Transport(detail) => UnknownReason::Transport { detail },
        }
    }

    /// 是否值得立刻重试。
    ///
    /// 结构不符和业务错误重试多少次结果都一样；**被拦截也不重试**：拦截是风控
    /// 评分的结果，几秒后再打一次只会把评分推得更高，issue #3 里六家门店一轮
    /// 就能发出十八次请求，正是这么来的。被拦后的处置是冷却，见
    /// [`AppleClient::pickup_message`]。
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::RateLimited(_) | Self::Transport(_))
    }
}

/// 与 Apple 说话时的一整套请求特征。
///
/// 这些头必须彼此一致：自称 Chrome 153 却不带任何 Chrome 必发的 `sec-ch-ua`，
/// 或者带着 jQuery 时代的 `X-Requested-With`，都是边缘风控一眼可辨的脚本特征。
/// 所以它们收在一个带名字的档案里整体切换，而不是散落各处各改各的；诊断输出
/// 也能写清「用的是哪一套」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestProfile {
    /// 档案名，带采集日期，出现在诊断输出里。
    pub name: &'static str,
    pub user_agent: String,
    /// `sec-ch-ua` 三件套；`None` 表示不发。
    pub client_hints: Option<ClientHints>,
    /// 是否发送 `sec-fetch-*`（Fetch Metadata）。
    pub fetch_metadata: bool,
    /// 取货接口请求的 `Accept`。
    pub api_accept: String,
    /// 是否发送 `X-Requested-With: XMLHttpRequest`。
    pub x_requested_with: bool,
    /// 是否附带 Apple 商店前端自己加的 `x-skip-redirect` 与 `x-aos-ui-fetch-call-1`。
    /// 后者的生成规则未经证实，只是仿照样例格式，因此默认不发。
    pub apple_extras: bool,
}

/// Chrome 每个请求都会带的低熵 client hints。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientHints {
    pub ua: String,
    pub mobile: String,
    pub platform: String,
}

impl RequestProfile {
    /// 默认档案：macOS 上的 Chrome 149，头的形状按 2026-09-13 从真实 Chrome 购买页
    /// 抓到的取货请求来。
    ///
    /// 版本号取 149 而不是当天最新的 153，是为了和 [`Transport::ChromeTls`] 复刻的
    /// TLS / HTTP/2 指纹（wreq-util 的 Chrome 149 档案）保持一致：自称一个版本、
    /// 握手却是另一个版本的样子，正是要避免的那类不一致。更新时整套一起换
    /// （UA 主版本、`sec-ch-ua` 品牌列表、仿真档案），并改档案名。不要每次请求
    /// 随机换 UA，那本身就是特征。
    pub fn chrome() -> Self {
        Self {
            name: "chrome-149-macos-20260913",
            user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
                         (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36"
                .into(),
            client_hints: Some(ClientHints {
                ua: r#""Google Chrome";v="149", "Chromium";v="149", "Not)A;Brand";v="24""#.into(),
                mobile: "?0".into(),
                platform: r#""macOS""#.into(),
            }),
            fetch_metadata: true,
            api_accept: "*/*".into(),
            x_requested_with: false,
            apple_extras: false,
        }
    }

    /// v0.4.1 及以前的请求特征。只用于诊断对照，不要用作默认。
    pub fn legacy() -> Self {
        Self {
            name: "legacy-0.4.1",
            user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
                         (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36"
                .into(),
            client_hints: None,
            fetch_metadata: false,
            api_accept: "application/json, text/javascript, */*; q=0.01".into(),
            x_requested_with: true,
            apple_extras: false,
        }
    }
}

impl Default for RequestProfile {
    fn default() -> Self {
        Self::chrome()
    }
}

/// 暖场取哪个页面。
///
/// 默认取购物袋页：它是动态页面，一次就把 `dssid2`、`as_dc`、`dssf` 等商店会话
/// cookie 全发下来。购买页走 CDN 缓存（`cache-control: public, max-age=120`），
/// 只发一个 `geo`，攒不到会话 —— 2026-09-13 用 `apw doctor` 对照过：购物袋页暖场后
/// 罐里六个 cookie，购买页暖场后只有一个。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarmPage {
    /// 购物袋页（默认）。
    Bag,
    /// 该地区的默认购买页。只发 `geo`，留给诊断对照。
    BuyPage,
    /// 不暖场。诊断用：验证「不带 cookie」这一变量。
    None,
}

/// 传输层实现：决定 TLS 握手与 HTTP/2 帧长什么样。
///
/// 请求头可以逐个对齐，握手指纹却是库决定的：rustls + h2 的 ClientHello 和
/// HTTP/2 SETTINGS 与任何浏览器都不一样，而这正是边缘风控打分最常用的信号，
/// 也最能解释「同一台电脑浏览器正常、程序被拦」。`ChromeTls` 用 BoringSSL 复刻
/// Chrome 的握手（wreq / wreq-util 的 Chrome 149 档案），请求头仍由
/// [`RequestProfile`] 控制，两边版本一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// rustls + h2（reqwest）。指纹与浏览器不同，保留给对照和没有 `chrome-tls` 的构建。
    Rustls,
    /// BoringSSL 复刻 Chrome 的 TLS 与 HTTP/2 指纹。需要 `chrome-tls` feature（默认开启）。
    ChromeTls,
}

impl Transport {
    /// 本次构建能用的默认传输：编译进了 `chrome-tls` 就用它。
    pub fn default_for_build() -> Self {
        if cfg!(feature = "chrome-tls") {
            Self::ChromeTls
        } else {
            Self::Rustls
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Rustls => "rustls",
            Self::ChromeTls => "chrome-tls",
        }
    }
}

/// 库存接口响应体的读取上限。一次查询的正常响应只有几 KB，拦截页也不过 128 KB，
/// 4 MB 足够；再大就不是我们要的东西了。
const MAX_RESPONSE_BYTES: usize = 4 << 20;

/// 暖场页的读取上限。读掉响应体是为了让连接能被复用；超过上限直接停下。
const MAX_WARM_BYTES: usize = 8 << 20;

/// 暖场失败后的最小重试间隔。每个门店任务都各自再试一遍暖场，只会把突发放大。
const WARM_RETRY_INTERVAL: Duration = Duration::from_secs(60);

/// 被拦后的冷却时长：首次 5 分钟，冷却结束后的探测再次被拦，依次延长到 10、20、30 分钟。
///
/// 这是保守的客户端策略，不是 Apple 已知的封禁时长。被拦之后继续以原频率发请求，
/// 只会让风控评分越来越高；issue #3 里「一旦 541 就一直 541」正是这个样子。
const BLOCK_COOLDOWNS: [Duration; 4] = [
    Duration::from_secs(5 * 60),
    Duration::from_secs(10 * 60),
    Duration::from_secs(20 * 60),
    Duration::from_secs(30 * 60),
];

/// 保留最近多少条出站请求记录，供诊断命令和日志使用。
const RECENT_RECORDS: usize = 256;

/// 连续分配给同一线路的取货请求数上限；暖场不计入。
const REQUESTS_PER_ROUTE: usize = 20;

#[derive(Debug)]
struct RouteSelection {
    cursor: usize,
    requests: usize,
}

/// 各地区被拦后的冷却账本。纯状态机，不碰网络，便于单测。
#[derive(Debug, Default)]
struct BlockTracker {
    states: HashMap<String, BlockState>,
}

impl BlockTracker {
    /// 该地区现在能不能发请求。`Ok(true)` 表示这次是冷却结束后放出的那一次探测。
    fn admit(&mut self, locale: &str, now: Instant) -> Result<bool, ApiError> {
        let Some(state) = self.states.get_mut(locale) else {
            return Ok(false);
        };
        if now < state.until {
            return Err(ApiError::Blocked(format!(
                "HTTP 541により待機中です。{}後に再試行します",
                human_duration(state.until - now)
            )));
        }
        if state.probing {
            return Err(ApiError::Blocked(
                "待機が終了したため、接続できるか1回確認しています".into(),
            ));
        }
        state.probing = true;
        Ok(true)
    }

    /// 登记一次被拦，返回这次采用的冷却时长。
    ///
    /// 只有**探测**再被拦才升档。同一轮里并发在飞的其他请求跟着被拦，是同一次
    /// 事件：v0.4.2-beta 曾把它们各算一次，六家门店一轮就把冷却从 5 分钟直接
    /// 抬到 30 分钟，用户看到的比不冷却还糟。
    fn record(&mut self, locale: &str, now: Instant, probing: bool) -> Duration {
        let state = self.states.entry(locale.to_string()).or_insert(BlockState {
            until: now,
            level: 0,
            probing: false,
        });
        if probing {
            state.level = (state.level + 1).min(BLOCK_COOLDOWNS.len() - 1);
        }
        state.probing = false;
        let cooldown = BLOCK_COOLDOWNS[state.level];
        state.until = state.until.max(now + cooldown);
        cooldown
    }

    /// 一次成功的查询。只有探测成功才算解封；冷却期内并发漏过去的成功不算数 ——
    /// 同一轮里有的通过有的被拦，说明额度正卡在边上，这时清掉冷却只会让下一轮
    /// 再撞一次。
    fn clear(&mut self, locale: &str, probing: bool) {
        if probing {
            self.states.remove(locale);
        }
    }

    fn end_probe(&mut self, locale: &str) {
        if let Some(state) = self.states.get_mut(locale) {
            state.probing = false;
        }
    }

    /// 只看不改：这条线路对该地区此刻能不能用。挑线路时先用它排序，选定了再
    /// [`Self::admit`]，避免把没选中的线路也标成「探测中」。
    fn availability(&self, locale: &str, now: Instant) -> RouteAvailability {
        match self.states.get(locale) {
            None => RouteAvailability::Free,
            Some(state) if now < state.until => RouteAvailability::Cooling(state.until - now),
            Some(state) if state.probing => RouteAvailability::Probing,
            Some(_) => RouteAvailability::Free,
        }
    }
}

/// 某条线路对某地区此刻的可用性。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RouteAvailability {
    Free,
    /// 冷却刚结束，已有一次探测在飞，先别往这条线路再塞请求。
    Probing,
    Cooling(Duration),
}

/// 一条出口线路：直连，或经某个代理。
///
/// 每条线路各有自己的传输层、cookie 罐、暖场状态和冷却账本。
struct Route {
    /// 给日志和请求记录看的名字：`direct`，或 `proxy#1`（不含账号密码）。
    label: String,
    /// 代理线路的地址，去掉了账号密码，出错时告诉用户是哪个代理连不上；直连为 `None`。
    endpoint: Option<String>,
    http: Http,
    warm: Mutex<HashMap<String, WarmState>>,
    blocks: Mutex<BlockTracker>,
}

impl Route {
    fn build(config: &ClientConfig, label: String, proxy: Option<&str>) -> Result<Self, ApiError> {
        Ok(Self {
            label,
            endpoint: proxy.map(redact_proxy),
            http: Http::build(config, proxy)?,
            warm: Mutex::new(HashMap::new()),
            blocks: Mutex::new(BlockTracker::default()),
        })
    }

    fn is_proxy(&self) -> bool {
        self.label != "direct"
    }
}

impl std::fmt::Debug for Route {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}[{}]", self.label, self.http.label())
    }
}

/// 直连排第一，之后每个代理一条线路。空白项跳过；地址不合法就整体报错，
/// 而不是悄悄少一条线路让用户以为代理在生效。
fn build_routes(config: &ClientConfig, proxies: &[String]) -> Result<Vec<Arc<Route>>, ApiError> {
    let mut routes = vec![Arc::new(Route::build(config, "direct".into(), None)?)];
    let mut n = 0usize;
    for proxy in proxies {
        let proxy = proxy.trim();
        if proxy.is_empty() {
            continue;
        }
        validate_proxy(proxy)?;
        n += 1;
        routes.push(Arc::new(Route::build(
            config,
            format!("proxy#{n}"),
            Some(proxy),
        )?));
    }
    Ok(routes)
}

/// 代理地址先由我们自己把关，再交给传输层。
///
/// 两个传输层对坏地址的态度不一样：reqwest 对没有协议的字符串会自动补上
/// `http://` 照单全收，wreq 则直接报错。不先统一，「代理地址无效」这个错误就
/// 只在某一种构建里出现。规则与 [`crate::config::normalize_proxies`] 一致。
fn validate_proxy(proxy: &str) -> Result<(), ApiError> {
    let invalid = |why: &str| {
        ApiError::Transport(format!(
            "プロキシURL {} が無効です：{why}",
            redact_proxy(proxy)
        ))
    };
    let url = reqwest::Url::parse(proxy).map_err(|e| invalid(&e.to_string()))?;
    if !matches!(url.scheme(), "http" | "https" | "socks5" | "socks5h") {
        return Err(invalid("http、https、socks5、socks5hのみ対応しています"));
    }
    if url.host_str().is_none() {
        return Err(invalid("ホスト名がありません"));
    }
    Ok(())
}

/// 代理地址去掉账号密码后的样子，只用于错误信息与日志。
fn redact_proxy(proxy: &str) -> String {
    match reqwest::Url::parse(proxy) {
        Ok(url) => match (url.host_str(), url.port()) {
            (Some(host), Some(port)) => format!("{}://{host}:{port}", url.scheme()),
            (Some(host), None) => format!("{}://{host}", url.scheme()),
            _ => format!("{}://…", url.scheme()),
        },
        Err(_) => "（解析できないURL）".into(),
    }
}

/// 代理线路这一轮出的网络错误，写成用户能照着排查的话。
///
/// 底层库连不上代理时只说一句 `client error (ProxyConnect)`，用户看不出是代理
/// 软件没开、端口填错，还是代理自己的出口不通。这里点名是哪个代理、该查什么，
/// 原始报错接在后面备查。
fn describe_proxy_failure(route: &Route, err: &ApiError) -> String {
    let raw = match err {
        ApiError::Transport(detail) => detail.as_str(),
        _ => "",
    };
    let at = route.endpoint.as_deref().unwrap_or("プロキシ");
    if raw.contains("ProxyConnect") {
        format!(
            "接続先 {}：プロキシ {at} に接続できません。プロキシが起動しているか、URLとポートが正しいか確認してください（詳細：{raw}）",
            route.label
        )
    } else {
        format!("接続先 {}（{at}）：{err}", route.label)
    }
}

/// 一轮里试过的线路都失败时，把每条线路的结果合成一条错误。
///
/// 错误的种类保持不变：只要有一条线路是被 Apple 拦下的，就仍按「被拦截」报，
/// 界面上「换条网络」的建议照样出现；全是网络错误才按网络错误报。只试了一条
/// 线路时原样返回，单线路的提示和以前一字不差。
fn summarize_round(last: ApiError, outcomes: &[String]) -> ApiError {
    if outcomes.len() < 2 {
        return last;
    }
    let text = outcomes.join("；");
    if matches!(last, ApiError::Blocked(_))
        || outcomes.iter().any(|o| o.contains("待機に入りました"))
    {
        ApiError::Blocked(text)
    } else {
        match last {
            ApiError::Transport(_) => ApiError::Transport(text),
            other => other,
        }
    }
}

/// 一次出站取货请求的全部参数，打包传给 [`AppleClient::query_routes`]。
struct Outbound<'a> {
    url: &'a str,
    query: &'a [(String, String)],
    region: &'a Region,
    referer: &'a str,
    scope: &'a PickupScope,
    parts: usize,
}

/// 一次取货查询的范围：指定门店，或某个地点周边的所有门店。
#[derive(Debug, Clone)]
enum PickupScope {
    Store(String),
    Nearby(String),
}

impl PickupScope {
    fn query_param(&self) -> (String, String) {
        match self {
            Self::Store(store) => ("store".into(), store.clone()),
            Self::Nearby(location) => ("location".into(), location.clone()),
        }
    }

    fn store(&self) -> Option<String> {
        match self {
            Self::Store(store) => Some(store.clone()),
            Self::Nearby(_) => None,
        }
    }

    fn location(&self) -> Option<String> {
        match self {
            Self::Store(_) => None,
            Self::Nearby(location) => Some(location.clone()),
        }
    }
}

/// 客户端配置。
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// 任意两次出站请求之间的最小间隔，用于全局限速。
    pub min_interval: Duration,
    /// 单次调用内部的最大重试次数（不含首次请求）。
    ///
    /// 只对网络失败和限流生效；被拦截（HTTP 541 / 403）一律不重试，
    /// 见 [`ApiError::is_retryable`]。
    pub max_retries: u32,
    /// 单次请求的总超时。
    pub timeout: Duration,
    /// 请求特征档案。
    pub profile: RequestProfile,
    /// 暖场策略。
    pub warm_page: WarmPage,
    /// 传输层实现。
    pub transport: Transport,
    /// 代理地址列表（`http://`、`https://`、`socks5://`、`socks5h://`），可以带账号密码。
    ///
    /// 每 20 次取货请求换下一条线路；被拦或连接失败时提前切换。
    /// 直连也参与轮换。为空则只有直连。
    pub proxies: Vec<String>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            min_interval: Duration::from_millis(500),
            max_retries: 2,
            timeout: Duration::from_secs(15),
            profile: RequestProfile::chrome(),
            warm_page: WarmPage::Bag,
            transport: Transport::default_for_build(),
            proxies: Vec::new(),
        }
    }
}

/// 一次真实出站请求的记录，供诊断命令与日志使用。只记 cookie 的名字，不记值。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestRecord {
    /// Unix 毫秒。
    pub at_ms: u64,
    /// `warm`（暖场）或 `pickup`（取货查询）。
    pub kind: &'static str,
    /// 用的传输层：`rustls` 或 `chrome-tls`。
    pub transport: &'static str,
    /// 走的出口线路：`direct`，或 `proxy#1`、`proxy#2`……（不含代理的账号密码）。
    pub route: String,
    pub locale: String,
    pub store: Option<String>,
    /// 按地点查询时的 `location` 参数；按门店查询时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    /// 这次请求携带的零件号数量（含搭档表带）。
    pub parts: usize,
    pub status: Option<u16>,
    /// 实际协商出的 HTTP 版本，如 `HTTP/2.0`。
    pub http_version: Option<String>,
    pub duration_ms: u64,
    /// `ok` / `blocked` / `rate_limited` / `http_error` / `not_json` / `no_cookie` / `transport`。
    pub outcome: String,
    /// 发送前 cookie 罐里对该地址生效的 cookie 名。
    pub cookies_sent: Vec<String>,
    /// 响应处理后对取货接口生效的 cookie 名。
    pub cookies_after: Vec<String>,
    /// 跟随重定向后的最终地址。
    pub final_url: Option<String>,
}

/// 某地区的暖场状态。
#[derive(Debug, Default)]
struct WarmState {
    /// 暖场页取回来了、而且罐里真的有对取货接口生效的 cookie。
    warmed: bool,
    last_attempt: Option<Instant>,
    failures: u32,
}

/// 某地区被拦后的冷却状态。
#[derive(Debug)]
struct BlockState {
    until: Instant,
    /// 已用到 [`BLOCK_COOLDOWNS`] 的第几档。
    level: usize,
    /// 冷却结束后是否已有一次探测在飞。同一时刻只放一个探测出去。
    probing: bool,
}

/// Apple 商店接口客户端，可跨任务共享。
///
/// 必须复用同一个实例：上游为每次查询都新建一个 HTTP 客户端，连接池完全无法
/// 复用，空闲连接持续堆积，配合它 500ms 一轮的轮询，几小时就能涨到十几 GB 内存。
/// `reqwest::Client` 内部就是 `Arc`，克隆代价极低，共享的是同一个连接池。
#[derive(Debug, Clone)]
pub struct AppleClient {
    /// 出口线路：直连排第一，之后每个代理一条。每条线路自带传输层与 cookie 罐
    /// （罐由我们持有而不是用库里隐藏的内部罐，是为了能查得到里面到底有没有
    /// 东西 —— 暖场之后要确认真的攒到了 cookie）、暖场状态和冷却账本。
    ///
    /// 放在 `RwLock` 里是为了让用户改完代理设置不用重启：[`Self::set_proxies`]
    /// 整体换掉线路列表，正在飞的请求拿着旧线路的 `Arc` 跑完即可。
    routes: Arc<RwLock<Vec<Arc<Route>>>>,
    /// 线路与已分配次数一起更新，避免并发查询超过每条线路的 20 次上限。
    selection: Arc<Mutex<RouteSelection>>,
    config: ClientConfig,
    /// 上一次出站请求的时刻，用于全局限速（不分线路：突发本身就不该有）。
    last_sent: Arc<Mutex<Option<Instant>>>,
    /// 最近的出站请求记录。
    recent: Arc<Mutex<VecDeque<RequestRecord>>>,
    request_log: Option<Arc<std::sync::Mutex<Option<RequestLog>>>>,
}

impl AppleClient {
    pub fn new(config: ClientConfig) -> Result<Self, ApiError> {
        let routes = build_routes(&config, &config.proxies)?;
        let first = usize::from(routes.len() > 1);
        Ok(Self {
            routes: Arc::new(RwLock::new(routes)),
            selection: Arc::new(Mutex::new(RouteSelection {
                cursor: first,
                requests: 0,
            })),
            config,
            last_sent: Arc::new(Mutex::new(None)),
            recent: Arc::new(Mutex::new(VecDeque::with_capacity(RECENT_RECORDS))),
            request_log: None,
        })
    }

    /// 在克隆客户端前启用持久日志。当前文件与上一份各不超过 1 MiB；不保存敏感字段。
    pub fn enable_request_log(&mut self, path: &Path) -> std::io::Result<()> {
        self.request_log = Some(Arc::new(std::sync::Mutex::new(Some(RequestLog::new(
            path,
        )?))));
        Ok(())
    }

    /// 换一套代理，立即生效；重建会话、冷却和轮换计数。地址解析失败时保留原线路。
    pub async fn set_proxies(&self, proxies: &[String]) -> Result<(), ApiError> {
        let routes = build_routes(&self.config, proxies)?;
        let first = usize::from(routes.len() > 1);
        let mut current = self.routes.write().await;
        let mut selection = self.selection.lock().await;
        *current = routes;
        *selection = RouteSelection {
            cursor: first,
            requests: 0,
        };
        Ok(())
    }

    /// 当前线路的名字，直连在前：`["direct", "proxy#1", …]`。
    pub async fn route_labels(&self) -> Vec<String> {
        self.routes
            .read()
            .await
            .iter()
            .map(|r| r.label.clone())
            .collect()
    }

    /// 直连那条线路。契约测试、诊断命令看 cookie 和协商结果时用它。
    async fn primary(&self) -> Arc<Route> {
        let routes = self.routes.read().await;
        Arc::clone(routes.first().expect("直接接続が1つ以上必要です"))
    }

    /// 指定地区的所有线路是否都在被拦后的冷却期内；接续探测不算冷却。
    pub async fn all_routes_cooling(&self, locale: &str) -> bool {
        let routes = self.routes.read().await;
        let now = Instant::now();
        for route in routes.iter() {
            if !matches!(
                route.blocks.lock().await.availability(locale, now),
                RouteAvailability::Cooling(_)
            ) {
                return false;
            }
        }
        !routes.is_empty()
    }

    /// 当前使用的请求特征档案。
    pub fn profile(&self) -> &RequestProfile {
        &self.config.profile
    }

    /// 当前使用的传输层。
    pub fn transport(&self) -> Transport {
        self.config.transport
    }

    /// 最近的出站请求记录，按时间先后排列。
    pub async fn recent_requests(&self) -> Vec<RequestRecord> {
        self.recent.lock().await.iter().cloned().collect()
    }

    async fn push_record(&self, record: RequestRecord) {
        let mut recent = self.recent.lock().await;
        if let Some(log) = &self.request_log {
            let log = Arc::clone(log);
            let record = record.clone();
            // 文件操作不占用异步线程；写盘失败不应让库存查询失败或反复刷报错。
            let _ = tokio::task::spawn_blocking(move || {
                let mut log = log.lock().unwrap_or_else(|err| err.into_inner());
                if let Some(writer) = log.as_ref()
                    && let Err(err) = writer.append(&record)
                {
                    eprintln!("通信ログを保存できないため、保存を停止しました：{err}");
                    *log = None;
                }
            })
            .await;
        }
        if recent.len() >= RECENT_RECORDS {
            recent.pop_front();
        }
        recent.push_back(record);
    }

    /// 罐里对某个地址生效的 cookie 名。
    pub async fn cookie_names_for(&self, url: &str) -> Vec<String> {
        self.primary().await.http.cookie_names_for(url)
    }

    /// 这个地区当前攒到的 cookie，没有则返回 `None`。契约测试用；**不要打印它的值**。
    pub async fn cookies_for(&self, region: &Region) -> Option<String> {
        self.primary()
            .await
            .http
            .cookie_header_for(&region.pickup_message_url())
    }

    /// 确保这个地区的 cookie 已经攒上了。
    ///
    /// # 为什么非做不可
    ///
    /// Apple 的边缘节点会对**没带 cookie** 的取货查询下手。issue #3 的报告者在
    /// 同一个浏览器里做了十轮成对对照，只差带不带 cookie：
    ///
    /// ```text
    /// 带 cookie  10/10 全部 200
    /// 不带 cookie 8/10  返回 541
    /// ```
    ///
    /// 而这件事**只在受审查的网络上才看得出来**：在没被盯上的网络里，带不带
    /// cookie 都是 200，怎么对照都测不出差别。所以别拿「我这里两种都正常」
    /// 当反证 —— 这个假设正是这么被误杀过一次的。
    ///
    /// # 三条纪律
    ///
    /// 1. **同一时刻只暖一次**：锁在整个请求期间持有，并发的门店任务等同一个结果。
    /// 2. **页面 2xx 不算数**：罐里真的有对取货接口生效的 cookie 才算暖好。
    /// 3. **失败不影响查询**：暖不上时照常发查询，只是 [`WARM_RETRY_INTERVAL`] 内
    ///    不再重试暖场。让一次辅助请求的失败去决定库存判定，正是这个项目最不该
    ///    有的东西。
    async fn ensure_warm(&self, route: &Route, region: &Region) {
        let url = match self.config.warm_page {
            WarmPage::Bag => region.bag_url(),
            WarmPage::BuyPage => region.default_buy_page_url(),
            WarmPage::None => return,
        };

        let mut states = route.warm.lock().await;
        let state = states.entry(region.locale.to_string()).or_default();
        if state.warmed {
            return;
        }
        if let Some(at) = state.last_attempt
            && at.elapsed() < WARM_RETRY_INTERVAL
        {
            return;
        }
        state.last_attempt = Some(Instant::now());

        self.throttle().await;
        let started = Instant::now();
        let mut record = RequestRecord {
            at_ms: now_ms(),
            kind: "warm",
            transport: route.http.label(),
            route: route.label.clone(),
            locale: region.locale.to_string(),
            store: None,
            location: None,
            parts: 0,
            status: None,
            http_version: None,
            duration_ms: 0,
            outcome: String::new(),
            cookies_sent: route.http.cookie_names_for(&url),
            cookies_after: Vec::new(),
            final_url: None,
        };

        let sent = route
            .http
            .fetch(
                &url,
                &[],
                navigation_headers(&self.config.profile, region),
                MAX_WARM_BYTES,
            )
            .await;
        match sent {
            Ok(fetched) => {
                // 响应体已经读掉（连接才能复用），内容本身用不上。
                let status = fetched.status;
                record.status = Some(status);
                record.http_version = Some(fetched.version);
                record.final_url = Some(fetched.final_url);
                let names = route.http.cookie_names_for(&region.pickup_message_url());
                let ok = (200..300).contains(&status) && !names.is_empty();
                record.cookies_after = names;
                record.outcome = if ok {
                    "ok".into()
                } else if (200..300).contains(&status) {
                    "no_cookie".into()
                } else {
                    "http_error".into()
                };
                if ok {
                    state.warmed = true;
                    state.failures = 0;
                } else {
                    state.failures = state.failures.saturating_add(1);
                }
            }
            Err(err) => {
                record.outcome = format!("transport: {err}");
                state.failures = state.failures.saturating_add(1);
            }
        }
        record.duration_ms = started.elapsed().as_millis() as u64;
        drop(states);
        self.push_record(record).await;
    }

    /// 忘掉某地区的暖场标记，下一次会重新取页面。
    ///
    /// 被拦截时调用。cookie 会过期，也会被边缘节点作废；一直拿着一份不再被认可
    /// 的 cookie 反复重试，只会一直被拦。只清标记不清罐：重新取页面会刷新会话。
    async fn forget_warm(&self, route: &Route, region: &Region) {
        if let Some(state) = route.warm.lock().await.get_mut(region.locale) {
            state.warmed = false;
            state.last_attempt = None;
        }
    }

    /// 登记一次被拦，返回这次采用的冷却时长。见 [`BlockTracker::record`]。
    async fn record_block(&self, route: &Route, region: &Region, probing: bool) -> Duration {
        route
            .blocks
            .lock()
            .await
            .record(region.locale, Instant::now(), probing)
    }

    async fn clear_block(&self, route: &Route, region: &Region, probing: bool) {
        route.blocks.lock().await.clear(region.locale, probing);
    }

    async fn end_probe(&self, route: &Route, region: &Region) {
        route.blocks.lock().await.end_probe(region.locale);
    }

    /// 给这次请求挑一条线路，跳过本次调用已经试过的。
    ///
    /// 跳过冷却中和探测中的线路，从当前线路开始按顺序查找可用线路。选定之后才
    /// [`BlockTracker::admit`]，抢探测名额输给并发的另一次调用就顺延到下一条。
    ///
    /// 一条线路也选不出来时返回 `Err`：只有直连时沿用原来的冷却提示；有多条线路
    /// 时说明每条各自还要等多久。
    async fn pick_route(
        &self,
        region: &Region,
        tried: &[String],
    ) -> Result<(Arc<Route>, bool), ApiError> {
        let routes = self.routes.read().await;
        let n = routes.len().max(1);
        let mut selection = self.selection.lock().await;
        let rotate = selection.requests >= REQUESTS_PER_ROUTE;
        let start = (selection.cursor + usize::from(rotate)) % n;
        let now = Instant::now();

        let mut unavailable: Vec<(String, RouteAvailability)> = Vec::new();
        for k in 0..routes.len() {
            let i = (start + k) % n;
            let route = &routes[i];
            if tried.contains(&route.label) {
                continue;
            }
            let availability = route.blocks.lock().await.availability(region.locale, now);
            match availability {
                RouteAvailability::Free => {
                    if let Ok(probing) = route
                        .blocks
                        .lock()
                        .await
                        .admit(region.locale, Instant::now())
                    {
                        if rotate || selection.cursor != i {
                            selection.requests = 0;
                        }
                        selection.cursor = i;
                        selection.requests += 1;
                        return Ok((Arc::clone(route), probing));
                    }
                }
                other => unavailable.push((route.label.clone(), other)),
            }
        }
        // 只有直连一条：沿用原来的提示，桌面端和测试都认这两句话。
        if routes.len() == 1 {
            let route = &routes[0];
            return match route
                .blocks
                .lock()
                .await
                .admit(region.locale, Instant::now())
            {
                Ok(probing) => Ok((Arc::clone(route), probing)),
                Err(err) => Err(err),
            };
        }
        let mut parts: Vec<String> = Vec::new();
        let mut soonest: Option<Duration> = None;
        for (label, state) in &unavailable {
            match state {
                RouteAvailability::Cooling(left) => {
                    parts.push(format!("{label} {}", human_duration(*left)));
                    soonest = Some(soonest.map_or(*left, |s| s.min(*left)));
                }
                RouteAvailability::Probing => parts.push(format!("{label} 接続確認中")),
                RouteAvailability::Free => {}
            }
        }
        for label in tried {
            if !parts.iter().any(|p| p.starts_with(label.as_str())) {
                parts.push(format!("{label} 今回は確認済み"));
            }
        }
        let when = soonest.map_or("接続確認の終了".to_string(), human_duration);
        Err(ApiError::Blocked(format!(
            "すべての接続先が利用できません（{}）。{when}後に再試行します",
            parts.join("、")
        )))
    }

    /// 查询 `store_number` 门店中 `parts` 各型号的可取货状态。
    ///
    /// 一次请求可以携带多个零件号，Apple 会在同一响应里返回全部结果，因此调用方
    /// 应当按门店聚合后再调用，而不是每个型号发一次请求。
    ///
    /// 被拦截（HTTP 541）后：不重试，登记冷却，之后对该地区的调用在冷却期内直接
    /// 返回 [`ApiError::Blocked`] 而不发请求；冷却结束后只放一次探测。
    pub async fn pickup_message(
        &self,
        region: &Region,
        store_number: &str,
        parts: &[String],
    ) -> Result<StoreAvailability, ApiError> {
        if store_number.is_empty() {
            return Err(ApiError::Transport("店舗番号が空です".into()));
        }
        let body = self
            .query(region, PickupScope::Store(store_number.to_string()), parts)
            .await?;
        parse_pickup_message(&body, store_number)
    }

    /// 一次查询 `location` 周边所有门店里 `parts` 各型号的可取货状态。
    ///
    /// `location` 的写法见 [`crate::model::Store::pickup_location`]。响应里有哪些
    /// 门店由 Apple 决定（按距离取若干家），调用方要自己核对想要的门店在不在，
    /// 不在的按门店单独补查。被拦与冷却的处理和 [`Self::pickup_message`] 相同。
    pub async fn pickup_message_nearby(
        &self,
        region: &Region,
        location: &str,
        parts: &[String],
    ) -> Result<Vec<StoreAvailability>, ApiError> {
        if location.trim().is_empty() {
            return Err(ApiError::Transport("確認する地域が空です".into()));
        }
        let body = self
            .query(region, PickupScope::Nearby(location.to_string()), parts)
            .await?;
        parse_pickup_stores(&body)
    }

    /// 两种范围共用的一次取货查询：冷却检查、暖场、发请求、被拦登记。
    async fn query(
        &self,
        region: &Region,
        scope: PickupScope,
        parts: &[String],
    ) -> Result<Vec<u8>, ApiError> {
        if parts.is_empty() {
            return Err(ApiError::Transport("製品番号の一覧が空です".into()));
        }

        let mut query: Vec<(String, String)> = vec![
            ("pl".into(), "true".into()),
            ("mts.0".into(), "regular".into()),
            scope.query_param(),
        ];
        for (i, part) in parts.iter().enumerate() {
            query.push((format!("parts.{i}"), part.clone()));
        }

        let referer = region.default_buy_page_url();
        let url = region.pickup_message_url();
        let outbound = Outbound {
            url: &url,
            query: &query,
            region,
            referer: &referer,
            scope: &scope,
            parts: parts.len(),
        };
        with_retry(self.config.max_retries, || self.query_routes(&outbound)).await
    }

    /// 每次重试重新选择线路并计数，达到 20 次后不继续使用原线路。
    async fn query_routes(&self, outbound: &Outbound<'_>) -> Result<Vec<u8>, ApiError> {
        let region = outbound.region;
        let attempts = self.routes.read().await.len().max(1);
        let multi = attempts > 1;
        let mut tried: Vec<String> = Vec::new();
        let mut last_err: Option<ApiError> = None;
        // 本轮每条线路各自的下场。全部失败时一起报出去：只报最后一条，用户会以为
        // 前面那几条根本没用上（#37 里就有人问「为什么一直看不到 proxy#1」）。
        let mut outcomes: Vec<String> = Vec::new();

        // 每条线路本次最多试一次：被拦或代理不通就换下一条，全试完才把最后的错误
        // 交出去。只有直连时和以前完全一样：一次请求，被拦就冷却。
        for _ in 0..attempts {
            let (route, probing) = match self.pick_route(region, &tried).await {
                Ok(picked) => picked,
                Err(err) => return Err(summarize_round(last_err.unwrap_or(err), &outcomes)),
            };
            tried.push(route.label.clone());

            // 先把 cookie 攒上再查，见 ensure_warm；暖不上也照常查。
            self.ensure_warm(&route, region).await;
            self.throttle().await;
            let result = self.get_once(&route, outbound).await;

            match result {
                Ok(body) => {
                    self.clear_block(&route, region, probing).await;
                    return Ok(body);
                }
                Err(ApiError::Blocked(detail)) => {
                    self.forget_warm(&route, region).await;
                    let cooldown = self.record_block(&route, region, probing).await;
                    let who = if multi {
                        format!("接続先 {} ", route.label)
                    } else {
                        String::new()
                    };
                    let text = format!(
                        "{detail}。{who}待機に入りました。{}後に1回再試行します",
                        human_duration(cooldown)
                    );
                    outcomes.push(text.clone());
                    last_err = Some(ApiError::Blocked(text));
                }
                // 直连失败也继续尝试其他线路，避免下一轮停在失败的直连上。
                Err(err @ ApiError::Transport(_)) if multi => {
                    if probing {
                        self.end_probe(&route, region).await;
                    }
                    let text = if route.is_proxy() {
                        describe_proxy_failure(&route, &err)
                    } else {
                        format!("直接接続：{err}")
                    };
                    outcomes.push(text.clone());
                    last_err = Some(ApiError::Transport(text));
                }
                Err(err) => {
                    if probing {
                        self.end_probe(&route, region).await;
                    }
                    return Err(err);
                }
            }
        }
        let last =
            last_err.unwrap_or_else(|| ApiError::Transport("利用できる接続先がありません".into()));
        Err(summarize_round(last, &outcomes))
    }

    /// 探测与 `region` 之间实际协商出来的 HTTP 版本。
    ///
    /// 这个方法存在的唯一理由是给契约测试当护栏，功能上没人需要它。
    ///
    /// `reqwest` 的 HTTP/2 支持挂在 `http2` feature 上，而这个 crate 用的是
    /// `default-features = false`。那个 feature 曾经漏了整整一个版本：客户端
    /// 静默退回 HTTP/1.1，所有查询照常成功、所有测试照常通过，**功能上完全
    /// 看不出来**。但对 Apple 的边缘节点来说，一个自称最新 Chrome 的客户端
    /// 用 HTTP/1.1 跟它说话，是一眼可辨的脚本特征。
    ///
    /// 这种「配置写漏了、功能却没坏」的缺陷，只能靠一条真的去连一次的测试兜住。
    pub async fn negotiated_http_version(&self, region: &Region) -> Result<String, ApiError> {
        let route = self.primary().await;
        self.throttle().await;
        let fetched = route
            .http
            .fetch(
                &region.bag_url(),
                &[],
                navigation_headers(&self.config.profile, region),
                MAX_WARM_BYTES,
            )
            .await?;
        Ok(fetched.version)
    }

    /// 每一次出站请求都要经过这里：任意两次之间至少间隔 `min_interval`（不分线路），
    async fn throttle(&self) {
        let slot = {
            let mut last = self.last_sent.lock().await;
            let now = Instant::now();
            // `last` 存的是上一次**预约**的发送时刻，可能仍在未来。必须在它之上
            // 累加间隔，而不是拿它和 now 求差 —— 那样几个并发调用会各自算出同一个
            // 「再等 min_interval」，然后在同一时刻一起冲出去。
            let slot = match *last {
                Some(prev) => (prev + self.config.min_interval).max(now),
                None => now,
            };
            *last = Some(slot);
            slot
        };
        tokio::time::sleep_until(slot).await;
    }

    /// 执行单次 HTTP 请求，把失败归类，并留下一条请求记录。
    async fn get_once(&self, route: &Route, req: &Outbound<'_>) -> Result<Vec<u8>, ApiError> {
        let Outbound {
            url,
            query,
            region,
            referer,
            scope,
            parts,
        } = *req;
        let started = Instant::now();
        let mut record = RequestRecord {
            at_ms: now_ms(),
            kind: "pickup",
            transport: route.http.label(),
            route: route.label.clone(),
            locale: region.locale.to_string(),
            store: scope.store(),
            location: scope.location(),
            parts,
            status: None,
            http_version: None,
            duration_ms: 0,
            outcome: String::new(),
            cookies_sent: route.http.cookie_names_for(url),
            cookies_after: Vec::new(),
            final_url: None,
        };

        let sent = route
            .http
            .fetch(
                url,
                query,
                api_headers(&self.config.profile, region, referer),
                MAX_RESPONSE_BYTES,
            )
            .await;
        let fetched = match sent {
            Ok(fetched) => fetched,
            Err(err) => {
                record.duration_ms = started.elapsed().as_millis() as u64;
                record.outcome = "transport".into();
                self.push_record(record).await;
                return Err(err);
            }
        };

        let status = fetched.status;
        record.status = Some(status);
        record.http_version = Some(fetched.version);
        record.final_url = Some(fetched.final_url);
        let content_type = fetched.content_type;
        let body = fetched.body;
        record.duration_ms = started.elapsed().as_millis() as u64;
        record.cookies_after = route.http.cookie_names_for(url);

        // 先看状态码：拦截页的响应体读到一半失败，也仍然是「被拦」，不能降级成网络错误。
        if let Some(err) = classify_status(status) {
            record.outcome = match &err {
                ApiError::Blocked(_) => "blocked".into(),
                ApiError::RateLimited(_) => "rate_limited".into(),
                _ => "http_error".into(),
            };
            self.push_record(record).await;
            return Err(err);
        }
        let body = match body {
            Ok(body) => body,
            Err(err) => {
                record.outcome = "transport".into();
                self.push_record(record).await;
                return Err(err);
            }
        };
        // 状态码 200 也未必是 JSON：被拦截时可能返回 HTML。
        if looks_like_json(&content_type, &body) {
            record.outcome = "ok".into();
            self.push_record(record).await;
            Ok(body)
        } else {
            record.outcome = "not_json".into();
            self.push_record(record).await;
            Err(ApiError::Blocked(
                "HTTP 200ですが、応答がJSONではありません".into(),
            ))
        }
    }
}

/// 页面导航（暖场、抓购买页）用的请求头：和地址栏直接打开一个页面时 Chrome 发的一致。
pub(crate) fn navigation_headers(profile: &RequestProfile, region: &Region) -> HeaderMap {
    // 插入顺序照抄 Chrome 发出的顺序：头的先后也是指纹的一部分。
    // accept-encoding 与 cookie 由传输层自己补在后面。
    let mut headers = HeaderMap::new();
    if let Some(hints) = &profile.client_hints {
        put(&mut headers, "sec-ch-ua", &hints.ua);
        put(&mut headers, "sec-ch-ua-mobile", &hints.mobile);
        put(&mut headers, "sec-ch-ua-platform", &hints.platform);
    }
    put(&mut headers, "upgrade-insecure-requests", "1");
    put(&mut headers, "user-agent", &profile.user_agent);
    put(
        &mut headers,
        "accept",
        "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8,application/signed-exchange;v=b3;q=0.7",
    );
    if profile.fetch_metadata {
        put(&mut headers, "sec-fetch-site", "none");
        put(&mut headers, "sec-fetch-mode", "navigate");
        put(&mut headers, "sec-fetch-user", "?1");
        put(&mut headers, "sec-fetch-dest", "document");
    }
    put(&mut headers, "accept-language", region.accept_language());
    headers
}

/// 取货接口请求头：和购买页里同源 `fetch()` 发出的一致。
pub(crate) fn api_headers(profile: &RequestProfile, region: &Region, referer: &str) -> HeaderMap {
    // 插入顺序照抄 Chrome 里 fetch() 发出的顺序：脚本自定义头夹在 accept 与
    // sec-fetch-* 之间，accept-encoding 与 cookie 由传输层补在末尾。
    let mut headers = HeaderMap::new();
    if let Some(hints) = &profile.client_hints {
        put(&mut headers, "sec-ch-ua", &hints.ua);
        put(&mut headers, "sec-ch-ua-mobile", &hints.mobile);
    }
    put(&mut headers, "user-agent", &profile.user_agent);
    if let Some(hints) = &profile.client_hints {
        put(&mut headers, "sec-ch-ua-platform", &hints.platform);
    }
    put(&mut headers, "accept", &profile.api_accept);
    if profile.x_requested_with {
        put(&mut headers, "x-requested-with", "XMLHttpRequest");
    }
    if profile.apple_extras {
        put(&mut headers, "x-skip-redirect", "true");
        put(&mut headers, "x-aos-ui-fetch-call-1", &fetch_call_token());
    }
    if profile.fetch_metadata {
        put(&mut headers, "sec-fetch-site", "same-origin");
        put(&mut headers, "sec-fetch-mode", "cors");
        put(&mut headers, "sec-fetch-dest", "empty");
    }
    put(&mut headers, "referer", referer);
    put(&mut headers, "accept-language", region.accept_language());
    headers
}

/// 一次出站请求拿回来的东西，传输层无关。
struct Fetched {
    status: u16,
    /// 实际协商出的 HTTP 版本，如 `HTTP/2.0`。
    version: String,
    /// 跟随重定向后的最终地址。
    final_url: String,
    content_type: String,
    /// 响应体，读到上限即止；读到一半失败时保留状态码，由调用方决定怎么归类。
    body: Result<Vec<u8>, ApiError>,
}

/// 传输层实现与它自己的 cookie 罐。
#[derive(Clone)]
enum Http {
    Rustls {
        client: reqwest::Client,
        jar: Arc<reqwest::cookie::Jar>,
    },
    #[cfg(feature = "chrome-tls")]
    Chrome {
        client: wreq::Client,
        jar: Arc<wreq::cookie::Jar>,
    },
}

impl std::fmt::Debug for Http {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

impl Http {
    fn build(config: &ClientConfig, proxy: Option<&str>) -> Result<Self, ApiError> {
        match config.transport {
            Transport::Rustls => {
                let jar = Arc::new(reqwest::cookie::Jar::default());
                let mut builder = reqwest::Client::builder()
                    .timeout(config.timeout)
                    .connect_timeout(Duration::from_secs(5))
                    .pool_idle_timeout(Duration::from_secs(90))
                    .pool_max_idle_per_host(8)
                    .cookie_provider(jar.clone());
                if let Some(proxy) = proxy {
                    let proxy = reqwest::Proxy::all(proxy).map_err(|e| {
                        ApiError::Transport(format!(
                            "プロキシURL {} が無効です：{e}",
                            redact_proxy(proxy)
                        ))
                    })?;
                    builder = builder.proxy(proxy);
                }
                let client = builder.build().map_err(|e| {
                    ApiError::Transport(format!("HTTPクライアントを作成できませんでした：{e}"))
                })?;
                Ok(Self::Rustls { client, jar })
            }
            Transport::ChromeTls => {
                #[cfg(feature = "chrome-tls")]
                {
                    // 只取仿真档案里的 TLS / HTTP/1 / HTTP/2 参数，请求头仍由
                    // RequestProfile 控制：握手是它的，说话是我们的，两边版本一致。
                    let emulation =
                        wreq::IntoEmulation::into_emulation(wreq_util::Emulation::Chrome149);
                    let jar = Arc::new(wreq::cookie::Jar::default());
                    let mut builder = wreq::Client::builder()
                        .timeout(config.timeout)
                        .connect_timeout(Duration::from_secs(5))
                        .pool_idle_timeout(Duration::from_secs(90))
                        .pool_max_idle_per_host(8)
                        .cookie_provider(jar.clone())
                        .tls_options(emulation.tls_options)
                        .http1_options(emulation.http1_options)
                        .http2_options(emulation.http2_options);
                    if let Some(proxy) = proxy {
                        let proxy = wreq::Proxy::all(proxy).map_err(|e| {
                            ApiError::Transport(format!(
                                "プロキシURL {} が無効です：{e}",
                                redact_proxy(proxy)
                            ))
                        })?;
                        builder = builder.proxy(proxy);
                    }
                    let client = builder.build().map_err(|e| {
                        ApiError::Transport(format!("HTTPクライアントを作成できませんでした：{e}"))
                    })?;
                    Ok(Self::Chrome { client, jar })
                }
                #[cfg(not(feature = "chrome-tls"))]
                {
                    Err(ApiError::Transport(
                        "このビルドはchrome-tlsを含みません。Transport::Rustlsを使用してください"
                            .into(),
                    ))
                }
            }
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Rustls { .. } => Transport::Rustls.label(),
            #[cfg(feature = "chrome-tls")]
            Self::Chrome { .. } => Transport::ChromeTls.label(),
        }
    }

    /// 发一次 GET，读到上限为止。只有连接层面的失败才返回 `Err`；拿到响应就一定
    /// 返回 `Ok`，状态码由调用方归类。
    async fn fetch(
        &self,
        url: &str,
        query: &[(String, String)],
        headers: HeaderMap,
        max_body: usize,
    ) -> Result<Fetched, ApiError> {
        match self {
            Self::Rustls { client, .. } => {
                let resp = client
                    .get(url)
                    .query(query)
                    .headers(headers)
                    .send()
                    .await
                    .map_err(|e| ApiError::Transport(e.to_string()))?;
                let status = resp.status().as_u16();
                let version = format!("{:?}", resp.version());
                let final_url = resp.url().to_string();
                let content_type = content_type_of(resp.headers());
                let body = read_body_capped(resp, max_body).await;
                Ok(Fetched {
                    status,
                    version,
                    final_url,
                    content_type,
                    body,
                })
            }
            #[cfg(feature = "chrome-tls")]
            Self::Chrome { client, .. } => {
                let resp = client
                    .get(url)
                    .query(query)
                    .headers(headers)
                    .send()
                    .await
                    .map_err(|e| ApiError::Transport(e.to_string()))?;
                let status = resp.status().as_u16();
                let version = format!("{:?}", resp.version());
                let final_url = resp.uri().to_string();
                let content_type = content_type_of(resp.headers());
                let body = read_wreq_body_capped(resp, max_body).await;
                Ok(Fetched {
                    status,
                    version,
                    final_url,
                    content_type,
                    body,
                })
            }
        }
    }

    /// 罐里对某个地址生效的 cookie 名。
    fn cookie_names_for(&self, url: &str) -> Vec<String> {
        match self {
            Self::Rustls { jar, .. } => {
                use reqwest::cookie::CookieStore;
                let Ok(url) = url.parse() else {
                    return Vec::new();
                };
                let Some(value) = jar.cookies(&url) else {
                    return Vec::new();
                };
                let Ok(text) = value.to_str() else {
                    return Vec::new();
                };
                text.split(';')
                    .filter_map(|pair| pair.trim().split('=').next())
                    .filter(|name| !name.is_empty())
                    .map(str::to_string)
                    .collect()
            }
            #[cfg(feature = "chrome-tls")]
            Self::Chrome { jar, .. } => {
                let Ok(uri) = url.parse::<wreq::Uri>() else {
                    return Vec::new();
                };
                jar.matches(uri).map(|c| c.name().to_string()).collect()
            }
        }
    }

    /// 罐里对某个地址生效的整条 `Cookie` 头，没有则 `None`。**不要打印它**。
    fn cookie_header_for(&self, url: &str) -> Option<String> {
        match self {
            Self::Rustls { jar, .. } => {
                use reqwest::cookie::CookieStore;
                let url = url.parse().ok()?;
                jar.cookies(&url)
                    .and_then(|v| v.to_str().ok().map(str::to_owned))
            }
            #[cfg(feature = "chrome-tls")]
            Self::Chrome { jar, .. } => {
                let uri = url.parse::<wreq::Uri>().ok()?;
                let pairs: Vec<String> = jar
                    .matches(uri)
                    .map(|c| format!("{}={}", c.name(), c.value()))
                    .collect();
                (!pairs.is_empty()).then(|| pairs.join("; "))
            }
        }
    }
}

fn content_type_of(headers: &HeaderMap) -> String {
    headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

/// 与 [`read_body_capped`] 相同的分块限量读取，wreq 版。
#[cfg(feature = "chrome-tls")]
async fn read_wreq_body_capped(resp: wreq::Response, max: usize) -> Result<Vec<u8>, ApiError> {
    use futures_util::StreamExt;
    let mut stream = resp.bytes_stream();
    let mut body = Vec::new();
    while body.len() < max {
        let Some(chunk) = stream.next().await else {
            break;
        };
        let chunk =
            chunk.map_err(|e| ApiError::Transport(format!("応答を読み込めませんでした：{e}")))?;
        let n = chunk.len().min(max - body.len());
        body.extend_from_slice(&chunk[..n]);
    }
    Ok(body)
}

/// 往头表里放一项。名字是我们自己写的常量，值是我们自己拼的 ASCII；万一不合法，
/// 宁可少发这一项也不能让库代码 panic。
fn put(headers: &mut HeaderMap, name: &'static str, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(HeaderName::from_static(name), value);
    }
}

/// 仿照 Apple 商店前端 `x-aos-ui-fetch-call-1` 样例（`y9kyn7tf7c-mtz5ds9h`）拼一个请求标识：
/// 10 位 `[a-z0-9]` 随机串加短横线加毫秒时间戳的 36 进制。
///
/// **这只是仿样例格式，不是已经确认的 Apple 算法**，所以默认档案不发它，只供诊断对照。
fn fetch_call_token() -> String {
    use rand::Rng;
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::rng();
    let head: String = (0..10)
        .map(|_| ALPHABET[rng.random_range(0..ALPHABET.len())] as char)
        .collect();
    format!("{head}-{}", base36(now_ms()))
}

fn base36(mut n: u64) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".into();
    }
    let mut out = Vec::new();
    while n > 0 {
        out.push(DIGITS[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// 把时长写成人看的「5 分钟」「90 秒」。
fn human_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 60 && secs.is_multiple_of(60) {
        format!("{}分", secs / 60)
    } else if secs >= 120 {
        format!("{}分", secs.div_ceil(60))
    } else {
        format!("{secs}秒")
    }
}

/// 把非 200 的状态码归类成本模块定义的错误；200 返回 `None`，交给调用方按各自的
/// 内容规则判断（库存接口要求是 JSON，购买页只要求非空）。
///
/// 抽出来是因为库存查询与购买页抓取原本各写了一份，五个分支逐字相同。同一件事有
/// 两处定义，迟早会只改其中一处 —— 比如哪天 Apple 换个新的拦截状态码。
pub(crate) fn classify_status(code: u16) -> Option<ApiError> {
    match code {
        200 => None,
        // 541 是 Apple 自定义的拦截状态码，不是标准 HTTP 状态码。
        541 => Some(ApiError::Blocked("HTTP 541".into())),
        403 => Some(ApiError::Blocked("HTTP 403".into())),
        429 => Some(ApiError::RateLimited("HTTP 429".into())),
        c if c >= 500 => Some(ApiError::RateLimited(format!("HTTP {c}"))),
        c => Some(ApiError::Transport(format!("HTTP {c}"))),
    }
}

/// 带指数退避的重试。只有可自愈的错误才重试，结构不符与业务错误重试多少次都一样。
///
/// 同样是原本两处各写一份：库存查询与购买页抓取的退避逻辑此前逐字相同。
pub(crate) async fn with_retry<T, F, Fut>(max_retries: u32, mut once: F) -> Result<T, ApiError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, ApiError>>,
{
    let mut last_err = None;

    for attempt in 0..=max_retries {
        if attempt > 0 {
            // 被拦截或限流时继续以原频率猛冲只会让情况更糟。
            tokio::time::sleep(Duration::from_secs(1u64 << (attempt - 1))).await;
        }
        match once().await {
            Ok(v) => return Ok(v),
            Err(err) if err.is_retryable() => last_err = Some(err),
            Err(err) => return Err(err),
        }
    }

    Err(last_err.unwrap_or_else(|| ApiError::Transport("再試行回数の上限に達しました".into())))
}

/// 分块读取响应体，累计到 `max` 立刻停下。
///
/// 不能用 `resp.bytes()` 再 `.take(max)`：那个方法会先把整份响应缓冲进内存，
/// 上限是在「已经吃完」之后才生效的，对超大响应或 gzip 解压炸弹起不到任何保护。
/// 边读边截才是真的有上限。
pub(crate) async fn read_body_capped(
    mut resp: reqwest::Response,
    max: usize,
) -> Result<Vec<u8>, ApiError> {
    let mut body = Vec::new();
    while body.len() < max {
        let chunk = resp
            .chunk()
            .await
            .map_err(|e| ApiError::Transport(format!("応答を読み込めませんでした：{e}")))?;
        let Some(chunk) = chunk else { break };
        let n = chunk.len().min(max - body.len());
        body.extend_from_slice(&chunk[..n]);
    }
    Ok(body)
}

fn looks_like_json(content_type: &str, body: &[u8]) -> bool {
    if content_type.contains("json") {
        return true;
    }
    body.iter()
        .find(|b| !b.is_ascii_whitespace())
        .is_some_and(|b| *b == b'{' || *b == b'[')
}

/// 抽象出调度引擎依赖的查询能力，便于在测试里替换掉真实网络请求。
///
/// 用泛型约束而不是 trait object：async fn in trait 在泛型位置可以直接写，
/// 做成 `dyn` 还得引第三方宏来装箱 future，而引擎只需要一个具体实现，不值得。
pub trait Fetcher: Clone + Send + Sync + 'static {
    fn pickup_message(
        &self,
        region: &'static Region,
        store_number: &str,
        parts: &[String],
    ) -> impl std::future::Future<Output = Result<StoreAvailability, ApiError>> + Send;

    /// 一次拿到 `location` 周边所有门店里 `parts` 的状态，见
    /// [`AppleClient::pickup_message_nearby`]。
    fn pickup_message_nearby(
        &self,
        region: &'static Region,
        location: &str,
        parts: &[String],
    ) -> impl std::future::Future<Output = Result<Vec<StoreAvailability>, ApiError>> + Send;

    /// 所有线路是否都因该地区的拦截而处于冷却期。
    fn all_routes_cooling(&self, locale: &str) -> impl std::future::Future<Output = bool> + Send {
        let _ = locale;
        async { false }
    }
}

impl Fetcher for AppleClient {
    async fn pickup_message(
        &self,
        region: &'static Region,
        store_number: &str,
        parts: &[String],
    ) -> Result<StoreAvailability, ApiError> {
        AppleClient::pickup_message(self, region, store_number, parts).await
    }

    async fn pickup_message_nearby(
        &self,
        region: &'static Region,
        location: &str,
        parts: &[String],
    ) -> Result<Vec<StoreAvailability>, ApiError> {
        AppleClient::pickup_message_nearby(self, region, location, parts).await
    }

    async fn all_routes_cooling(&self, locale: &str) -> bool {
        AppleClient::all_routes_cooling(self, locale).await
    }
}

/// 单个零件号在单个门店的查询结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartStatus {
    pub part_number: String,
    pub availability: Availability,
    /// Apple 返回的商品名，可用于校验本地目录是否过期。
    pub product_title: Option<String>,
    /// 原始字段值，保留下来便于排查问题和适配未来新增的取值。
    pub pickup_display: String,
}

/// 单个门店的查询结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreAvailability {
    pub store_number: String,
    pub store_name: String,
    pub parts: std::collections::BTreeMap<String, PartStatus>,
}

// ---- 响应结构。只声明用得到的字段：Apple 的响应有几十个字段，全部映射既无必要，
// ---- 也更容易随接口调整而整体失配。

#[derive(Debug, Deserialize)]
struct PickupResponse {
    #[serde(default)]
    head: PickupHead,
    #[serde(default)]
    body: PickupBody,
}

#[derive(Debug, Default, Deserialize)]
struct PickupHead {
    /// 用 `serde_json::Value` 承接而不是 `String`：字段缺失与「给了但为空串」
    /// 在 `String` 下都是空，而这两者的处置恰好相反。也顺带容下 `"200"` 改成
    /// 数字 `200` 这类形态变化。
    #[serde(default)]
    status: Option<serde_json::Value>,
}

#[derive(Debug, Default, Deserialize)]
struct PickupBody {
    #[serde(default)]
    stores: Vec<PickupStore>,
    #[serde(rename = "errorMessage", default)]
    error_message: Option<String>,
    /// 旧版 fulfillment-messages 的嵌套结构，留作兜底，以防 Apple 把数据挪回去。
    #[serde(default)]
    content: PickupContent,
}

#[derive(Debug, Default, Deserialize)]
struct PickupContent {
    #[serde(rename = "pickupMessage", default)]
    pickup_message: PickupMessageNode,
}

#[derive(Debug, Default, Deserialize)]
struct PickupMessageNode {
    #[serde(default)]
    stores: Vec<PickupStore>,
}

#[derive(Debug, Deserialize)]
struct PickupStore {
    #[serde(rename = "storeNumber", default)]
    store_number: String,
    #[serde(rename = "storeName", default)]
    store_name: String,
    #[serde(rename = "partsAvailability", default)]
    parts_availability: std::collections::BTreeMap<String, PickupPart>,
}

#[derive(Debug, Deserialize)]
struct PickupPart {
    #[serde(rename = "partNumber", default)]
    part_number: Option<String>,
    #[serde(rename = "pickupDisplay", default)]
    pickup_display: Option<String>,
    #[serde(rename = "messageTypes", default)]
    message_types: MessageTypes,
}

#[derive(Debug, Default, Deserialize)]
struct MessageTypes {
    #[serde(default)]
    regular: RegularMessage,
}

#[derive(Debug, Default, Deserialize)]
struct RegularMessage {
    #[serde(rename = "storePickupProductTitle", default)]
    store_pickup_product_title: Option<String>,
}

/// 解析取货状态响应。
pub fn parse_pickup_message(raw: &[u8], want_store: &str) -> Result<StoreAvailability, ApiError> {
    let resp = parse_response(raw)?;
    let stores = store_list(&resp)?;

    // 指定了 store 参数时 Apple 只返回该门店，但仍按编号核对，
    // 避免把别的门店的库存错认成目标门店的。
    let matched = stores
        .iter()
        .find(|s| s.store_number == want_store)
        .ok_or_else(|| ApiError::SchemaDrift {
            field: "body.stores[].storeNumber".into(),
            raw: format!("応答に店舗 {want_store} がありません"),
        })?;

    if matched.parts_availability.is_empty() {
        return Err(ApiError::SchemaDrift {
            field: "body.stores[].partsAvailability".into(),
            raw: format!("店舗 {want_store} の構成別在庫情報がありません"),
        });
    }

    store_availability(matched)
}

/// 解析按地点查询的响应，返回其中每一家门店的状态，顺序照响应。
///
/// 某家门店的 `partsAvailability` 为空时仍保留这家店（parts 为空），调用方按
/// 型号对账时自然会把它的目标标成未知，而不是在这里把整份响应判废。
pub fn parse_pickup_stores(raw: &[u8]) -> Result<Vec<StoreAvailability>, ApiError> {
    let resp = parse_response(raw)?;
    store_list(&resp)?.iter().map(store_availability).collect()
}

fn parse_response(raw: &[u8]) -> Result<PickupResponse, ApiError> {
    let resp: PickupResponse = serde_json::from_slice(raw).map_err(|e| ApiError::SchemaDrift {
        field: "（応答全体）".into(),
        raw: format!("JSONとして解析できません：{e}"),
    })?;
    check_envelope(&resp)?;
    Ok(resp)
}

fn store_list(resp: &PickupResponse) -> Result<&[PickupStore], ApiError> {
    let stores = if resp.body.stores.is_empty() {
        &resp.body.content.pickup_message.stores
    } else {
        &resp.body.stores
    };

    // Apple 对已经停售、尚未开售或当前不可购买的零件号，可能返回一个成功信封
    // （head.status=200）和空门店列表。它没有说目标门店不存在，更不代表门店无货。
    // 把这种结果报成结构漂移会误导用户等待程序更新；明确指向商品状态，用户才知道
    // 应先核对官网和型号目录。
    if stores.is_empty() {
        return Err(ApiError::Apple(
            "Appleから店舗情報が返されませんでした。選択した構成は販売終了、販売開始前、または現在購入できない可能性があります".into(),
        ));
    }
    Ok(stores)
}

fn store_availability(matched: &PickupStore) -> Result<StoreAvailability, ApiError> {
    let mut parts = std::collections::BTreeMap::new();
    for (key, info) in &matched.parts_availability {
        // 条目里的 partNumber 与 map 键不一致时，无法判断哪个可信。随便选一个
        // 继续解析，可能把另一个型号的库存记到目标型号名下 —— 那比报错更糟。
        if let Some(inner) = info.part_number.as_deref().map(str::trim)
            && !inner.is_empty()
            && inner != key
        {
            return Err(ApiError::SchemaDrift {
                field: format!("body.stores[].partsAvailability.{key}.partNumber"),
                raw: format!("項目の製品番号 {inner} とキー {key} が一致しません"),
            });
        }

        let raw_display = info.pickup_display.clone().unwrap_or_default();
        parts.insert(
            key.clone(),
            PartStatus {
                part_number: key.clone(),
                availability: availability_from(&raw_display),
                product_title: info
                    .message_types
                    .regular
                    .store_pickup_product_title
                    .clone(),
                pickup_display: raw_display,
            },
        );
    }

    Ok(StoreAvailability {
        store_number: matched.store_number.clone(),
        store_name: matched.store_name.clone(),
        parts,
    })
}

/// 在读取门店数据之前先校验响应信封。
///
/// 必须先做这一步。Go 版只在 `stores` 为空时才看 `errorMessage`，也从不检查
/// `head.status`，于是「head.status=500 + errorMessage 非空 + stores 非空」
/// 这样一个明确的失败响应会被当成正常数据解析，最终得出「无货」。
fn check_envelope(resp: &PickupResponse) -> Result<(), ApiError> {
    if let Some(msg) = resp.body.error_message.as_deref()
        && !msg.trim().is_empty()
    {
        return Err(ApiError::Apple(msg.to_string()));
    }

    // status 缺失（含 JSON null）不判失败：保留的旧接口兜底路径本就没有这一层，
    // 把「没给」当失败会让那条路径直接报废。给了就必须是成功值。
    match &resp.head.status {
        None | Some(serde_json::Value::Null) => Ok(()),
        Some(serde_json::Value::String(s)) if s == "200" => Ok(()),
        Some(serde_json::Value::Number(n)) if n.as_u64() == Some(200) => Ok(()),
        Some(other) => Err(ApiError::SchemaDrift {
            field: "head.status".into(),
            raw: other.to_string(),
        }),
    }
}

/// 把 Apple 的 `pickupDisplay` 字段翻译成三态。
///
/// 已实际观测到的取值：`available`（可取货）、`unavailable`（不可取货）、
/// `ineligible`（该型号在此门店不支持到店取货）。
///
/// 未知取值一律归为 `Unknown` 并带上原始值，而不是 `OutOfStock` —— 猜错成
/// 「无货」会让用户错过机会，猜错成「未知」只是让用户多看一眼。同样重要的是，
/// 「不认识」和「Apple 说不知道」在类型上是可区分的：`SchemaDrift` 会一路传到
/// 界面上，提示用户接口可能已经变了，而不是安静地显示成一个普通的未知。
pub fn availability_from(pickup_display: &str) -> Availability {
    match pickup_display.trim().to_ascii_lowercase().as_str() {
        "available" => Availability::InStock,
        "unavailable" | "ineligible" => Availability::OutOfStock,
        "default" => Availability::Unknown(UnknownReason::PickupNotOpen {
            raw: pickup_display.trim().to_string(),
        }),
        other => Availability::Unknown(UnknownReason::SchemaDrift {
            field: "pickupDisplay".into(),
            raw: other.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCALE: &str = "zh_HK";

    #[test]
    fn 同一轮并发被拦只算一次不升档() {
        let mut tracker = BlockTracker::default();
        let now = Instant::now();
        // 六家门店同时在飞，第一家被拦后其余五家的 541 陆续到达。
        let first = tracker.record(LOCALE, now, false);
        assert_eq!(first, BLOCK_COOLDOWNS[0]);
        for i in 1..6 {
            let again = tracker.record(LOCALE, now + Duration::from_millis(i * 100), false);
            assert_eq!(again, BLOCK_COOLDOWNS[0], "第 {i} 个并发失败不该把冷却抬高");
        }
        // 冷却期内一律拒绝。
        assert!(
            tracker
                .admit(LOCALE, now + Duration::from_secs(60))
                .is_err()
        );
    }

    #[test]
    fn 只有探测再被拦才升档且探测成功才解封() {
        let mut tracker = BlockTracker::default();
        let t0 = Instant::now();
        tracker.record(LOCALE, t0, false);
        let after = t0 + BLOCK_COOLDOWNS[0] + Duration::from_secs(1);

        // 冷却结束：放一次探测，其余请求等它的结果。
        assert!(matches!(tracker.admit(LOCALE, after), Ok(true)));
        assert!(tracker.admit(LOCALE, after).is_err());

        // 探测再被拦：升到下一档。
        assert_eq!(tracker.record(LOCALE, after, true), BLOCK_COOLDOWNS[1]);
        let after2 = after + BLOCK_COOLDOWNS[1] + Duration::from_secs(1);
        assert!(matches!(tracker.admit(LOCALE, after2), Ok(true)));

        // 冷却期内并发漏过去的成功不算解封；探测成功才算。
        tracker.clear(LOCALE, false);
        assert!(
            tracker.admit(LOCALE, after2).is_err(),
            "非探测的成功不该清掉冷却"
        );
        tracker.clear(LOCALE, true);
        assert!(
            matches!(tracker.admit(LOCALE, after2), Ok(false)),
            "解封后照常放行"
        );
    }

    #[test]
    fn 探测因网络失败结束后下一次仍是探测() {
        let mut tracker = BlockTracker::default();
        let t0 = Instant::now();
        tracker.record(LOCALE, t0, false);
        let after = t0 + BLOCK_COOLDOWNS[0] + Duration::from_secs(1);
        assert!(matches!(tracker.admit(LOCALE, after), Ok(true)));
        tracker.end_probe(LOCALE);
        assert!(matches!(tracker.admit(LOCALE, after), Ok(true)));
    }
}
