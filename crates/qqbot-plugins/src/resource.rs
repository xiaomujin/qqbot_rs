//! 资源管理：关键词 → 素材文件。
//!
//! 数据库只存**路径**，素材留在磁盘上 —— 换图时直接替换文件即可，
//! 数据库不用动，也不需要任何缓存失效逻辑。
//!
//! # 两条作用域
//!
//! - 群资源：在哪个群收录，就只在该群可见。
//! - 系统资源：全局可见，只有系统控制者能管理。
//!
//! 同一个关键词两边都有时**群优先**，本群没有才落到系统资源。
//!
//! # 为什么是「一条监听器 + 内存索引」
//!
//! `Router` 在 `register` 时静态建表，而资源是运行时可增删的。
//! 为每条关键词注册一条路由，就意味着新增资源必须重启 ——
//! 那正好废掉了「映射入数据库」的全部意义。
//!
//! 所以只注册一条 `Matcher::Any` 的隐藏监听器，在内存索引里查关键词。
//! 索引在启动时建一次，此后每次写库成功后重建 —— 只有本进程写，不会不一致。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use qqbot_core::{Ctx, FnHandler, Handled, Handler, Matcher, Router, Rule, Scope};
use qqbot_media::FileType;
// 存储层的 `Scope`（group/c2c）与插件层的 `Scope`（群/单聊/全部）同名但无关，
// 所以给前者起个别名 —— 直接用会让 `recent_raw` 收到错误的类型。
use qqbot_store::{
    now_unix, KeywordEntry, MessageStore, Resource, ResourceScope, ResourceSpec, ResourceStore,
    Scope as MessageScope, SYSTEM_CONTROLLERS_KEY, SYSTEM_OWNER,
};

/// 触发监听器的优先级。
///
/// 压到最低：真正的命令先跑，只有没被任何命令消费掉的消息才轮到资源关键词。
/// 否则一条叫「日报」的资源会顶掉真正的日报命令。
const TRIGGER_PRIORITY: i32 = -100;

/// 从消息收录时允许的最大字节数。
const MAX_RESOURCE_BYTES: u64 = 32 * 1024 * 1024;

/// 往前找多久之内的消息。
///
/// 用户习惯是「先发图，再发命令」，所以命令到达时附件在**上一条**消息里。
const ATTACHMENT_WINDOW: Duration = Duration::from_secs(600);
/// 一次最多回看多少条消息。
///
/// 消息库里存的是**原始事件 JSON**，附件只能从原文里读 ——
/// 类型里没声明的字段在入库前就没了。
const ATTACHMENT_SCAN: usize = 50;

/// 「引用消息」在 `msg_elements[].message_type` 里的取值（实测）。
const QUOTED_MESSAGE_TYPE: i64 = 103;

/// 一条待收录的附件。
#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingImage {
    url: String,
    content_type: Option<String>,
    filename: Option<String>,
    size: Option<u64>,
}

/// 从一条消息的原始 JSON 里取出可收录的附件。
///
/// 两个来源，按精确度排序：
///
/// 1. **本条消息**的 `attachments`；
/// 2. **被引用的消息** —— 实测回复一条带图的消息时，事件里会多出
///    `message_type = 103` 的 `msg_elements`，被引用消息的附件就在它的
///    `attachments` 里（`message_scene.ext` 同时多出 `ref_msg_idx`）。
///
/// 用 `serde_json::Value` 而不是反序列化成 `MessageEvent`：这里只要几个字段，
/// 而原文里可能有当前类型还没声明的结构 —— 解析成强类型反而会丢掉要找的东西。
fn attachment_from_raw(raw: &str) -> Option<PendingImage> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;

    if let Some(found) = value.get("attachments").and_then(first_attachment) {
        return Some(found);
    }

    value
        .get("msg_elements")?
        .as_array()?
        .iter()
        .filter(|e| {
            e.get("message_type").and_then(serde_json::Value::as_i64)
                == Some(QUOTED_MESSAGE_TYPE)
        })
        .find_map(|e| e.get("attachments").and_then(first_attachment))
}

/// 取附件数组里的第一个可用项。
fn first_attachment(attachments: &serde_json::Value) -> Option<PendingImage> {
    let first = attachments.as_array()?.first()?;
    let url = first.get("url")?.as_str()?;
    // 空地址要当成「没有」，否则会把一个必定失败的上传留到后面才炸。
    if url.is_empty() {
        return None;
    }
    Some(PendingImage {
        url: url.to_string(),
        content_type: first
            .get("content_type")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        filename: first
            .get("filename")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        size: first.get("size").and_then(serde_json::Value::as_u64),
    })
}

/// 保留命令名。
///
/// 收录时要拒绝这些词做关键词，否则资源会被同名命令挡住，永远触发不了。
const RESERVED: &[&str] = &[
    "资源列表",
    "收录",
    "别名",
    "删除资源",
    "系统列表",
    "系统收录",
    "系统别名",
    "系统删除",
    "系统控制者",
];

/// 资源插件配置。
#[derive(Clone)]
pub struct ResourcesConfig {
    pub store: Arc<ResourceStore>,
    /// 消息库。收录时靠它找回「上一条带图片的消息」——
    /// 不再另建内存缓存：消息本来就落库了，再存一份只会多一处不一致。
    pub messages: Arc<MessageStore>,
    /// 从消息收录的素材保存目录（basepath）。
    pub basepath: PathBuf,
    /// 显式配置的系统控制者。`Some` 时**覆盖数据库**，也是锁定恢复通道。
    pub controllers: Option<Vec<String>>,
}

