use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use qqbot_api::payload::{Hello, Payload, RawPayload};
use qqbot_api::{ApiClient, Event, Intents, OpCode};

use crate::backoff::Backoff;
use crate::GatewayError;

/// 发往某个分片 actor 的控制命令。
#[derive(Debug)]
pub enum GatewayCmd {
    /// 主动发送一条原始 JSON（一般用不到，保留扩展位）。
    Send(Box<serde_json::value::RawValue>),
    /// 立即重连。
    Reconnect,
    /// 关闭该分片。
    Shutdown,
}

/// 网关配置。
#[derive(Debug, Clone)]
pub struct GatewayConfig {
    pub intents: Intents,
    /// 分片总数。`None` 表示采用 `/gateway/bot` 返回的建议值。
    pub shards: Option<u32>,
    /// 事件通道容量。满时丢弃事件并计数（背压策略：宁可丢事件，不可断心跳）。
    pub event_buffer: usize,
    /// 直接指定网关地址，跳过 `/gateway/bot`。
    ///
    /// 用于协议测试（指向本地 mock 服务）与私有化部署。
    pub url_override: Option<String>,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            intents: qqbot_api::DEFAULT_INTENTS,
            shards: None,
            event_buffer: 1024,
            url_override: None,
        }
    }
}

impl GatewayConfig {
    pub fn with_intents(mut self, intents: Intents) -> Self {
        self.intents = intents;
        self
    }

    pub fn with_shards(mut self, shards: u32) -> Self {
        self.shards = Some(shards);
        self
    }

    /// 覆盖网关地址（跳过 `/gateway/bot` 探测）。
    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.url_override = Some(url.into());
        self
    }
}

/// 单次连接的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    /// 连接断开，按退避重连（可 Resume）。
    Reconnect,
    /// 服务端判定 session 无效，清空状态后重新 Identify。
    InvalidSession,
    /// 收到 Shutdown。
    Shutdown,
}

/// 网关句柄：持有事件接收端与各分片的控制端。
pub struct GatewayHandle {
    events: mpsc::Receiver<Arc<Event>>,
    cmds: Vec<mpsc::Sender<GatewayCmd>>,
    tasks: JoinSet<()>,
    shards: u32,
    gateway_url: String,
}

impl GatewayHandle {
    /// 分片总数。
    pub fn shard_count(&self) -> u32 {
        self.shards
    }

    /// 网关地址。
    pub fn gateway_url(&self) -> &str {
        &self.gateway_url
    }

    /// 接收下一个事件。
    pub async fn recv(&mut self) -> Option<Arc<Event>> {
        self.events.recv().await
    }

    /// 向第 0 个分片下发命令。
    pub async fn send_cmd(&self, cmd: GatewayCmd) -> Result<(), GatewayError> {
        match self.cmds.first() {
            Some(tx) => tx.send(cmd).await.map_err(|_| GatewayError::EventChannelClosed),
            None => Err(GatewayError::EventChannelClosed),
        }
    }

    /// 关闭所有分片并等待退出。
    pub async fn shutdown(mut self) {
        for tx in &self.cmds {
            let _ = tx.send(GatewayCmd::Shutdown).await;
        }
        while self.tasks.join_next().await.is_some() {}
    }
}

/// 建立网关连接。
///
/// 默认先请求 `/gateway/bot` 获取地址与建议分片数；若 [`GatewayConfig::url_override`]
/// 已指定地址则跳过探测。随后为每个分片启动一个 [`GatewayActor`]。
pub async fn spawn_gateway(
    api: ApiClient,
    cfg: GatewayConfig,
) -> Result<GatewayHandle, GatewayError> {
    let (url, suggested_shards, max_concurrency) = match cfg.url_override.as_deref() {
        Some(url) => {
            tracing::info!(url, "使用显式指定的网关地址（跳过 /gateway/bot）");
            (url.to_string(), 1u32, 0u32)
        }
        None => {
            let info = api.gateway().await?;
            (
                info.url,
                info.shards,
                info.session_start_limit.as_ref().map(|s| s.max_concurrency).unwrap_or(0),
            )
        }
    };

    let shards = cfg.shards.unwrap_or(suggested_shards).max(1);

    tracing::info!(url = %url, shards, max_concurrency, "网关信息已确定");

    let (events_tx, events_rx) = mpsc::channel::<Arc<Event>>(cfg.event_buffer);
    let mut tasks = JoinSet::new();
    let mut cmds = Vec::with_capacity(shards as usize);

    for index in 0..shards {
        let (cmd_tx, cmd_rx) = mpsc::channel::<GatewayCmd>(16);
        cmds.push(cmd_tx);

        let actor = GatewayActor {
            api: api.clone(),
            gateway_url: url.clone(),
            shard: [index, shards],
            intents: cfg.intents,
            events: events_tx.clone(),
            inbox: cmd_rx,
            session_id: None,
            last_seq: 0,
            backoff: Backoff::default(),
        };

        tasks.spawn(actor.run());
    }

    // 上层只持有 events_rx；这里丢掉多余 sender 以免通道永不关闭。
    drop(events_tx);

    Ok(GatewayHandle {
        events: events_rx,
        cmds,
        tasks,
        shards,
        gateway_url: url,
    })
}

