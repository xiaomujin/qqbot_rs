use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use moka::sync::Cache;
use tokio::sync::{mpsc, oneshot, Semaphore};

use crate::error::RenderError;
use crate::svg::{RenderedImage, SvgRenderer};
use crate::template::TemplateEngine;

/// 渲染服务配置。
#[derive(Debug, Clone)]
pub struct RenderConfig {
    /// 并发渲染数。resvg 是纯 CPU 工作，取 CPU 核数即可。
    pub workers: usize,
    /// 单次渲染超时，超时即降级。
    pub timeout: Duration,
    /// 结果缓存条数。
    pub cache_capacity: u64,
    /// 默认输出倍率（2.0 = 高清）。
    pub default_scale: f32,
    /// 队列容量。满时 `send` 会等待，形成背压。
    pub queue_capacity: usize,
}

impl Default for RenderConfig {
    fn default() -> Self {
        Self {
            workers: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4),
            timeout: Duration::from_secs(3),
            cache_capacity: 512,
            default_scale: 2.0,
            queue_capacity: 256,
        }
    }
}

/// 渲染来源。
#[derive(Debug, Clone)]
pub enum RenderSource {
    /// 使用内置/已注册模板。
    Template { name: String, data: serde_json::Value },
    /// 直接给出 SVG 源码（例如词云这类由算法生成的布局）。
    Svg(String),
}

impl RenderSource {
    fn label(&self) -> &str {
        match self {
            RenderSource::Template { name, .. } => name,
            RenderSource::Svg(_) => "<inline-svg>",
        }
    }

    fn feed(&self, feed: &mut impl FnMut(&[u8])) {
        match self {
            RenderSource::Template { name, data } => {
                feed(name.as_bytes());
                feed(data.to_string().as_bytes());
            }
            RenderSource::Svg(svg) => feed(svg.as_bytes()),
        }
    }
}

/// 一次渲染请求。
#[derive(Debug, Clone)]
pub struct RenderRequest {
    pub source: RenderSource,
    /// 输出倍率；`<= 0` 表示用服务默认值。
    pub scale: f32,
    pub cache: bool,
}

impl RenderRequest {
    pub fn new(template: impl Into<String>, data: serde_json::Value) -> Self {
        Self {
            source: RenderSource::Template { name: template.into(), data },
            scale: 0.0,
            cache: true,
        }
    }

    /// 直接渲染一段 SVG 源码。
    pub fn from_svg(svg: impl Into<String>) -> Self {
        Self { source: RenderSource::Svg(svg.into()), scale: 0.0, cache: true }
    }

    pub fn template_name(&self) -> &str {
        self.source.label()
    }

    pub fn with_scale(mut self, scale: f32) -> Self {
        self.scale = scale;
        self
    }

    pub fn without_cache(mut self) -> Self {
        self.cache = false;
        self
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RenderStats {
    pub hits: u64,
    pub misses: u64,
    pub rendered: u64,
    pub failed: u64,
    pub timed_out: u64,
}

#[derive(Default)]
struct Metrics {
    hits: AtomicU64,
    misses: AtomicU64,
    rendered: AtomicU64,
    failed: AtomicU64,
    timed_out: AtomicU64,
}

struct Job {
    request: RenderRequest,
    key: u64,
    ack: oneshot::Sender<Result<RenderedImage, RenderError>>,
}

/// 渲染服务。
///
/// 结构上等价于「渲染 actor 池」：
/// - 有界 `mpsc` 队列 → 调用方背压
/// - `Semaphore` 限制并发渲染数
/// - CPU 密集的 resvg 调用跑在 `spawn_blocking`，不阻塞异步执行器
/// - 调用方 `timeout` 即可降级，不会拖垮上游
#[derive(Clone)]
pub struct RenderService {
    tx: mpsc::Sender<Job>,
    cache: Cache<u64, Arc<RenderedImage>>,
    timeout: Duration,
    default_scale: f32,
    metrics: Arc<Metrics>,
}

impl RenderService {
    pub fn new(cfg: RenderConfig) -> Self {
        let cache = Cache::builder().max_capacity(cfg.cache_capacity).build();
        let (tx, rx) = mpsc::channel(cfg.queue_capacity);

        let engine = Arc::new(TemplateEngine::new());
        let renderer = Arc::new(SvgRenderer::new());
        let metrics = Arc::new(Metrics::default());

        tracing::info!(
            workers = cfg.workers,
            fonts = renderer.font_count(),
            templates = ?engine.template_names(),
            "渲染服务已启动"
        );

        tokio::spawn(dispatcher(
            rx,
            engine,
            renderer,
            cache.clone(),
            Arc::new(Semaphore::new(cfg.workers.max(1))),
            metrics.clone(),
        ));

        Self {
            tx,
            cache,
            timeout: cfg.timeout,
            default_scale: cfg.default_scale,
            metrics,
        }
    }

    pub fn stats(&self) -> RenderStats {
        RenderStats {
            hits: self.metrics.hits.load(Ordering::Relaxed),
            misses: self.metrics.misses.load(Ordering::Relaxed),
            rendered: self.metrics.rendered.load(Ordering::Relaxed),
            failed: self.metrics.failed.load(Ordering::Relaxed),
            timed_out: self.metrics.timed_out.load(Ordering::Relaxed),
        }
    }

    /// 便捷入口：按模板名渲染。
    pub async fn render_template(
        &self,
        template: &str,
        data: serde_json::Value,
    ) -> Result<RenderedImage, RenderError> {
        self.render(RenderRequest::new(template, data)).await
    }

    /// 便捷入口：直接渲染 SVG 源码。
    pub async fn render_svg(&self, svg: impl Into<String>) -> Result<RenderedImage, RenderError> {
        self.render(RenderRequest::from_svg(svg)).await
    }

    pub async fn render(&self, req: RenderRequest) -> Result<RenderedImage, RenderError> {
        let scale = if req.scale > 0.0 { req.scale } else { self.default_scale };
        let key = cache_key(&req.source, scale);
        let use_cache = req.cache;

        if use_cache && let Some(hit) = self.cache.get(&key) {
            self.metrics.hits.fetch_add(1, Ordering::Relaxed);
            metrics::counter!("qqbot_render_cache_total", "result" => "hit").increment(1);
            return Ok((*hit).clone());
        }

        let (ack_tx, ack_rx) = oneshot::channel();
        let job = Job {
            request: RenderRequest { scale, ..req },
            key,
            ack: ack_tx,
        };

        self.tx.send(job).await.map_err(|_| RenderError::Closed)?;

        match tokio::time::timeout(self.timeout, ack_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(RenderError::Canceled),
            Err(_) => {
                self.metrics.timed_out.fetch_add(1, Ordering::Relaxed);
                metrics::counter!("qqbot_render_timeout_total").increment(1);
                Err(RenderError::Timeout)
            }
        }
    }
}

async fn dispatcher(
    mut rx: mpsc::Receiver<Job>,
    engine: Arc<TemplateEngine>,
    renderer: Arc<SvgRenderer>,
    cache: Cache<u64, Arc<RenderedImage>>,
    sem: Arc<Semaphore>,
    metrics: Arc<Metrics>,
) {
    while let Some(job) = rx.recv().await {
        if job.request.cache && let Some(hit) = cache.get(&job.key) {
            metrics.hits.fetch_add(1, Ordering::Relaxed);
            let _ = job.ack.send(Ok((*hit).clone()));
            continue;
        }
        metrics.misses.fetch_add(1, Ordering::Relaxed);
        metrics::counter!("qqbot_render_cache_total", "result" => "miss").increment(1);

        // 并发已满时在此等待 → recv 暂停 → 队列填满 → 调用方背压。
        let Ok(permit) = sem.clone().acquire_owned().await else { break };

        let engine = engine.clone();
        let renderer = renderer.clone();
        let cache = cache.clone();
        let metrics = metrics.clone();

        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let result = render_one(&engine, &renderer, &job.request);
            match &result {
                Ok(img) => {
                    metrics.rendered.fetch_add(1, Ordering::Relaxed);
                    metrics::counter!("qqbot_render_total", "result" => "ok").increment(1);
                    metrics::histogram!("qqbot_render_bytes").record(img.png.len() as f64);
                    if job.request.cache {
                        cache.insert(job.key, Arc::new(img.clone()));
                    }
                }
                Err(err) => {
                    metrics.failed.fetch_add(1, Ordering::Relaxed);
                    metrics::counter!("qqbot_render_total", "result" => "err").increment(1);
                    tracing::warn!(error = %err, source = %job.request.template_name(), "渲染失败");
                }
            }
            let _ = job.ack.send(result);
        });
    }
}