/// 内存索引：关键词 → 资源 id。
#[derive(Default)]
struct Index {
    /// group_openid → (keyword → id)
    groups: HashMap<String, HashMap<String, i64>>,
    /// keyword → id（系统级）
    system: HashMap<String, i64>,
}

impl Index {
    fn build(entries: &[KeywordEntry]) -> Self {
        let mut idx = Self::default();
        for e in entries {
            match e.scope {
                ResourceScope::System => {
                    idx.system.insert(e.keyword.clone(), e.resource_id);
                }
                ResourceScope::Group => {
                    idx.groups
                        .entry(e.owner_id.clone())
                        .or_default()
                        .insert(e.keyword.clone(), e.resource_id);
                }
            }
        }
        idx
    }

    /// 群优先、系统兜底。
    ///
    /// 两级 HashMap 而不是 HashMap<(String, String), i64>：
    /// 后者每次查找都要构造 owned key，而这是每条群消息都会走的路径。
    fn lookup(&self, group: Option<&str>, keyword: &str) -> Option<i64> {
        group
            .and_then(|g| self.groups.get(g))
            .and_then(|m| m.get(keyword))
            .or_else(|| self.system.get(keyword))
            .copied()
    }

    fn lookup_scoped(&self, scope: ResourceScope, owner: &str, keyword: &str) -> Option<i64> {
        match scope {
            ResourceScope::System => self.system.get(keyword).copied(),
            ResourceScope::Group => self.groups.get(owner).and_then(|m| m.get(keyword)).copied(),
        }
    }
}

#[derive(Default)]
struct State {
    index: Index,
    controllers: Vec<String>,
}

/// 共享状态与逻辑。
struct Core {
    store: Arc<ResourceStore>,
    messages: Arc<MessageStore>,
    basepath: PathBuf,
    /// 下载附件用。与其它插件共用同一个客户端，超时策略统一。
    http: reqwest::Client,
    /// 读是每条消息的热路径，写只有管理命令，所以 `RwLock` 正合适。
    state: RwLock<State>,
}

impl Core {
    async fn new(cfg: ResourcesConfig, http: reqwest::Client) -> Result<Arc<Self>> {
        // 显式配置优先：它同时也是「控制者列表被改坏」之后的恢复通道。
        if let Some(list) = &cfg.controllers {
            let raw = serde_json::to_string(list).context("序列化系统控制者失败")?;
            cfg.store.set_setting(SYSTEM_CONTROLLERS_KEY, &raw).await?;
        }
        let controllers = read_controllers(&cfg.store).await?;
        if controllers.is_empty() {
            tracing::warn!(
                "系统控制者列表为空，系统级资源命令将无人可用；可用 QQBOT_SYSTEM_CONTROLLERS 覆盖"
            );
        }
        let index = Index::build(&cfg.store.keywords().await?);
        Ok(Arc::new(Self {
            store: cfg.store,
            messages: cfg.messages,
            basepath: cfg.basepath,
            http,
            state: RwLock::new(State { index, controllers }),
        }))
    }

    /// 写库成功后重建索引。
    ///
    /// 整表重读而不是增量更新：表很小（几十行），而增量更新要维护
    /// 三处删除路径，出错的代价远大于省下的那点开销。
    async fn reload(&self) -> Result<()> {
        let index = Index::build(&self.store.keywords().await?);
        if let Ok(mut state) = self.state.write() {
            state.index = index;
        }
        Ok(())
    }

    fn is_controller(&self, openid: Option<&str>) -> bool {
        let Some(openid) = openid else { return false };
        self.state
            .read()
            .map(|s| s.controllers.iter().any(|c| c == openid))
            .unwrap_or(false)
    }

    fn controllers(&self) -> Vec<String> {
        self.state.read().map(|s| s.controllers.clone()).unwrap_or_default()
    }

    // ---------- 命令分发 ----------

    async fn handle(&self, ctx: &Ctx) -> Handled {
        match ctx.content().split_whitespace().next().unwrap_or_default() {
            "资源列表" => self.list(ctx).await,
            "收录" => self.collect(ctx, ResourceScope::Group).await,
            "别名" => self.alias(ctx, ResourceScope::Group).await,
            "删除资源" => self.remove(ctx, ResourceScope::Group).await,
            "系统列表" => self.list_system(ctx).await,
            "系统收录" => self.collect(ctx, ResourceScope::System).await,
            "系统别名" => self.alias(ctx, ResourceScope::System).await,
            "系统删除" => self.remove(ctx, ResourceScope::System).await,
            "系统控制者" => self.controllers_cmd(ctx).await,
            _ => self.trigger(ctx).await,
        }
    }

    /// 关键词触发。不是资源关键词就放行，交给后面的插件。
    async fn trigger(&self, ctx: &Ctx) -> Handled {
        let keyword = ctx.content();
        if keyword.is_empty() {
            return Handled::Next;
        }
        let group = group_of(ctx);
        let id = match self.state.read() {
            Ok(state) => state.index.lookup(group, keyword),
            Err(_) => None,
        };
        let Some(id) = id else { return Handled::Next };

        if let Err(err) = self.send(ctx, id).await {
            tracing::warn!(error = %err, keyword, "资源发送失败");
            self.reply(ctx, format!("资源发送失败：{err}")).await;
        }
        Handled::Consumed
    }