/// 每个分片一个 actor。
struct GatewayActor {
    api: ApiClient,
    gateway_url: String,
    shard: [u32; 2],
    intents: Intents,
    events: mpsc::Sender<Arc<Event>>,
    inbox: mpsc::Receiver<GatewayCmd>,

    // ---- 独占状态：只有本 actor 触碰 ----
    session_id: Option<String>,
    last_seq: i64,
    backoff: Backoff,
}

impl GatewayActor {
    async fn run(mut self) {
        loop {
            let flow = match self.connect_and_pump().await {
                Ok(flow) => flow,
                Err(err) => {
                    tracing::warn!(
                        shard = ?self.shard,
                        error = %err,
                        retryable = self.is_retryable(&err),
                        "网关连接中断"
                    );
                    Flow::Reconnect
                }
            };

            match flow {
                Flow::Shutdown => {
                    tracing::info!(shard = ?self.shard, "分片已关闭");
                    return;
                }
                Flow::InvalidSession => {
                    tracing::warn!(shard = ?self.shard, "session 无效，清空后重新 Identify");
                    self.session_id = None;
                    self.last_seq = 0;
                }
                Flow::Reconnect => {}
            }

            let delay = self.backoff.next_delay();
            tracing::info!(shard = ?self.shard, ?delay, attempt = self.backoff.attempt(), "退避后重连");

            // ⚠️ 必须用**绝对 deadline** 循环等待。若直接 select! 一个 sleep，
            // 任何一条 Send 命令都会让 select 提前结束、把剩余的退避时间取消掉，
            // 指数退避就被打穿成热重连（而且命令还被静默丢弃）。
            // 这里只让 Shutdown / Reconnect 打破退避。
            let deadline = tokio::time::Instant::now() + delay;
            loop {
                tokio::select! {
                    _ = tokio::time::sleep_until(deadline) => break,
                    cmd = self.inbox.recv() => match cmd {
                        None | Some(GatewayCmd::Shutdown) => return,
                        Some(GatewayCmd::Reconnect) => break,
                        Some(GatewayCmd::Send(_)) => {
                            // 连接尚未建立，这条下发注定失败：显式记录而非静默吞掉。
                            tracing::debug!(shard = ?self.shard, "退避期间收到 Send，连接未就绪，已丢弃");
                        }
                    },
                }
            }
        }
    }

    fn is_retryable(&self, err: &GatewayError) -> bool {
        match err {
            GatewayError::Api(e) => e.is_retryable(),
            _ => true,
        }
    }

    async fn connect_and_pump(&mut self) -> Result<Flow, GatewayError> {
        let (mut ws, _resp) = tokio_tungstenite::connect_async(self.gateway_url.as_str()).await?;
        tracing::debug!(shard = ?self.shard, "WebSocket 已连接，等待 Hello");

        // ---- op 10 Hello ----
        let hello_text = next_text(&mut ws).await?.ok_or(GatewayError::ClosedBeforeHello)?;
        let hello: Payload<Hello> = serde_json::from_str(&hello_text)?;
        let interval_ms = hello
            .d
            .map(|d| d.heartbeat_interval)
            .unwrap_or(30_000)
            .max(1_000);
        tracing::debug!(shard = ?self.shard, interval_ms, "收到 Hello");

        // ---- op 2 Identify 或 op 6 Resume ----
        // 载荷由 ApiClient 构造，确保 token 带上 "QQBot " 前缀。
        let resuming = self.session_id.clone().filter(|_| self.last_seq > 0);
        match resuming {
            Some(session_id) => {
                tracing::info!(shard = ?self.shard, seq = self.last_seq, "发送 Resume");
                let d = self.api.resume(session_id, self.last_seq).await?;
                send_op(&mut ws, OpCode::Resume, d).await?;
            }
            None => {
                tracing::info!(shard = ?self.shard, intents = self.intents.bits(), "发送 Identify");
                let d = self.api.identify(self.intents, self.shard).await?;
                send_op(&mut ws, OpCode::Identify, d).await?;
            }
        }

        // ---- 主循环 ----
        let mut heartbeat = tokio::time::interval(Duration::from_millis(interval_ms));
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // 首次 tick 立即触发，跳过它（Hello 之后不必马上发心跳）。
        heartbeat.tick().await;

        let mut awaiting_ack = false;

        loop {
            tokio::select! {
                biased;

                cmd = self.inbox.recv() => {
                    match cmd {
                        None | Some(GatewayCmd::Shutdown) => return Ok(Flow::Shutdown),
                        Some(GatewayCmd::Reconnect) => return Ok(Flow::Reconnect),
                        Some(GatewayCmd::Send(raw)) => {
                            ws.send(Message::text(raw.get().to_string())).await?;
                        }
                    }
                }

                _ = heartbeat.tick() => {
                    if awaiting_ack {
                        // 上一个心跳没有收到 ACK，判定链路已死。
                        tracing::warn!(shard = ?self.shard, "心跳未收到 ACK，判定连接失效");
                        metrics::counter!("qqbot_gateway_heartbeat_timeout_total").increment(1);
                        return Ok(Flow::Reconnect);
                    }
                    let d = if self.last_seq > 0 { Some(self.last_seq) } else { None };
                    send_op(&mut ws, OpCode::Heartbeat, d).await?;
                    awaiting_ack = true;
                    tracing::debug!(shard = ?self.shard, seq = ?d, "已发送心跳 (op 1)");
                    metrics::counter!("qqbot_gateway_heartbeat_total").increment(1);
                }

                frame = ws.next() => {
                    let Some(frame) = frame else {
                        tracing::info!(shard = ?self.shard, "WebSocket 流结束");
                        return Ok(Flow::Reconnect);
                    };

                    let text = match frame? {
                        Message::Text(t) => t.as_str().to_string(),
                        Message::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
                        Message::Close(_) => {
                            tracing::info!(shard = ?self.shard, "收到 Close 帧");
                            return Ok(Flow::Reconnect);
                        }
                        _ => continue,
                    };

                    match self.on_frame(&text)? {
                        Some(flow) => return Ok(flow),
                        None => {
                            awaiting_ack = false;
                        }
                    }
                }
            }
        }
    }