fn render_one(
    engine: &TemplateEngine,
    renderer: &SvgRenderer,
    req: &RenderRequest,
) -> Result<RenderedImage, RenderError> {
    // 两个分支只是所有权不同：模板渲染必然产出 String，内联 SVG 只是借用。
    // 用 Cow 避免为了统一类型而整体克隆一份 SVG（词云有 10–20KB）。
    let svg: std::borrow::Cow<'_, str> = match &req.source {
        RenderSource::Template { name, data } => std::borrow::Cow::Owned(engine.render(name, data)?),
        RenderSource::Svg(svg) => std::borrow::Cow::Borrowed(svg.as_str()),
    };
    renderer.render_png(&svg, req.scale)
}

/// 稳定哈希（FNV-1a），用作缓存 key。
fn cache_key(source: &RenderSource, scale: f32) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    {
        let mut feed = |bytes: &[u8]| {
            for b in bytes {
                h ^= *b as u64;
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
        };
        feed(&scale.to_bits().to_le_bytes());
        source.feed(&mut feed);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn svc() -> RenderService {
        RenderService::new(RenderConfig {
            workers: 2,
            timeout: Duration::from_secs(5),
            cache_capacity: 16,
            default_scale: 2.0,
            queue_capacity: 8,
        })
    }

    #[tokio::test]
    async fn renders_card_and_caches() {
        let s = svc();
        let data = json!({
            "title": "签到成功",
            "rows": [{"label": "小明", "value": "+1 天"}],
            "width": 720,
            "height": 240
        });

        let first = s.render_template("card.svg", data.clone()).await.unwrap();
        assert_eq!((first.width, first.height), (1440, 480));

        let second = s.render_template("card.svg", data).await.unwrap();
        assert_eq!(first.png, second.png);

        let st = s.stats();
        assert!(st.hits >= 1, "第二次应命中缓存: {st:?}");
    }

    #[tokio::test]
    async fn missing_template_surfaces_error() {
        let s = svc();
        let err = s.render_template("nope.svg", json!({})).await.unwrap_err();
        assert!(matches!(err, RenderError::Template(_)));
    }

    #[tokio::test]
    async fn cache_key_is_stable_and_sensitive() {
        let src = |v: i32| RenderSource::Template { name: "card.svg".into(), data: json!({"x": v}) };
        let a = cache_key(&src(1), 2.0);
        let b = cache_key(&src(1), 2.0);
        let c = cache_key(&src(2), 2.0);
        let d = cache_key(&src(1), 1.0);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, d);
    }

    #[tokio::test]
    async fn renders_inline_svg() {
        let s = svc();
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="60" height="40"><rect width="60" height="40" fill="#00ff00"/></svg>"##;
        let img = s.render_svg(svg).await.unwrap();
        assert_eq!((img.width, img.height), (120, 80));
    }
}