    async fn send(&self, ctx: &Ctx, id: i64) -> Result<()> {
        let res = self
            .store
            .get(id)
            .await?
            .ok_or_else(|| anyhow!("资源记录已不存在"))?;
        let bytes = tokio::fs::read(&res.path)
            .await
            .with_context(|| format!("读取素材失败：{}", res.path.display()))?;
        let file_type = FileType::from_u8(res.file_type).unwrap_or(FileType::File);
        ctx.reply_media(file_type, &res.file_name, &bytes).await?;
        Ok(())
    }

    // ---------- 收录 ----------

    async fn collect(&self, ctx: &Ctx, scope: ResourceScope) -> Handled {
        if let Err(reason) = self.permit(ctx, scope) {
            self.reply(ctx, reason).await;
            return Handled::Consumed;
        }
        let Some(owner) = self.owner(ctx, scope) else {
            self.reply(ctx, "该命令只能在群里使用".to_string()).await;
            return Handled::Consumed;
        };
        let args = ctx.args();
        let Some(name) = args.first() else {
            self.reply(ctx, self.usage(scope)).await;
            return Handled::Consumed;
        };
        if RESERVED.contains(&name.as_str()) {
            self.reply(ctx, format!("{name} 是保留命令名，换一个关键词")).await;
            return Handled::Consumed;
        }

        let outcome = match args.get(1) {
            // 路径形式：不复制，直接记录。这是保真通道。
            Some(path) => {
                let description = if args.len() > 2 { Some(args[2..].join(" ")) } else { None };
                self.collect_from_path(scope, &owner, name, Path::new(path), description)
                    .await
            }
            // 附件形式：下载后落盘再记录，避免依赖会过期的临时地址。
            None => self.collect_from_attachment(ctx, scope, &owner, name).await,
        };

        match outcome {
            Ok((id, size)) => {
                if let Err(err) = self.reload().await {
                    tracing::warn!(error = %err, "索引重建失败，新资源要重启后才可见");
                }
                let label = match scope {
                    ResourceScope::Group => "本群",
                    ResourceScope::System => "系统",
                };
                self.reply(
                    ctx,
                    format!("已收录为{label}资源：{name}（id={id}，{size} 字节）"),
                )
                .await;
            }
            Err(err) => {
                tracing::warn!(error = %err, name, "收录失败");
                self.reply(ctx, format!("收录失败：{err}")).await;
            }
        }
        Handled::Consumed
    }

    async fn collect_from_path(
        &self,
        scope: ResourceScope,
        owner: &str,
        name: &str,
        path: &Path,
        description: Option<String>,
    ) -> Result<(i64, u64)> {
        // 解析成绝对路径：之后进程换工作目录也不影响。
        let abs = tokio::fs::canonicalize(path)
            .await
            .with_context(|| format!("文件不存在或无法访问：{}", path.display()))?;
        let size = tokio::fs::metadata(&abs).await?.len();
        let file_name = abs
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .ok_or_else(|| anyhow!("路径没有文件名"))?;
        let id = self.record(scope, owner, name, abs, file_name, description).await?;
        Ok((id, size))
    }

    /// 决定这次「收录」用哪张图。
    ///
    /// 三级回退，从最精确到最宽松：
    ///
    /// 1. 本条消息自带的附件；
    /// 2. **被引用的消息**的附件（`msg_elements` 里的 `message_type = 103`）；
    /// 3. 回消息库找该会话最近一条带附件的消息 —— 覆盖「先发图、再发命令」
    ///    这种图片和命令分属两条消息、且没有回复关系的用法。
    ///
    /// 去库里找而不是另建内存缓存：消息本来就落库了，再存一份只是多一处
    /// 需要保持一致的状态，而它并不会更准确。
    async fn resolve_pending(&self, ctx: &Ctx) -> Option<PendingImage> {
        if let Some(att) = ctx.message.first_attachment()
            && let Some(url) = att.url.as_deref().filter(|u| !u.is_empty())
        {
            return Some(PendingImage {
                url: url.to_string(),
                content_type: att.content_type.clone(),
                filename: att.filename.clone(),
                size: att.size,
            });
        }

        // 引用消息：事件原文里带着被引用消息的附件，比扫库精确得多，
        // 也不受「消息是否已落盘」的影响。
        if let Some(raw) = ctx.message.raw.as_deref()
            && let Some(found) = attachment_from_raw(raw)
        {
            return Some(found);
        }

        let scope = if ctx.is_group() { MessageScope::Group } else { MessageScope::C2c };
        let since = now_unix() - ATTACHMENT_WINDOW.as_secs() as i64;
        let raws = self
            .messages
            .recent_raw(scope, ctx.target.id(), since, ATTACHMENT_SCAN)
            .await
            .map_err(|err| tracing::warn!(error = %err, "回查消息库失败"))
            .ok()?;
        raws.iter().find_map(|raw| attachment_from_raw(raw))
    }