    /// 处理一帧下行数据。返回 `Some` 表示需要结束当前连接。
    fn on_frame(&mut self, text: &str) -> Result<Option<Flow>, GatewayError> {
        let raw: RawPayload = serde_json::from_str(text)?;

        if let Some(s) = raw.s {
            self.last_seq = s;
        }

        match raw.op {
            OpCode::Dispatch => {
                // 单个事件解析失败**不能**拖垮长连接：跳过它并计数即可。
                // 否则一个字段的类型差异就会导致断线重连、期间事件丢失。
                let event = match Event::parse(raw.t.as_deref(), raw.d.as_deref(), raw.id.as_deref()) {
                    Ok(event) => event,
                    Err(err) => {
                        metrics::counter!("qqbot_gateway_event_parse_error_total").increment(1);
                        tracing::warn!(
                            shard = ?self.shard,
                            t = ?raw.t,
                            error = %err,
                            "事件解析失败，已跳过（连接保持）"
                        );
                        return Ok(None);
                    }
                };
                let Some(event) = event else { return Ok(None) };

                match &event {
                    Event::Ready(ready) => {
                        self.session_id = Some(ready.session_id.clone());
                        self.last_seq = raw.s.unwrap_or(1);
                        self.backoff.reset();
                        tracing::info!(
                            shard = ?ready.shard,
                            session_id = %ready.session_id,
                            user = %ready.user.username,
                            "READY，鉴权成功"
                        );
                    }
                    Event::Resumed => {
                        self.backoff.reset();
                        tracing::info!(shard = ?self.shard, seq = self.last_seq, "RESUMED，事件补发完成");
                    }
                    _ => {}
                }

                // 关键：绝不阻塞读循环。
                match self.events.try_send(Arc::new(event)) {
                    Ok(()) => {
                        metrics::counter!("qqbot_gateway_events_total").increment(1);
                    }
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        metrics::counter!("qqbot_gateway_events_dropped_total").increment(1);
                        tracing::warn!(shard = ?self.shard, "事件通道已满，丢弃事件");
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        return Ok(Some(Flow::Shutdown));
                    }
                }
            }
            OpCode::Hello => {}
            OpCode::HeartbeatAck => {
                tracing::debug!(shard = ?self.shard, "收到心跳 ACK (op 11)");
            }
            OpCode::Reconnect => {
                tracing::info!(shard = ?self.shard, "服务端要求重连 (op 7)");
                return Ok(Some(Flow::Reconnect));
            }
            OpCode::InvalidSession => {
                return Ok(Some(Flow::InvalidSession));
            }
            OpCode::Heartbeat | OpCode::Identify | OpCode::Resume => {}
            OpCode::HttpCallbackAck | OpCode::CallbackVerify => {}
            OpCode::Unknown(v) => {
                tracing::debug!(shard = ?self.shard, opcode = v, "忽略未知 opcode");
            }
        }

        Ok(None)
    }
}

// ---------- 辅助函数 ----------

type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

async fn send_op<T: Serialize>(ws: &mut Ws, op: OpCode, d: T) -> Result<(), GatewayError> {
    let payload: Payload<T> = Payload::new(op, Some(d));
    let text = serde_json::to_string(&payload)?;
    ws.send(Message::text(text)).await?;
    Ok(())
}

async fn next_text<S>(ws: &mut WebSocketStream<S>) -> Result<Option<String>, GatewayError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    while let Some(frame) = ws.next().await {
        match frame? {
            Message::Text(t) => return Ok(Some(t.as_str().to_string())),
            Message::Binary(b) => return Ok(Some(String::from_utf8_lossy(&b).into_owned())),
            Message::Close(_) => return Ok(None),
            _ => continue,
        }
    }
    Ok(None)
}