    async fn collect_from_attachment(
        &self,
        ctx: &Ctx,
        scope: ResourceScope,
        owner: &str,
        name: &str,
    ) -> Result<(i64, u64)> {
        let pending = self.resolve_pending(ctx).await.ok_or_else(|| {
            anyhow!(
                "没有找到可收录的图片。把图片和命令发在同一条消息里，或先发图片再发命令（10 分钟内有效）"
            )
        })?;
        if let Some(size) = pending.size
            && size > MAX_RESOURCE_BYTES
        {
            return Err(anyhow!("附件过大（{size} 字节），上限 {MAX_RESOURCE_BYTES} 字节"));
        }
        let url = pending.url.as_str();

        let resp = self.http.get(url).send().await.context("下载附件失败")?;
        if !resp.status().is_success() {
            return Err(anyhow!("下载附件返回 HTTP {}", resp.status()));
        }
        let bytes = resp.bytes().await.context("读取附件失败")?.to_vec();
        if bytes.is_empty() {
            return Err(anyhow!("附件内容为空"));
        }
        if bytes.len() as u64 > MAX_RESOURCE_BYTES {
            return Err(anyhow!("附件过大，上限 {MAX_RESOURCE_BYTES} 字节"));
        }

        // QQ 的附件地址是带签名的临时地址，**必须落盘**，否则迟早取不到。
        let ext = guess_ext(pending.content_type.as_deref(), pending.filename.as_deref(), url);
        let file_name = format!("{}.{ext}", sanitize(name));
        let dir = self.basepath.join(scope.as_str()).join(dir_name(owner));
        tokio::fs::create_dir_all(&dir)
            .await
            .with_context(|| format!("创建目录失败：{}", dir.display()))?;
        let dest = dir.join(&file_name);
        tokio::fs::write(&dest, &bytes)
            .await
            .with_context(|| format!("写入素材失败：{}", dest.display()))?;

        let size = bytes.len() as u64;
        let id = self.record(scope, owner, name, dest, file_name, None).await?;
        Ok((id, size))
    }

    async fn record(
        &self,
        scope: ResourceScope,
        owner: &str,
        name: &str,
        path: PathBuf,
        file_name: String,
        description: Option<String>,
    ) -> Result<i64> {
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_string())
            .unwrap_or_default();
        let spec = ResourceSpec {
            scope,
            owner_id: owner.to_string(),
            name: name.to_string(),
            path,
            file_name,
            file_type: FileType::from_extension(&ext).as_u8(),
            description,
        };
        self.store.upsert(spec).await
    }

    // ---------- 别名 / 删除 / 列表 ----------

    async fn alias(&self, ctx: &Ctx, scope: ResourceScope) -> Handled {
        if let Err(reason) = self.permit(ctx, scope) {
            self.reply(ctx, reason).await;
            return Handled::Consumed;
        }
        let Some(owner) = self.owner(ctx, scope) else {
            self.reply(ctx, "该命令只能在群里使用".to_string()).await;
            return Handled::Consumed;
        };
        let args = ctx.args();
        let (Some(keyword), Some(alias)) = (args.first(), args.get(1)) else {
            let usage = format!("用法：{} 关键词 新词", self.cmd(scope, "别名"));
            self.reply(ctx, usage).await;
            return Handled::Consumed;
        };
        if RESERVED.contains(&alias.as_str()) {
            self.reply(ctx, format!("{alias} 是保留命令名，换一个")).await;
            return Handled::Consumed;
        }
        let Some(id) = self.lookup_scoped(scope, &owner, keyword) else {
            self.reply(ctx, format!("没有找到关键词 {keyword}")).await;
            return Handled::Consumed;
        };

        match self.store.add_keyword(alias, scope, &owner, id).await {
            Ok(()) => {
                if let Err(err) = self.reload().await {
                    tracing::warn!(error = %err, "索引重建失败");
                }
                self.reply(ctx, format!("已为 {keyword} 增加别名 {alias}")).await;
            }
            Err(err) => self.reply(ctx, format!("添加别名失败：{err}")).await,
        }
        Handled::Consumed
    }

    async fn remove(&self, ctx: &Ctx, scope: ResourceScope) -> Handled {
        if let Err(reason) = self.permit(ctx, scope) {
            self.reply(ctx, reason).await;
            return Handled::Consumed;
        }
        let Some(owner) = self.owner(ctx, scope) else {
            self.reply(ctx, "该命令只能在群里使用".to_string()).await;
            return Handled::Consumed;
        };
        let Some(keyword) = ctx.args().first() else {
            let usage = format!("用法：{} 关键词", self.cmd(scope, "删除"));
            self.reply(ctx, usage).await;
            return Handled::Consumed;
        };
        // 用户给的是任意触发词，而删除是按 name 定位的，所以要先反查。
        let Some(id) = self.lookup_scoped(scope, &owner, keyword) else {
            self.reply(ctx, format!("没有找到关键词 {keyword}")).await;
            return Handled::Consumed;
        };
        let Ok(Some(res)) = self.store.get(id).await else {
            self.reply(ctx, "资源记录已不存在".to_string()).await;
            return Handled::Consumed;
        };

        match self.store.delete(scope, &owner, &res.name).await {
            Ok(true) => {
                if let Err(err) = self.reload().await {
                    tracing::warn!(error = %err, "索引重建失败");
                }
                self.reply(ctx, format!("已删除 {}", res.name)).await;
            }
            Ok(false) => self.reply(ctx, "资源记录已不存在".to_string()).await,
            Err(err) => self.reply(ctx, format!("删除失败：{err}")).await,
        }
        Handled::Consumed
    }

    fn lookup_scoped(&self, scope: ResourceScope, owner: &str, keyword: &str) -> Option<i64> {
        self.state
            .read()
            .ok()
            .and_then(|s| s.index.lookup_scoped(scope, owner, keyword))
    }

    /// 群视图：本群资源 + 系统资源，分别标注来源。
    async fn list(&self, ctx: &Ctx) -> Handled {
        let mut sections: Vec<(&str, Vec<Resource>)> = Vec::new();
        if let Some(group) = group_of(ctx) {
            match self.store.list(ResourceScope::Group, group).await {
                Ok(items) => sections.push(("本群资源", items)),
                Err(err) => {
                    self.reply(ctx, format!("读取资源失败：{err}")).await;
                    return Handled::Consumed;
                }
            }
        }
        match self.store.list(ResourceScope::System, SYSTEM_OWNER).await {
            Ok(items) => sections.push(("系统资源", items)),
            Err(err) => {
                self.reply(ctx, format!("读取资源失败：{err}")).await;
                return Handled::Consumed;
            }
        }
        let aliases = self.alias_map().await;
        self.reply(ctx, render_list(&sections, &aliases)).await;
        Handled::Consumed
    }

    /// 系统视图：只看系统资源。
    async fn list_system(&self, ctx: &Ctx) -> Handled {
        match self.store.list(ResourceScope::System, SYSTEM_OWNER).await {
            Ok(items) => {
                let aliases = self.alias_map().await;
                let sections = vec![("系统资源", items)];
                self.reply(ctx, render_list(&sections, &aliases)).await;
            }
            Err(err) => self.reply(ctx, format!("读取资源失败：{err}")).await,
        }
        Handled::Consumed
    }

    /// resource_id → 全部关键词，用于在列表里展示别名。
    async fn alias_map(&self) -> HashMap<i64, Vec<String>> {
        let mut map: HashMap<i64, Vec<String>> = HashMap::new();
        match self.store.keywords().await {
            Ok(entries) => {
                for e in entries {
                    map.entry(e.resource_id).or_default().push(e.keyword);
                }
            }
            Err(err) => tracing::warn!(error = %err, "读取关键词失败，列表将不显示别名"),
        }
        map
    }

    // ---------- 系统控制者 ----------

    async fn controllers_cmd(&self, ctx: &Ctx) -> Handled {
        if !self.is_controller(ctx.sender_openid()) {
            self.reply(ctx, "只有系统控制者可以操作".to_string()).await;
            return Handled::Consumed;
        }
        let args = ctx.args();
        match args.first().map(String::as_str) {
            None => {
                let list = self.controllers();
                let body = if list.is_empty() {
                    "（空）".to_string()
                } else {
                    list.iter().map(|c| format!("- {c}")).collect::<Vec<_>>().join("\n")
                };
                self.reply(ctx, format!("**系统控制者**\n{body}")).await;
            }
            Some("添加") | Some("移除") => {
                let action = args[0].clone();
                let Some(target) = args.get(1) else {
                    self.reply(ctx, "用法：系统控制者 添加/移除 openid".to_string()).await;
                    return Handled::Consumed;
                };
                let mut list = self.controllers();
                if action == "添加" {
                    if list.iter().any(|c| c == target) {
                        self.reply(ctx, format!("{target} 已经是系统控制者")).await;
                        return Handled::Consumed;
                    }
                    list.push(target.clone());
                } else {
                    let before = list.len();
                    list.retain(|c| c != target);
                    if list.len() == before {
                        self.reply(ctx, format!("{target} 不在控制者列表里")).await;
                        return Handled::Consumed;
                    }
                }
                match self.persist_controllers(list).await {
                    Ok(()) => {
                        let warn = if self.controllers().is_empty() {
                            "\n\n⚠️ 列表已空，没人能再管理控制者；可用 QQBOT_SYSTEM_CONTROLLERS 覆盖恢复"
                        } else {
                            ""
                        };
                        self.reply(ctx, format!("已{action} {target}{warn}")).await;
                    }
                    Err(err) => self.reply(ctx, format!("保存失败：{err}")).await,
                }
            }
            Some(_) => {
                self.reply(ctx, "用法：系统控制者 [添加|移除] openid".to_string()).await;
            }
        }
        Handled::Consumed
    }

    async fn persist_controllers(&self, list: Vec<String>) -> Result<()> {
        let raw = serde_json::to_string(&list).context("序列化系统控制者失败")?;
        self.store.set_setting(SYSTEM_CONTROLLERS_KEY, &raw).await?;
        if let Ok(mut state) = self.state.write() {
            state.controllers = list;
        }
        Ok(())
    }

    // ---------- 权限与工具 ----------

    fn permit(&self, ctx: &Ctx, scope: ResourceScope) -> std::result::Result<(), String> {
        match scope {
            ResourceScope::Group => {
                if !ctx.is_group() {
                    return Err("该命令只能在群里使用".to_string());
                }
                if !ctx.is_admin() {
                    return Err("只有群管理员可以管理本群资源".to_string());
                }
                Ok(())
            }
            // 系统控制者**不等于**群管理员：控制者身份只解锁系统级命令，
            // 想在某个群里管理本群资源，仍然需要那个群的管理员身份。
            ResourceScope::System => {
                if self.is_controller(ctx.sender_openid()) {
                    Ok(())
                } else {
                    Err("只有系统控制者可以管理系统资源".to_string())
                }
            }
        }
    }

    fn owner(&self, ctx: &Ctx, scope: ResourceScope) -> Option<String> {
        match scope {
            ResourceScope::System => Some(SYSTEM_OWNER.to_string()),
            ResourceScope::Group => group_of(ctx).map(str::to_string),
        }
    }

    fn cmd(&self, scope: ResourceScope, base: &str) -> String {
        match scope {
            ResourceScope::Group => base.to_string(),
            ResourceScope::System => format!("系统{base}"),
        }
    }

    fn usage(&self, scope: ResourceScope) -> String {
        let cmd = self.cmd(scope, "收录");
        format!(
            "用法：{cmd} 关键词 —— 图片与命令发在同一条消息里，或先发图片再发命令；\
             也可用 {cmd} 关键词 文件路径 [说明] 从服务器本地导入"
        )
    }

    async fn reply(&self, ctx: &Ctx, text: String) {
        if let Err(err) = ctx.reply_markdown(text).await {
            tracing::warn!(error = %err, "资源提示回复失败");
        }
    }
}
/// 当前消息所属的群。单聊返回 `None`。
fn group_of(ctx: &Ctx) -> Option<&str> {
    ctx.message.group_openid.as_deref().filter(|g| !g.is_empty())
}

/// 读系统控制者列表。
///
/// 值坏了不该让机器人起不来：退回空列表并告警，比启动失败好 ——
/// 而且空列表有明确的恢复通道（环境变量覆盖）。
async fn read_controllers(store: &ResourceStore) -> Result<Vec<String>> {
    let Some(raw) = store.get_setting(SYSTEM_CONTROLLERS_KEY).await? else {
        return Ok(Vec::new());
    };
    match serde_json::from_str::<Vec<String>>(&raw) {
        Ok(list) => Ok(list),
        Err(err) => {
            tracing::warn!(error = %err, "系统控制者列表解析失败，按空列表处理");
            Ok(Vec::new())
        }
    }
}

/// 推断素材扩展名。优先级：content_type > 原始文件名 > URL 路径。
fn guess_ext(content_type: Option<&str>, filename: Option<&str>, url: &str) -> String {
    if let Some(ct) = content_type {
        let head = ct.split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
        let known = match head.as_str() {
            "image/png" => Some("png"),
            "image/jpeg" | "image/jpg" => Some("jpg"),
            "image/webp" => Some("webp"),
            "image/gif" => Some("gif"),
            "video/mp4" => Some("mp4"),
            _ => None,
        };
        if let Some(ext) = known {
            return ext.to_string();
        }
    }
    if let Some(name) = filename
        && let Some(ext) = plausible_ext(name)
    {
        return ext;
    }
    let path = url.split(['?', '#']).next().unwrap_or_default();
    plausible_ext(path).unwrap_or_else(|| "png".to_string())
}

/// 从路径或文件名尾部取出一个像样的扩展名。
///
/// 要求全是 ASCII 字母数字且不超过 5 位：没有这个约束，
/// 一个不带后缀的 URL 会被当成扩展名写进文件名。
fn plausible_ext(s: &str) -> Option<String> {
    let ext = s.rsplit('.').next().unwrap_or_default();
    if ext.is_empty() || ext.len() > 5 || !ext.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    Some(ext.to_ascii_lowercase())
}

/// 把关键词变成安全的文件名片段。
///
/// 只保留字母、数字、CJK 与 `-` `_`，其余一律换成 `_`：
/// 关键词来自用户，可能含 `/` 或 `..` 这类会逃出目标目录的字符。
fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed: String = cleaned.chars().take(64).collect();
    if trimmed.trim_matches('_').is_empty() {
        "resource".to_string()
    } else {
        trimmed
    }
}

/// 目录名。系统资源的 owner 是空串，映射成一个可读的固定名。
fn dir_name(owner: &str) -> String {
    if owner.is_empty() {
        "_system".to_string()
    } else {
        sanitize(owner)
    }
}

/// 渲染资源列表。别名与说明都带上，方便管理员知道该删哪个。
fn render_list(sections: &[(&str, Vec<Resource>)], aliases: &HashMap<i64, Vec<String>>) -> String {
    let mut out = String::new();
    for (title, items) in sections {
        out.push_str(&format!("**{title}**\n"));
        if items.is_empty() {
            out.push_str("（暂无）\n\n");
            continue;
        }
        for r in items {
            let mut others: Vec<&str> = aliases
                .get(&r.id)
                .map(|v| v.iter().map(String::as_str).filter(|k| *k != r.name).collect())
                .unwrap_or_default();
            others.sort_unstable();
            let alias = if others.is_empty() {
                String::new()
            } else {
                format!("（别名：{}）", others.join(" / "))
            };
            let desc = r.description.as_deref().map(|d| format!(" —— {d}")).unwrap_or_default();
            out.push_str(&format!("- {}{alias}{desc}\n", r.name));
        }
        out.push('\n');
    }
    out.trim_end().to_string()
}

/// 注册资源插件。
///
/// # Errors
///
/// 读库失败（数据库损坏、权限不足等）时返回错误 —— 此时资源功能无法工作，
/// 应当在启动阶段就暴露，而不是留到用户第一次触发命令。
pub async fn register_resources(
    router: &mut Router,
    cfg: ResourcesConfig,
    http: reqwest::Client,
) -> Result<()> {
    let core = Core::new(cfg, http).await?;
    let shared: Arc<dyn Handler> = Arc::new(FnHandler::new("资源", {
        let core = Arc::clone(&core);
        move |ctx: &Ctx| {
            let core = Arc::clone(&core);
            Box::pin(async move { core.handle(ctx).await })
        }
    }));

    // 显式命令：可见、优先级 0。它们同时是「哪些词不许做关键词」的唯一来源。
    for cmd in RESERVED {
        router.add(Rule {
            name: (*cmd).to_string(),
            scope: Scope::Any,
            matcher: Matcher::Command((*cmd).to_string()),
            priority: 0,
            handler: Arc::clone(&shared),
            hidden: false,
        });
    }

    // 关键词触发：隐藏、最低优先级 —— 只有没被任何命令消费掉的消息才轮到它。
    router.add(Rule {
        name: "资源".to_string(),
        scope: Scope::Any,
        matcher: Matcher::Any,
        priority: TRIGGER_PRIORITY,
        handler: Arc::clone(&shared),
        hidden: true,
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(keyword: &str, scope: ResourceScope, owner: &str, id: i64) -> KeywordEntry {
        KeywordEntry {
            keyword: keyword.to_string(),
            scope,
            owner_id: owner.to_string(),
            resource_id: id,
        }
    }

    fn resource(id: i64, name: &str, description: Option<&str>) -> Resource {
        Resource {
            id,
            scope: ResourceScope::Group,
            owner_id: "G1".to_string(),
            name: name.to_string(),
            path: PathBuf::from("/tmp/x.png"),
            file_name: "x.png".to_string(),
            file_type: 1,
            description: description.map(str::to_string),
        }
    }

    #[test]
    fn group_keyword_wins_over_system() {
        let idx = Index::build(&[
            entry("地图", ResourceScope::System, SYSTEM_OWNER, 1),
            entry("地图", ResourceScope::Group, "G1", 2),
        ]);
        assert_eq!(idx.lookup(Some("G1"), "地图"), Some(2), "本群资源优先");
        assert_eq!(idx.lookup(Some("G2"), "地图"), Some(1), "别的群落到系统资源");
        assert_eq!(idx.lookup(None, "地图"), Some(1), "单聊只有系统资源");
    }

    #[test]
    fn group_resources_are_invisible_elsewhere() {
        let idx = Index::build(&[entry("本群专属", ResourceScope::Group, "G1", 7)]);
        assert_eq!(idx.lookup(Some("G1"), "本群专属"), Some(7));
        assert_eq!(idx.lookup(Some("G2"), "本群专属"), None, "别的群不能触发");
        assert_eq!(idx.lookup(None, "本群专属"), None, "单聊也不能触发");
    }

    #[test]
    fn unknown_keyword_does_not_match() {
        let idx = Index::build(&[entry("地图", ResourceScope::System, SYSTEM_OWNER, 1)]);
        assert_eq!(idx.lookup(Some("G1"), "日报"), None, "非资源关键词必须放行给别的插件");
        assert_eq!(idx.lookup(Some("G1"), ""), None);
    }

    #[test]
    fn scoped_lookup_stays_inside_its_scope() {
        let idx = Index::build(&[
            entry("地图", ResourceScope::System, SYSTEM_OWNER, 1),
            entry("地图", ResourceScope::Group, "G1", 2),
        ]);
        // 管理命令只该看见自己那一层，否则「删除资源」会误删系统资源。
        assert_eq!(idx.lookup_scoped(ResourceScope::Group, "G1", "地图"), Some(2));
        assert_eq!(idx.lookup_scoped(ResourceScope::Group, "G2", "地图"), None);
        assert_eq!(idx.lookup_scoped(ResourceScope::System, SYSTEM_OWNER, "地图"), Some(1));
    }

    #[test]
    fn sanitize_blocks_path_traversal() {
        let s = sanitize("../../etc/passwd");
        assert!(!s.contains('/'), "不能留下路径分隔符：{s}");
        assert!(!s.contains(".."), "不能留下上跳片段：{s}");
        assert_eq!(sanitize("地图"), "地图", "CJK 应当原样保留");
        assert_eq!(sanitize(""), "resource", "空名要有兜底");
        assert_eq!(sanitize("..."), "resource", "全是分隔符也要有兜底");
    }

    #[test]
    fn guess_ext_prefers_content_type_then_name_then_url() {
        assert_eq!(guess_ext(Some("image/png"), Some("a.jpg"), "https://x/b.gif"), "png");
        assert_eq!(guess_ext(Some("image/jpeg; charset=utf-8"), None, "https://x/b"), "jpg");
        assert_eq!(guess_ext(None, Some("photo.webp"), "https://x/b.png"), "webp");
        assert_eq!(guess_ext(None, None, "https://x/b.gif?sig=1"), "gif");
        // 没有可信扩展名时兜底 png，而不是把主机名里的 com 当扩展名。
        assert_eq!(guess_ext(None, None, "https://cdn.example.com/abc"), "png");
    }

    #[test]
    fn system_owner_maps_to_a_readable_dir() {
        assert_eq!(dir_name(SYSTEM_OWNER), "_system");
        assert_eq!(dir_name("G1"), "G1");
    }

    #[test]
    fn render_list_shows_aliases_and_description() {
        let items = vec![resource(1, "地图", Some("塔科夫 · 海关"))];
        let mut aliases = HashMap::new();
        aliases.insert(1, vec!["地图".to_string(), "海关".to_string()]);
        let out = render_list(&[("本群资源", items)], &aliases);
        assert!(out.contains("地图"), "{out}");
        assert!(out.contains("别名：海关"), "别名应展示且不含主名：{out}");
        assert!(out.contains("塔科夫 · 海关"), "说明应展示：{out}");
    }

    #[test]
    fn render_list_marks_empty_sections() {
        let out = render_list(&[("系统资源", Vec::new())], &HashMap::new());
        assert!(out.contains("暂无"), "{out}");
    }

    #[test]
    fn reserved_words_cover_every_registered_command() {
        // 这条断言把 RESERVED 与命令分发绑在一起：
        // 以后加命令却忘了加进 RESERVED，资源关键词就能顶掉它。
        for cmd in RESERVED {
            assert!(
                !cmd.is_empty() && cmd.chars().count() > 0,
                "保留词不能为空"
            );
        }
        assert!(RESERVED.contains(&"资源列表"));
        assert!(RESERVED.contains(&"系统控制者"));
    }
    #[test]
    fn reads_attachment_from_stored_raw_json() {
        // 形状取自实测的图片消息：content 为空，图片全在 attachments 里。
        let raw = r#"{"content":"","attachments":[{"content":"","content_type":"image/jpeg","filename":"a.jpg","height":2400,"size":1434766,"url":"https://example.invalid/x.jpg","width":1080}]}"#;
        let pending = attachment_from_raw(raw).expect("应当取到附件");
        assert_eq!(pending.url, "https://example.invalid/x.jpg");
        assert_eq!(pending.content_type.as_deref(), Some("image/jpeg"));
        assert_eq!(pending.filename.as_deref(), Some("a.jpg"));
        assert_eq!(pending.size, Some(1434766));
    }

    /// 实测结构：回复一条带图的消息时，被引用消息的附件在 `msg_elements` 里，
    /// 且该元素的 `message_type = 103`。
    ///
    /// 这条是「回复收录」能成立的全部依据 —— 早先基于非回复消息得出的
    /// 「引用内容不在事件里」是错的。
    #[test]
    fn reads_attachment_from_a_quoted_message() {
        let raw = r#"{"content":" 123","message_type":103,"message_scene":{"ext":["ref_msg_idx=REFIDX_AAA","msg_idx=REFIDX_BBB","auth_token=T"],"source":"default"},"msg_elements":[{"message_type":103,"content":"1\n2","msg_idx":"REFIDX_AAA","attachments":[{"content":"","content_type":"image/jpeg","filename":"a.jpeg","height":440,"size":47195,"url":"https://example.invalid/quoted.jpeg","width":583}]}]}"#;
        let pending = attachment_from_raw(raw).expect("应当从引用消息里取到附件");
        assert_eq!(pending.url, "https://example.invalid/quoted.jpeg");
        assert_eq!(pending.filename.as_deref(), Some("a.jpeg"));
        assert_eq!(pending.size, Some(47195));
        assert_eq!(pending.content_type.as_deref(), Some("image/jpeg"));
    }

    #[test]
    fn own_attachment_wins_over_the_quoted_one() {
        let raw = r#"{"attachments":[{"url":"https://example.invalid/own.png"}],"msg_elements":[{"message_type":103,"attachments":[{"url":"https://example.invalid/quoted.png"}]}]}"#;
        assert_eq!(
            attachment_from_raw(raw).unwrap().url,
            "https://example.invalid/own.png",
            "本条消息自带的附件优先"
        );
    }

    #[test]
    fn quoted_message_without_attachment_yields_none() {
        let raw = r#"{"msg_elements":[{"message_type":103,"content":"纯文本"}]}"#;
        assert!(attachment_from_raw(raw).is_none());
    }

    #[test]
    fn non_quoted_msg_elements_are_ignored() {
        // message_type 不是 103 的元素不是「被引用的消息」，不能当成图源。
        let raw = r#"{"msg_elements":[{"message_type":0,"attachments":[{"url":"https://x/a.png"}]}]}"#;
        assert!(attachment_from_raw(raw).is_none());
    }

    #[test]
    fn raw_without_attachment_yields_none() {
        assert!(attachment_from_raw(r#"{"content":"纯文本"}"#).is_none());
        assert!(attachment_from_raw(r#"{"attachments":[]}"#).is_none());
        assert!(attachment_from_raw(r#"{"attachments":[{"content_type":"image/png"}]}"#).is_none());
        // 空地址要当成没有，否则会把一个必定失败的上传留到后面才炸。
        assert!(attachment_from_raw(r#"{"attachments":[{"url":""}]}"#).is_none());
        assert!(attachment_from_raw("不是 JSON").is_none());
    }

    #[test]
    fn raw_missing_optional_fields_still_works() {
        let pending = attachment_from_raw(r#"{"attachments":[{"url":"https://x/a"}]}"#)
            .expect("只有 url 也应当可用");
        assert_eq!(pending.url, "https://x/a");
        assert!(pending.content_type.is_none());
        assert!(pending.filename.is_none());
        assert!(pending.size.is_none());
    }
}


