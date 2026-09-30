//! 资源映射的持久化。
//!
//! 数据库只存**路径**，素材本身留在磁盘上 —— 换图时直接替换文件即可，
//! 数据库不用动，也不需要任何缓存失效逻辑。
//!
//! 资源分两种作用域：群自建的只在该群可见，系统级的全局可见。
//! 触发时**群优先、系统兜底**（见 `qqbot-plugins` 的 `ResourcePlugin`）。

use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use rusqlite::{Connection, OptionalExtension};

use crate::model::now_unix;

/// 首次建库时播种的系统控制者。
pub const DEFAULT_SYSTEM_CONTROLLER: &str = "5CF47107AFE2275EE0173D298F6FF07E";

/// 系统控制者列表在 `settings` 表里的键名。
pub const SYSTEM_CONTROLLERS_KEY: &str = "system_controllers";

/// 系统资源的 `owner_id` 哨兵值。
///
/// **必须是空串而不是 NULL**：SQLite 的 UNIQUE 索引把 NULL 视为互不相等，
/// 用 NULL 会让 `PRIMARY KEY (keyword, scope, owner_id)` 拦不住重复 ——
/// 而「同一作用域内关键词唯一」正是靠它保证的。
pub const SYSTEM_OWNER: &str = "";

/// 资源作用域。
///
/// 与消息的 [`crate::Scope`] 名字相近但语义无关，所以刻意分开命名。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceScope {
    /// 某个群自建，只在该群可见。
    Group,
    /// 全局可见。
    System,
}

impl ResourceScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Group => "group",
            Self::System => "system",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "group" => Some(Self::Group),
            "system" => Some(Self::System),
            _ => None,
        }
    }
}

fn parse_scope(raw: &str) -> Result<ResourceScope> {
    ResourceScope::parse(raw).with_context(|| format!("未知的资源作用域 {raw:?}"))
}

/// 一条资源的元数据（不含文件内容）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    pub id: i64,
    pub scope: ResourceScope,
    pub owner_id: String,
    pub name: String,
    pub path: PathBuf,
    pub file_name: String,
    /// 官方富媒体的 FileType：1 图片 / 2 视频 / 3 语音 / 4 文件。
    pub file_type: u8,
    pub description: Option<String>,
}

/// 关键词索引项。启动时整表读出来建内存索引。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeywordEntry {
    pub keyword: String,
    pub scope: ResourceScope,
    pub owner_id: String,
    pub resource_id: i64,
}

/// 新增 / 覆盖资源时的输入。
#[derive(Debug, Clone)]
pub struct ResourceSpec {
    pub scope: ResourceScope,
    pub owner_id: String,
    pub name: String,
    pub path: PathBuf,
    pub file_name: String,
    pub file_type: u8,
    pub description: Option<String>,
}

const RESOURCE_COLUMNS: &str =
    "id, scope, owner_id, name, path, file_name, file_type, description";

/// 一条 B 站订阅。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BiliSubscription {
    /// UP 主的 UID。
    pub uid: String,
    /// UP 主昵称（订阅时抓的快照）。
    pub name: String,
    pub created_at: i64,
}

/// 一发弹药。
///
/// 字段对齐 cq-bot 的 `bullet` 表（MIT），但只保留静态 JSON 里真实存在、
/// 且玩家真正会看的那几项。
#[derive(Debug, Clone, PartialEq)]
pub struct Ammo {
    /// 游戏内物品 id。
    pub id: String,
    /// 可读 slug，如 `556x45mm-m855`。检索与显示都用它。
    pub normalized_name: String,
    /// 中文名，来自 `regular/items_zh` 的 `<id> Name`。解析不到时为 `None`。
    pub name_zh: Option<String>,
    pub caliber: String,
    pub damage: i64,
    pub penetration_power: i64,
    pub armor_damage: i64,
    /// 0.0 ~ 1.0 的比例。
    pub fragmentation_chance: f64,
    pub initial_speed: i64,
    pub projectile_count: i64,
    pub tracer: bool,
    pub base_price: i64,
}

/// 一个塔科夫任务。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TarkovTask {
    pub id: String,
    /// 可读 slug，如 `gunsmith-part-1`。
    pub normalized_name: String,
    /// 中文名，来自 `regular/tasks_zh` 语言包。实测 515/515 命中。
    ///
    /// 类型上仍可空：列要兼容旧库，导入器则**强制非空**（缺了就不写库）。
    pub name_zh: Option<String>,
    /// 英文名，由 `normalizedName` 反推（`the-punisher-part-1` → `The Punisher Part 1`）。
    pub name_en: Option<String>,
    /// 商人的可读 slug，如 `prapor`。
    pub trader: String,
    /// 商人的中文名与头像。
    pub trader_name_zh: Option<String>,
    pub trader_image: String,
    /// 任务图标。
    pub task_image: String,
    /// 任务发生的地图 slug，空串表示不限地图。
    pub map: String,
    /// 地图中文名，来自 `regular/maps_zh`。
    pub map_name_zh: Option<String>,
    pub min_level: i64,
    pub is_kappa: bool,
    pub is_lightkeeper: bool,
    pub experience: i64,
    pub wiki_link: String,
    /// 阵营（`USEC` / `BEAR`）。上游的 `Any` 在导入时就被过滤掉，空串表示不限。
    pub faction: Option<String>,
    /// 是否可重复接取。
    pub restartable: bool,
    /// 本次导入的 unix 秒。
    pub updated_at: i64,
}

/// 任务目标。
///
/// 这里**故意没有** `count` / `is_optional` / `is_raid` / `time` 这些字段：
/// 它们在导入时就已经折进 `display_text` 与 `marks` 了。
/// 再存一份原始字段看着无害，实际是「两处真相」——
/// 渲染层早晚会挑一处读，于是「×1 要不要显示」这种判断要在两个地方各写一遍。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TarkovObjective {
    /// 上游的 `type`：`visit` / `shoot` / `giveItem` / `findItem` …
    ///
    /// **兜底的关键** —— 它是个稳定的枚举，万一新目标还没进语言包，
    /// 导入时靠它拼出「击杀」这类通用文案。渲染已经不用它，留着是排障。
    pub objective_type: String,
    /// 静态 JSON 里的 `description`。实测它就是**目标自己的 id**，
    /// 一个翻译键 —— 直接显示给用户等于显示一串 hex。
    pub description_key: String,
    /// **导入时拼好的正文**：「在海关使用 AKS-74U 消灭 Scav ×25」。
    ///
    /// 数量并进正文、×1 省掉、语言包缺失时退回通用文案 —— 都在导入时做完一次。
    /// 渲染层拿到什么印什么。
    pub display_text: String,
    /// **导入时拼好的附加标记**（可选 / 战局内 / 时间窗口），`|` 分隔，空串表示没有。
    ///
    /// 存 `|` 而不是 ` · `：标记怎么呈现是排版的事，由渲染层按版式决定 ——
    /// 目标少时走块引用，目标多时整段塞进代码块、标记只能写在行内。
    pub marks: String,
}

/// 任务奖励的一行。
#[derive(Debug, Clone, PartialEq)]
pub struct TarkovReward {
    /// `standing`（商人声望）/ `item`（物品）/ `skill`（技能）/ `unlock`（解锁）。
    pub kind: String,
    /// 商人或物品的上游 id。
    pub ref_id: String,
    pub name_zh: Option<String>,
    /// 声望是小数（0.1），物品是整数，用 f64 通吃。
    pub amount: f64,
    /// 结构化补充的 JSON：工艺解锁的 `{station, level}` 等。
    pub extra: Option<String>,
}

/// 前置任务的一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TarkovPrereq {
    pub prereq_id: String,
    /// 上游给的是数组（如 `["complete"]`），这里拍平成逗号分隔。
    pub status: String,
}

/// 后续任务的一行。
///
/// 上游没有这个方向的数据，由 `taskRequirements` 反转得到。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TarkovSuccessor {
    pub successor_id: String,
    /// 与前置一样是 `complete` / `active` / `failed`，逗号分隔。
    pub status: String,
}

/// 任务需要的一组钥匙：一张地图 + 这张地图上要的钥匙。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TarkovTaskKey {
    /// 地图中文名，空串表示上游没给地图。
    pub map_name: String,
    /// 该地图上的钥匙名，`、` 连接 —— 导入时已用语言包解析。
    pub keys: String,
}

/// 失败条件的一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TarkovTaskFail {
    /// 会把本任务判失败的任务名（导入时已解析成中文）。
    pub task_name: String,
    /// `complete` / `active` / `failed`，逗号分隔。
    pub status: String,
}

/// 一个任务的完整内容：任务本身 + 六张子表。
#[derive(Debug, Clone, PartialEq)]
pub struct TarkovTaskDetail {
    pub task: TarkovTask,
    pub objectives: Vec<TarkovObjective>,
    pub rewards: Vec<TarkovReward>,
    pub prereqs: Vec<TarkovPrereq>,
    pub successors: Vec<TarkovSuccessor>,
    /// 需要钥匙，一行一张地图。
    pub keys: Vec<TarkovTaskKey>,
    /// 商人的接取门槛，每行一句人话（导入时拼好，渲染层只管加 `- `）。
    pub requirements: Vec<String>,
    /// 失败条件。
    pub fails: Vec<TarkovTaskFail>,
}

/// 一件塔科夫物品（含跳蚤价格）。
#[derive(Debug, Clone, PartialEq)]
pub struct TarkovItem {
    pub id: String,
    /// 可读 slug，如 `colt-m4a1-556x45-assault-rifle`。
    pub normalized_name: String,
    /// 中文名，来自 `regular/items_zh`。
    pub name_zh: Option<String>,
    /// 商人基础价。
    pub base_price: i64,
    /// 跳蚤市场价。`None` 表示这件物品没有跳蚤数据（实测 5442 件里只有 3525 件有）。
    pub last_low_price: Option<i64>,
    pub avg24h_price: Option<i64>,
    pub low24h_price: Option<i64>,
    pub high24h_price: Option<i64>,
    pub weight: f64,
}

/// 拼出「按 slug 片段检索」的 SQL 与参数。
///
/// 弹药 / 任务 / 物品三个检索方法本来各抄了一遍同样的拼装逻辑，
/// 连 `for i in 0..tokens.len()` 都一模一样。抽出来之后，
/// 加一个新的可检索表只需要写它自己的列映射。
///
/// `select` 必须是**本文件里的常量**，绝不能来自外部输入 ——
/// 它会被直接拼进 SQL。
///
/// `columns` 里多个列之间是 **OR** —— 任务既有英文 slug 又有中文名，
/// 用户打哪个都该查到。多个 token 之间仍然是 AND。
///
/// 同一个 token 的多个列**复用同一个占位符**，所以参数个数还是每个 token 一个。
fn like_query(
    select: &str,
    columns: &[&str],
    tokens: &[String],
    limit: usize,
) -> (String, Vec<rusqlite::types::Value>) {
    let mut sql = format!("{select} WHERE 1 = 1");
    for (i, _) in tokens.iter().enumerate() {
        let clause: Vec<String> = columns
            .iter()
            .map(|c| format!("{c} LIKE ?{}", i + 1))
            .collect();
        let _ = write!(sql, " AND ({})", clause.join(" OR "));
    }
    let _ = write!(sql, " ORDER BY normalized_name LIMIT ?{}", tokens.len() + 1);

    let mut params: Vec<rusqlite::types::Value> = tokens
        .iter()
        .map(|t| rusqlite::types::Value::Text(format!("%{t}%")))
        .collect();
    params.push(rusqlite::types::Value::Integer(limit as i64));
    (sql, params)
}

const SELECT_AMMO: &str = "SELECT id, normalized_name, name_zh, caliber, damage, \
     penetration_power, armor_damage, fragmentation_chance, initial_speed, projectile_count, \
     tracer, base_price FROM ammo";

const SELECT_TASKS: &str = "SELECT id, normalized_name, name_zh, name_en, trader, trader_name_zh, \
     trader_image, task_image, map, map_name_zh, min_level, is_kappa, is_lightkeeper, experience, \
     wiki_link, faction, restartable, updated_at FROM tarkov_task";

const SELECT_ITEMS: &str = "SELECT id, normalized_name, name_zh, base_price, last_low_price, \
     avg24h_price, low24h_price, high24h_price, weight FROM tarkov_item";

/// 从一行读出物品。三个查询共用同一段列映射，列顺序必须与 SQL 里一致。
fn read_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<TarkovItem> {
    Ok(TarkovItem {
        id: row.get(0)?,
        normalized_name: row.get(1)?,
        name_zh: row.get(2)?,
        base_price: row.get(3)?,
        last_low_price: row.get(4)?,
        avg24h_price: row.get(5)?,
        low24h_price: row.get(6)?,
        high24h_price: row.get(7)?,
        weight: row.get(8)?,
    })
}

/// 资源库句柄。
#[derive(Clone)]
pub struct ResourceStore {
    conn: Arc<Mutex<Connection>>,
}

impl ResourceStore {
    /// 用一条已建好表的连接构造。
    pub fn new(conn: Connection) -> Self {
        Self { conn: Arc::new(Mutex::new(conn)) }
    }

    /// 在阻塞线程池上跑一次数据库操作。
    ///
    /// 资源操作全部由管理命令触发，频率是人手级别，所以不做连接池，
    /// 一条连接加锁就够。消息热路径走的是 `MessageStore` 的批量写线程，
    /// 与这里互不影响。
    async fn with<T, F>(&self, op: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let guard = conn.lock().map_err(|_| anyhow!("资源库连接已中毒"))?;
            op(&guard)
        })
        .await
        .context("资源库操作任务 panic")?
    }

    /// 读出全部关键词，供启动时建内存索引。
    pub async fn keywords(&self) -> Result<Vec<KeywordEntry>> {
        self.with(|conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT keyword, scope, owner_id, resource_id FROM resource_keywords",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (keyword, scope, owner_id, resource_id) = row?;
                // 作用域受 CHECK 约束，正常到不了这里；真到了就跳过并告警，
                // 不要让一行坏数据把整个索引拖垮。
                let Some(scope) = ResourceScope::parse(&scope) else {
                    tracing::warn!(scope, keyword, "资源作用域无法识别，已跳过该关键词");
                    continue;
                };
                out.push(KeywordEntry { keyword, scope, owner_id, resource_id });
            }
            Ok(out)
        })
        .await
    }

    /// 按 id 取资源。
    pub async fn get(&self, id: i64) -> Result<Option<Resource>> {
        self.with(move |conn| load(conn, id)).await
    }

    /// 列出某作用域下的全部资源（按名称排序）。
    pub async fn list(&self, scope: ResourceScope, owner_id: &str) -> Result<Vec<Resource>> {
        let owner = owner_id.to_string();
        self.with(move |conn| {
            let sql = format!(
                "SELECT {RESOURCE_COLUMNS} FROM resources \
                 WHERE scope = ?1 AND owner_id = ?2 ORDER BY name"
            );
            let mut stmt = conn.prepare_cached(&sql)?;
            let rows = stmt.query_map(rusqlite::params![scope.as_str(), owner], read_row)?;
            let mut out = Vec::new();
            for row in rows {
                // 双层 Result：外层是 rusqlite，内层是作用域解析。
                out.push(row??);
            }
            Ok(out)
        })
        .await
    }

    /// 新增或覆盖一条资源，并把 `name` 登记为主关键词。
    ///
    /// 两步在**同一个事务**里 —— 否则中途失败会留下一个没有任何触发词的孤儿资源。
    pub async fn upsert(&self, spec: ResourceSpec) -> Result<i64> {
        self.with(move |conn| {
            let tx = conn.unchecked_transaction()?;
            let now = now_unix();
            tx.execute(
                "INSERT INTO resources(scope, owner_id, name, path, file_name, file_type, description, created_at, updated_at) \
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8) \
                 ON CONFLICT(scope, owner_id, name) DO UPDATE SET \
                   path = excluded.path, \
                   file_name = excluded.file_name, \
                   file_type = excluded.file_type, \
                   description = excluded.description, \
                   updated_at = excluded.updated_at",
                rusqlite::params![
                    spec.scope.as_str(),
                    spec.owner_id,
                    spec.name,
                    spec.path.to_string_lossy(),
                    spec.file_name,
                    spec.file_type,
                    spec.description,
                    now,
                ],
            )?;

            let id: i64 = tx.query_row(
                "SELECT id FROM resources WHERE scope = ?1 AND owner_id = ?2 AND name = ?3",
                rusqlite::params![spec.scope.as_str(), spec.owner_id, spec.name],
                |r| r.get(0),
            )?;

            // 主名也登记成关键词，让「同一作用域内关键词唯一」这条约束覆盖它。
            tx.execute(
                "INSERT INTO resource_keywords(keyword, scope, owner_id, resource_id) \
                 VALUES(?1, ?2, ?3, ?4) \
                 ON CONFLICT(keyword, scope, owner_id) DO UPDATE SET resource_id = excluded.resource_id",
                rusqlite::params![spec.name, spec.scope.as_str(), spec.owner_id, id],
            )?;
            tx.commit()?;
            Ok(id)
        })
        .await
    }

    /// 给已有资源加一个触发词。
    ///
    /// 关键词在该作用域内已被占用时返回错误 —— 这正是我们要的：
    /// 与其让两个资源抢一个词、行为取决于查询顺序，不如直接拒绝。
    pub async fn add_keyword(
        &self,
        keyword: &str,
        scope: ResourceScope,
        owner_id: &str,
        resource_id: i64,
    ) -> Result<()> {
        let (keyword, owner) = (keyword.to_string(), owner_id.to_string());
        self.with(move |conn| {
            conn.execute(
                "INSERT INTO resource_keywords(keyword, scope, owner_id, resource_id) VALUES(?1, ?2, ?3, ?4)",
                rusqlite::params![keyword, scope.as_str(), owner, resource_id],
            )
            .with_context(|| format!("关键词 {keyword:?} 已被占用"))?;
            Ok(())
        })
        .await
    }

    /// 删除一个关键词，返回是否真的删掉了。
    pub async fn remove_keyword(
        &self,
        keyword: &str,
        scope: ResourceScope,
        owner_id: &str,
    ) -> Result<bool> {
        let (keyword, owner) = (keyword.to_string(), owner_id.to_string());
        self.with(move |conn| {
            let n = conn.execute(
                "DELETE FROM resource_keywords WHERE keyword = ?1 AND scope = ?2 AND owner_id = ?3",
                rusqlite::params![keyword, scope.as_str(), owner],
            )?;
            Ok(n > 0)
        })
        .await
    }

    /// 删除资源及其全部关键词（关键词靠外键级联）。
    pub async fn delete(
        &self,
        scope: ResourceScope,
        owner_id: &str,
        name: &str,
    ) -> Result<bool> {
        let (owner, name) = (owner_id.to_string(), name.to_string());
        self.with(move |conn| {
            let n = conn.execute(
                "DELETE FROM resources WHERE scope = ?1 AND owner_id = ?2 AND name = ?3",
                rusqlite::params![scope.as_str(), owner, name],
            )?;
            Ok(n > 0)
        })
        .await
    }

    // ---- B 站订阅 ----
    //
    // 放在这个 store 而不是另开一条连接：订阅和 `settings` 一样属于
    // 「业务配置」，同一个 SQLite 文件再开第三条连接没有收益。

    /// 新增或更新一条订阅。返回**是否新增**（已存在时只刷新昵称）。
    ///
    /// 重复点订阅是很自然的操作，不该报错。
    pub async fn bili_subscribe(&self, uid: &str, group_id: &str, name: &str) -> Result<bool> {
        let uid = uid.to_string();
        let group_id = group_id.to_string();
        let name = name.to_string();
        self.with(move |conn| {
            let existed = conn
                .query_row(
                    "SELECT 1 FROM bili_subscriptions WHERE uid = ?1 AND group_id = ?2",
                    rusqlite::params![uid, group_id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            conn.execute(
                "INSERT INTO bili_subscriptions(uid, group_id, name, created_at) \
                 VALUES(?1, ?2, ?3, ?4) \
                 ON CONFLICT(uid, group_id) DO UPDATE SET name = excluded.name",
                rusqlite::params![uid, group_id, name, now_unix()],
            )?;
            Ok(!existed)
        })
        .await
    }

    /// 删除一条订阅。返回**是否确实删掉了**（用来区分「退订成功」与「本来就没订」）。
    pub async fn bili_unsubscribe(&self, uid: &str, group_id: &str) -> Result<bool> {
        let uid = uid.to_string();
        let group_id = group_id.to_string();
        self.with(move |conn| {
            let removed = conn.execute(
                "DELETE FROM bili_subscriptions WHERE uid = ?1 AND group_id = ?2",
                rusqlite::params![uid, group_id],
            )?;
            Ok(removed > 0)
        })
        .await
    }

    /// 某个群的订阅列表，按订阅时间排序。
    pub async fn bili_subscriptions(&self, group_id: &str) -> Result<Vec<BiliSubscription>> {
        let group_id = group_id.to_string();
        self.with(move |conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT uid, name, created_at FROM bili_subscriptions \
                 WHERE group_id = ?1 ORDER BY created_at, uid",
            )?;
            let rows = stmt.query_map(rusqlite::params![group_id], |r| {
                Ok(BiliSubscription { uid: r.get(0)?, name: r.get(1)?, created_at: r.get(2)? })
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await
    }

    /// 全部订阅（供将来的推送任务遍历）。
    pub async fn all_bili_subscriptions(&self) -> Result<Vec<(String, BiliSubscription)>> {
        self.with(move |conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT group_id, uid, name, created_at FROM bili_subscriptions \
                 ORDER BY group_id, created_at, uid",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    BiliSubscription { uid: r.get(1)?, name: r.get(2)?, created_at: r.get(3)? },
                ))
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await
    }

    // ---- 塔科夫物品与跳蚤价格 ----

    /// 整表替换物品数据。返回写入行数。理由同 `replace_ammo`。
    pub async fn replace_items(&self, items: Vec<TarkovItem>) -> Result<usize> {
        self.with(move |conn| {
            let tx = conn.unchecked_transaction()?;
            tx.execute("DELETE FROM tarkov_item", [])?;
            {
                let mut stmt = tx.prepare_cached(
                    "INSERT OR REPLACE INTO tarkov_item(id, normalized_name, name_zh, base_price, \
                     last_low_price, avg24h_price, low24h_price, high24h_price, weight) \
                     VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                )?;
                for it in &items {
                    stmt.execute(rusqlite::params![
                        it.id,
                        it.normalized_name,
                        it.name_zh,
                        it.base_price,
                        it.last_low_price,
                        it.avg24h_price,
                        it.low24h_price,
                        it.high24h_price,
                        it.weight,
                    ])?;
                }
            }
            tx.commit()?;
            Ok(items.len())
        })
        .await
    }

    /// 按关键词检索物品。`tokens` 的语义同 `search_ammo`。
    pub async fn search_items(
        &self,
        tokens: Vec<String>,
        limit: usize,
    ) -> Result<Vec<TarkovItem>> {
        self.with(move |conn| {
            let (sql, params) =
                like_query(SELECT_ITEMS, &["normalized_name", "name_zh"], &tokens, limit);
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(params), read_item)?;
            // 必须先 collect 成 `rusqlite::Result` 再 `?`：
            // `FromIterator<Result<T, E>>` 要求 E 与目标**完全一致**，
            // 不会替我们把 rusqlite::Error 转成 anyhow::Error。
            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
        })
        .await
    }

    /// 按游戏内 id 精确取一件物品。
    pub async fn get_item(&self, id: &str) -> Result<Option<TarkovItem>> {
        let id = id.to_string();
        self.with(move |conn| {
            let found = conn
                .query_row(
                    &format!("{SELECT_ITEMS} WHERE id = ?1"),
                    rusqlite::params![id],
                    read_item,
                )
                .optional()?;
            Ok(found)
        })
        .await
    }

    /// 物品表当前行数。为 0 表示还没导入过。
    pub async fn item_count(&self) -> Result<i64> {
        self.with(|conn| {
            let n: i64 = conn.query_row("SELECT COUNT(*) FROM tarkov_item", [], |r| r.get(0))?;
            Ok(n)
        })
        .await
    }

    // ---- 塔科夫任务 ----

    /// 从数据库行读一个任务。列顺序必须与 `SELECT_TASKS` 一致。
    fn read_task(r: &rusqlite::Row<'_>) -> rusqlite::Result<TarkovTask> {
        Ok(TarkovTask {
            id: r.get(0)?,
            normalized_name: r.get(1)?,
            name_zh: r.get(2)?,
            name_en: r.get(3)?,
            trader: r.get(4)?,
            trader_name_zh: r.get(5)?,
            trader_image: r.get(6)?,
            task_image: r.get(7)?,
            map: r.get(8)?,
            map_name_zh: r.get(9)?,
            min_level: r.get(10)?,
            is_kappa: r.get(11)?,
            is_lightkeeper: r.get(12)?,
            experience: r.get(13)?,
            wiki_link: r.get(14)?,
            faction: r.get(15)?,
            restartable: r.get(16)?,
            updated_at: r.get(17)?,
        })
    }

    /// 整表替换任务数据（含六张子表）。返回**任务**条数。理由同 `replace_ammo`。
    ///
    /// 九张表在**同一个事务**里替换：中途失败就整体回滚，
    /// 不会留下「任务在、目标没了」这种半截状态。
    pub async fn replace_tasks(&self, items: Vec<TarkovTaskDetail>) -> Result<usize> {
        self.with(move |conn| {
            let tx = conn.unchecked_transaction()?;
            // 先删子表再删主表：虽然这里没开外键约束，但顺序反了将来加约束就炸。
            for table in [
                "tarkov_task_objective",
                "tarkov_task_reward",
                "tarkov_task_prereq",
                "tarkov_task_successor",
                "tarkov_task_key",
                "tarkov_task_requirement",
                "tarkov_task_fail",
                "tarkov_task",
            ] {
                tx.execute(&format!("DELETE FROM {table}"), [])?;
            }
            {
                let mut task_stmt = tx.prepare_cached(
                    "INSERT OR REPLACE INTO tarkov_task(id, normalized_name, name_zh, name_en, \
                     trader, trader_name_zh, trader_image, task_image, map, map_name_zh, \
                     min_level, is_kappa, is_lightkeeper, experience, wiki_link, faction, \
                     restartable, updated_at) \
                     VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, \
                             ?16, ?17, ?18)",
                )?;
                let mut obj_stmt = tx.prepare_cached(
                    "INSERT OR REPLACE INTO tarkov_task_objective(task_id, ordinal, \
                     objective_type, description_key, display_text, marks) \
                     VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
                )?;
                let mut key_stmt = tx.prepare_cached(
                    "INSERT OR REPLACE INTO tarkov_task_key(task_id, ordinal, map_name, keys) \
                     VALUES(?1, ?2, ?3, ?4)",
                )?;
                let mut req_stmt = tx.prepare_cached(
                    "INSERT OR REPLACE INTO tarkov_task_requirement(task_id, ordinal, text) \
                     VALUES(?1, ?2, ?3)",
                )?;
                let mut fail_stmt = tx.prepare_cached(
                    "INSERT OR REPLACE INTO tarkov_task_fail(task_id, ordinal, task_name, status) \
                     VALUES(?1, ?2, ?3, ?4)",
                )?;
                let mut reward_stmt = tx.prepare_cached(
                    "INSERT OR REPLACE INTO tarkov_task_reward(task_id, ordinal, kind, ref_id, \
                     name_zh, amount, extra) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )?;
                let mut prereq_stmt = tx.prepare_cached(
                    "INSERT OR REPLACE INTO tarkov_task_prereq(task_id, ordinal, prereq_id, status) \
                     VALUES(?1, ?2, ?3, ?4)",
                )?;
                let mut succ_stmt = tx.prepare_cached(
                    "INSERT OR REPLACE INTO tarkov_task_successor(task_id, ordinal, successor_id, \
                     status) VALUES(?1, ?2, ?3, ?4)",
                )?;

                for detail in &items {
                    let t = &detail.task;
                    task_stmt.execute(rusqlite::params![
                        t.id,
                        t.normalized_name,
                        t.name_zh,
                        t.name_en,
                        t.trader,
                        t.trader_name_zh,
                        t.trader_image,
                        t.task_image,
                        t.map,
                        t.map_name_zh,
                        t.min_level,
                        t.is_kappa,
                        t.is_lightkeeper,
                        t.experience,
                        t.wiki_link,
                        t.faction,
                        t.restartable,
                        t.updated_at,
                    ])?;
                    for (i, o) in detail.objectives.iter().enumerate() {
                        obj_stmt.execute(rusqlite::params![
                            t.id,
                            i as i64,
                            o.objective_type,
                            o.description_key,
                            o.display_text,
                            o.marks,
                        ])?;
                    }
                    for (i, r) in detail.rewards.iter().enumerate() {
                        reward_stmt.execute(rusqlite::params![
                            t.id,
                            i as i64,
                            r.kind,
                            r.ref_id,
                            r.name_zh,
                            r.amount,
                            r.extra,
                        ])?;
                    }
                    for (i, p) in detail.prereqs.iter().enumerate() {
                        prereq_stmt.execute(rusqlite::params![t.id, i as i64, p.prereq_id, p.status])?;
                    }
                    for (i, s) in detail.successors.iter().enumerate() {
                        succ_stmt.execute(rusqlite::params![
                            t.id,
                            i as i64,
                            s.successor_id,
                            s.status,
                        ])?;
                    }
                    for (i, k) in detail.keys.iter().enumerate() {
                        key_stmt.execute(rusqlite::params![t.id, i as i64, k.map_name, k.keys])?;
                    }
                    for (i, text) in detail.requirements.iter().enumerate() {
                        req_stmt.execute(rusqlite::params![t.id, i as i64, text])?;
                    }
                    for (i, f) in detail.fails.iter().enumerate() {
                        fail_stmt.execute(rusqlite::params![t.id, i as i64, f.task_name, f.status])?;
                    }
                }
            }
            tx.commit()?;
            Ok(items.len())
        })
        .await
    }

    /// 按关键词检索任务。`tokens` 的语义同 `search_ammo`。
    ///
    /// 同时匹配 slug 与中文名 —— 上游有语言包时用户打中文，没有时打 slug。
    pub async fn search_tasks(
        &self,
        tokens: Vec<String>,
        limit: usize,
    ) -> Result<Vec<TarkovTask>> {
        self.with(move |conn| {
            let (sql, params) =
                like_query(SELECT_TASKS, &["normalized_name", "name_zh"], &tokens, limit);
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(params), Self::read_task)?;
            // 必须先 collect 成 `rusqlite::Result` 再 `?`：
            // `FromIterator<Result<T, E>>` 要求 E 与目标**完全一致**，
            // 不会替我们把 rusqlite::Error 转成 anyhow::Error。
            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
        })
        .await
    }

    /// 取一个任务的完整内容（任务 + 目标 + 奖励 + 前置/后续 + 钥匙/门槛/失败条件）。
    ///
    /// 找不到返回 `None` 而不是空壳 —— 「这个 id 不存在」与
    /// 「这个任务没有目标」是两回事。
    pub async fn task_detail(&self, id: String) -> Result<Option<TarkovTaskDetail>> {
        self.with(move |conn| {
            let sql = format!("{SELECT_TASKS} WHERE id = ?1");
            let task = conn
                .query_row(&sql, [&id], Self::read_task)
                .optional()?;
            let Some(task) = task else {
                return Ok(None);
            };

            let mut obj_stmt = conn.prepare_cached(
                "SELECT objective_type, description_key, display_text, marks \
                 FROM tarkov_task_objective WHERE task_id = ?1 ORDER BY ordinal",
            )?;
            let objectives = obj_stmt
                .query_map([&id], |r| {
                    Ok(TarkovObjective {
                        objective_type: r.get(0)?,
                        description_key: r.get(1)?,
                        display_text: r.get(2)?,
                        marks: r.get(3)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;

            let mut reward_stmt = conn.prepare_cached(
                "SELECT kind, ref_id, name_zh, amount, extra FROM tarkov_task_reward \
                 WHERE task_id = ?1 ORDER BY ordinal",
            )?;
            let rewards = reward_stmt
                .query_map([&id], |r| {
                    Ok(TarkovReward {
                        kind: r.get(0)?,
                        ref_id: r.get(1)?,
                        name_zh: r.get(2)?,
                        amount: r.get(3)?,
                        extra: r.get(4)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;

            let mut prereq_stmt = conn.prepare_cached(
                "SELECT prereq_id, status FROM tarkov_task_prereq \
                 WHERE task_id = ?1 ORDER BY ordinal",
            )?;
            let prereqs = prereq_stmt
                .query_map([&id], |r| {
                    Ok(TarkovPrereq {
                        prereq_id: r.get(0)?,
                        status: r.get(1)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;

            let mut succ_stmt = conn.prepare_cached(
                "SELECT successor_id, status FROM tarkov_task_successor \
                 WHERE task_id = ?1 ORDER BY ordinal",
            )?;
            let successors = succ_stmt
                .query_map([&id], |r| {
                    Ok(TarkovSuccessor {
                        successor_id: r.get(0)?,
                        status: r.get(1)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;

            let mut key_stmt = conn.prepare_cached(
                "SELECT map_name, keys FROM tarkov_task_key \
                 WHERE task_id = ?1 ORDER BY ordinal",
            )?;
            let keys = key_stmt
                .query_map([&id], |r| {
                    Ok(TarkovTaskKey {
                        map_name: r.get(0)?,
                        keys: r.get(1)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;

            let mut req_stmt = conn.prepare_cached(
                "SELECT text FROM tarkov_task_requirement \
                 WHERE task_id = ?1 ORDER BY ordinal",
            )?;
            let requirements = req_stmt
                .query_map([&id], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;

            let mut fail_stmt = conn.prepare_cached(
                "SELECT task_name, status FROM tarkov_task_fail \
                 WHERE task_id = ?1 ORDER BY ordinal",
            )?;
            let fails = fail_stmt
                .query_map([&id], |r| {
                    Ok(TarkovTaskFail {
                        task_name: r.get(0)?,
                        status: r.get(1)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;

            Ok(Some(TarkovTaskDetail {
                task,
                objectives,
                rewards,
                prereqs,
                successors,
                keys,
                requirements,
                fails,
            }))
        })
        .await
    }

    /// 按 id 批量取显示名（中文名优先，退回 slug）。
    ///
    /// 用来把前置任务 id 变成人看得懂的名字。查不到的 id 不出现在结果里，
    /// 调用方退回显示 id —— 至少能对上号。
    pub async fn task_names(&self, ids: Vec<String>) -> Result<std::collections::HashMap<String, String>> {
        if ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        self.with(move |conn| {
            // 占位符要按个数拼：SQLite 不接受把数组当参数传。
            let placeholders = (1..=ids.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "SELECT id, COALESCE(name_zh, normalized_name) FROM tarkov_task \
                 WHERE id IN ({placeholders})"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(ids.iter()), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            rows.collect::<rusqlite::Result<std::collections::HashMap<_, _>>>()
                .map_err(Into::into)
        })
        .await
    }

    /// 按中文名或 slug **精确**取任务 id。
    ///
    /// 卡片上的按钮发出去的是 `任务 <完整中文名>`，点进来时查询是完整名字 ——
    /// 走 LIKE 会退化成模糊匹配（`惩罚者 - 1` 也能命中 `惩罚者 - 10`），
    /// 所以先精确匹配一次，匹配不到再退回关键词检索。
    pub async fn task_id_by_name(&self, name: String) -> Result<Option<String>> {
        self.with(move |conn| {
            conn.query_row(
                "SELECT id FROM tarkov_task WHERE name_zh = ?1 OR normalized_name = ?1 LIMIT 1",
                [&name],
                |r| r.get(0),
            )
            .optional()
            .map_err(Into::into)
        })
        .await
    }

    /// 任务表当前行数。为 0 表示还没导入过。
    pub async fn task_count(&self) -> Result<i64> {
        self.with(|conn| {
            let n: i64 = conn.query_row("SELECT COUNT(*) FROM tarkov_task", [], |r| r.get(0))?;
            Ok(n)
        })
        .await
    }
    // ---- 塔科夫弹药 ----

    /// 整表替换弹药数据。返回写入行数。
    ///
    /// 用「先清空再写入」而不是增量合并：上游是一份完整快照，
    /// 增量合并会留下一批上游已经删掉的条目。整表替换在**同一个事务**里，
    /// 中途失败不会留下空表。
    pub async fn replace_ammo(&self, items: Vec<Ammo>) -> Result<usize> {
        self.with(move |conn| {
            let tx = conn.unchecked_transaction()?;
            tx.execute("DELETE FROM ammo", [])?;
            {
                let mut stmt = tx.prepare_cached(
                    "INSERT OR REPLACE INTO ammo(id, normalized_name, name_zh, caliber, damage, \
                     penetration_power, armor_damage, fragmentation_chance, initial_speed, \
                     projectile_count, tracer, base_price) \
                     VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                )?;
                for a in &items {
                    stmt.execute(rusqlite::params![
                        a.id,
                        a.normalized_name,
                        a.name_zh,
                        a.caliber,
                        a.damage,
                        a.penetration_power,
                        a.armor_damage,
                        a.fragmentation_chance,
                        a.initial_speed,
                        a.projectile_count,
                        a.tracer,
                        a.base_price,
                    ])?;
                }
            }
            tx.commit()?;
            Ok(items.len())
        })
        .await
    }

    /// 按关键词检索弹药。
    ///
    /// `tokens` 是**已经归一化**的片段（小写、去点、去空格），
    /// 要求全部命中 `normalized_name` —— 于是 `5.45 bp` 会变成
    /// `["545", "bp"]`，而 `545x39mm-bp` 两个都含。
    pub async fn search_ammo(&self, tokens: Vec<String>, limit: usize) -> Result<Vec<Ammo>> {
        self.with(move |conn| {
            let (sql, params) =
                like_query(SELECT_AMMO, &["normalized_name", "name_zh"], &tokens, limit);
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(params), |r| {
                Ok(Ammo {
                    id: r.get(0)?,
                    normalized_name: r.get(1)?,
                    name_zh: r.get(2)?,
                    caliber: r.get(3)?,
                    damage: r.get(4)?,
                    penetration_power: r.get(5)?,
                    armor_damage: r.get(6)?,
                    fragmentation_chance: r.get(7)?,
                    initial_speed: r.get(8)?,
                    projectile_count: r.get(9)?,
                    tracer: r.get(10)?,
                    base_price: r.get(11)?,
                })
            })?;
            // 必须先 collect 成 `rusqlite::Result` 再 `?`：
            // `FromIterator<Result<T, E>>` 要求 E 与目标**完全一致**，
            // 不会替我们把 rusqlite::Error 转成 anyhow::Error。
            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
        })
        .await
    }

    /// 弹药表当前行数。为 0 表示还没导入过。
    pub async fn ammo_count(&self) -> Result<i64> {
        self.with(|conn| {
            let n: i64 = conn.query_row("SELECT COUNT(*) FROM ammo", [], |r| r.get(0))?;
            Ok(n)
        })
        .await
    }

    // ---- 图语 ----

    /// 记下待用的图语。同一个人重复设置时覆盖上一条。
    pub async fn set_caption(&self, target_id: &str, sender_id: &str, text: &str) -> Result<()> {
        let target_id = target_id.to_string();
        let sender_id = sender_id.to_string();
        let text = text.to_string();
        self.with(move |conn| {
            conn.execute(
                "INSERT INTO pending_captions(target_id, sender_id, text, created_at) \
                 VALUES(?1, ?2, ?3, ?4) \
                 ON CONFLICT(target_id, sender_id) DO UPDATE SET \
                   text = excluded.text, created_at = excluded.created_at",
                rusqlite::params![target_id, sender_id, text, now_unix()],
            )?;
            Ok(())
        })
        .await
    }

    /// 取出并**删除**待用图语。
    ///
    /// 无论是否过期都会删掉：这是一次性状态，留着只会让下一张图莫名其妙被配字。
    /// 返回 `None` 表示没有，或已经超过 `ttl_secs`。
    pub async fn take_caption(
        &self,
        target_id: &str,
        sender_id: &str,
        ttl_secs: i64,
    ) -> Result<Option<String>> {
        let target_id = target_id.to_string();
        let sender_id = sender_id.to_string();
        self.with(move |conn| {
            let row: Option<(String, i64)> = conn
                .query_row(
                    "SELECT text, created_at FROM pending_captions \
                     WHERE target_id = ?1 AND sender_id = ?2",
                    rusqlite::params![target_id, sender_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            conn.execute(
                "DELETE FROM pending_captions WHERE target_id = ?1 AND sender_id = ?2",
                rusqlite::params![target_id, sender_id],
            )?;
            Ok(row.filter(|(_, at)| now_unix() - at < ttl_secs).map(|(text, _)| text))
        })
        .await
    }

    /// 清掉过期条目，供将来的清理任务使用。
    pub async fn purge_captions(&self, ttl_secs: i64) -> Result<usize> {
        self.with(move |conn| {
            let cutoff = now_unix() - ttl_secs;
            let removed = conn.execute(
                "DELETE FROM pending_captions WHERE created_at < ?1",
                rusqlite::params![cutoff],
            )?;
            Ok(removed)
        })
        .await
    }

    /// 读一条系统设置。
    pub async fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let key = key.to_string();
        self.with(move |conn| {
            conn.query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get(0))
                .optional()
                .context("读取系统设置失败")
        })
        .await
    }

    /// 写一条系统设置。
    pub async fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        let (key, value) = (key.to_string(), value.to_string());
        self.with(move |conn| {
            conn.execute(
                "INSERT INTO settings(key, value, updated_at) VALUES(?1, ?2, ?3) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
                rusqlite::params![key, value, now_unix()],
            )?;
            Ok(())
        })
        .await
    }
}

fn load(conn: &Connection, id: i64) -> Result<Option<Resource>> {
    let sql = format!("SELECT {RESOURCE_COLUMNS} FROM resources WHERE id = ?1");
    let row = conn
        .query_row(&sql, [id], read_row)
        .optional()
        .context("读取资源失败")?;
    row.transpose()
}

fn read_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Result<Resource>> {
    let scope: String = r.get(1)?;
    let file_type: i64 = r.get(6)?;
    Ok((|| {
        Ok(Resource {
            id: r.get(0)?,
            scope: parse_scope(&scope)?,
            owner_id: r.get(2)?,
            name: r.get(3)?,
            path: PathBuf::from(r.get::<_, String>(4)?),
            file_name: r.get(5)?,
            file_type: u8::try_from(file_type).context("file_type 超出 u8 范围")?,
            description: r.get(7)?,
        })
    })())
}

/// 首次到达 v2 时播种默认系统控制者。
///
/// 用 `DO NOTHING` 而不是 `DO UPDATE`：**只在设置不存在时写入**。
/// 否则控制者把列表清空后，下次启动默认值会复活，
/// 「删掉最后一个控制者」就成了一个无法完成的动作。
pub fn seed_defaults(conn: &Connection) -> Result<()> {
    let value = format!("[\"{DEFAULT_SYSTEM_CONTROLLER}\"]");
    conn.execute(
        "INSERT INTO settings(key, value, updated_at) VALUES(?1, ?2, ?3) ON CONFLICT(key) DO NOTHING",
        rusqlite::params![SYSTEM_CONTROLLERS_KEY, value, now_unix()],
    )
    .context("播种默认系统控制者失败")?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> ResourceStore {
        let conn = Connection::open_in_memory().unwrap();
        // migrate 只管建表，PRAGMA 在 schema::open 里设；内存库要自己补上，
        // 否则外键级联不生效，删除资源的测试会假绿。
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        crate::schema::migrate(&conn).unwrap();
        ResourceStore::new(conn)
    }

    fn spec(scope: ResourceScope, owner: &str, name: &str, path: &str) -> ResourceSpec {
        ResourceSpec {
            scope,
            owner_id: owner.to_string(),
            name: name.to_string(),
            path: PathBuf::from(path),
            file_name: "a.png".to_string(),
            file_type: 1,
            description: None,
        }
    }

    #[tokio::test]
    async fn subscribe_is_idempotent_and_refreshes_the_name() {
        let s = store();
        assert!(s.bili_subscribe("123", "G1", "旧名字").await.unwrap(), "首次订阅应当是新增");
        assert!(!s.bili_subscribe("123", "G1", "新名字").await.unwrap(), "重复订阅不该算新增");

        let list = s.bili_subscriptions("G1").await.unwrap();
        assert_eq!(list.len(), 1, "重复订阅不该产生第二行: {list:?}");
        assert_eq!(list[0].name, "新名字", "昵称应当被刷新");
    }

    #[tokio::test]
    async fn unsubscribe_reports_whether_anything_was_removed() {
        let s = store();
        s.bili_subscribe("123", "G1", "甲").await.unwrap();
        assert!(s.bili_unsubscribe("123", "G1").await.unwrap(), "删掉了一条应当返回 true");
        assert!(!s.bili_unsubscribe("123", "G1").await.unwrap(), "本来就没有应当返回 false");
        assert!(s.bili_subscriptions("G1").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn subscriptions_are_scoped_to_the_group() {
        let s = store();
        s.bili_subscribe("123", "G1", "甲").await.unwrap();
        s.bili_subscribe("123", "G2", "甲").await.unwrap();
        s.bili_subscribe("456", "G1", "乙").await.unwrap();

        // 同一个 UP 可以被多个群订阅；同一个群可以订阅多个 UP。
        assert_eq!(s.bili_subscriptions("G1").await.unwrap().len(), 2);
        assert_eq!(s.bili_subscriptions("G2").await.unwrap().len(), 1);
        assert!(s.bili_subscriptions("G3").await.unwrap().is_empty());

        // 退订只影响本群。
        s.bili_unsubscribe("123", "G1").await.unwrap();
        assert_eq!(s.bili_subscriptions("G1").await.unwrap().len(), 1);
        assert_eq!(s.bili_subscriptions("G2").await.unwrap().len(), 1, "别的群不该被影响");
    }

    #[tokio::test]
    async fn all_subscriptions_carry_the_group() {
        let s = store();
        s.bili_subscribe("123", "G1", "甲").await.unwrap();
        s.bili_subscribe("456", "G2", "乙").await.unwrap();
        let all = s.all_bili_subscriptions().await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].0, "G1", "应当按群排序，供推送任务按群聚合");
        assert_eq!(all[1].0, "G2");
    }

    #[tokio::test]
    async fn caption_round_trips_and_is_consumed_once() {
        let s = store();
        assert!(s.take_caption("G1", "U1", 300).await.unwrap().is_none(), "没设过就是 None");

        s.set_caption("G1", "U1", "你好").await.unwrap();
        assert_eq!(s.take_caption("G1", "U1", 300).await.unwrap().as_deref(), Some("你好"));
        assert!(
            s.take_caption("G1", "U1", 300).await.unwrap().is_none(),
            "取过一次之后就该没了"
        );
    }

    #[tokio::test]
    async fn caption_is_scoped_to_target_and_sender() {
        let s = store();
        s.set_caption("G1", "U1", "甲的话").await.unwrap();
        assert!(s.take_caption("G2", "U1", 300).await.unwrap().is_none(), "会话之间不能串");
        assert!(s.take_caption("G1", "U2", 300).await.unwrap().is_none(), "不能拿别人的");
        assert_eq!(s.take_caption("G1", "U1", 300).await.unwrap().as_deref(), Some("甲的话"));
    }

    #[tokio::test]
    async fn stale_caption_is_dropped_but_still_deleted() {
        let s = store();
        s.set_caption("G1", "U1", "过期了").await.unwrap();
        // ttl 为 0：写入时刻已经「过期」。
        assert!(s.take_caption("G1", "U1", 0).await.unwrap().is_none(), "过期的不该返回");
        assert!(
            s.take_caption("G1", "U1", 300).await.unwrap().is_none(),
            "过期的也要删掉，否则下一张图会莫名被配字"
        );
    }

    #[tokio::test]
    async fn purge_removes_only_stale_captions() {
        let s = store();
        s.set_caption("G1", "U1", "新的").await.unwrap();
        // cutoff = now - 300，刚写入的条目比它新，不该被清。
        assert_eq!(s.purge_captions(300).await.unwrap(), 0, "没过期的不该被清");
        assert_eq!(
            s.take_caption("G1", "U1", 300).await.unwrap().as_deref(),
            Some("新的"),
            "清理不该动到还能用的条目"
        );

        // 时间戳是秒级，所以要跨过一个整秒才能构造出「更旧」的条目。
        s.set_caption("G1", "U1", "旧的").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        assert_eq!(s.purge_captions(0).await.unwrap(), 1);
        assert!(s.take_caption("G1", "U1", 300).await.unwrap().is_none());
    }

    fn ammo(id: &str, slug: &str, pen: i64) -> Ammo {
        Ammo {
            id: id.into(),
            normalized_name: slug.into(),
            name_zh: None,
            caliber: "Caliber545x39".into(),
            damage: 50,
            penetration_power: pen,
            armor_damage: 40,
            fragmentation_chance: 0.16,
            initial_speed: 890,
            projectile_count: 1,
            tracer: false,
            base_price: 110,
        }
    }

    #[tokio::test]
    async fn ammo_replace_is_a_full_snapshot() {
        let s = store();
        assert_eq!(s.ammo_count().await.unwrap(), 0, "一开始是空的");

        s.replace_ammo(vec![ammo("a1", "545x39mm-bp", 37), ammo("a2", "556x45mm-m855", 31)])
            .await
            .unwrap();
        assert_eq!(s.ammo_count().await.unwrap(), 2);

        // 第二次导入只有一条：旧的两条必须消失，不能留下上游已删的条目。
        s.replace_ammo(vec![ammo("a3", "762x39mm-ps", 26)]).await.unwrap();
        assert_eq!(s.ammo_count().await.unwrap(), 1);
        let all = s.search_ammo(vec![], 10).await.unwrap();
        assert_eq!(all[0].normalized_name, "762x39mm-ps");
    }

    #[tokio::test]
    async fn ammo_search_requires_every_token() {
        let s = store();
        s.replace_ammo(vec![
            ammo("a1", "545x39mm-bp", 37),
            ammo("a2", "545x39mm-ps", 20),
            ammo("a3", "556x45mm-m855", 31),
        ])
        .await
        .unwrap();

        // 单片段命中两条
        assert_eq!(s.search_ammo(vec!["545".into()], 10).await.unwrap().len(), 2);
        // 加一个片段就精确到一条 —— 这正是「全部命中」的意义
        let bp = s.search_ammo(vec!["545".into(), "bp".into()], 10).await.unwrap();
        assert_eq!(bp.len(), 1, "{bp:?}");
        assert_eq!(bp[0].normalized_name, "545x39mm-bp");
        assert_eq!(bp[0].penetration_power, 37, "数值字段要完整取回");

        // 查不到就是空，不是全表
        assert!(s.search_ammo(vec!["不存在".into()], 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn ammo_search_honours_the_limit() {
        let s = store();
        let items: Vec<Ammo> = (0..20).map(|i| ammo(&format!("a{i}"), &format!("545x39mm-{i:02}"), i)).collect();
        s.replace_ammo(items).await.unwrap();
        assert_eq!(s.search_ammo(vec!["545".into()], 5).await.unwrap().len(), 5);
    }

    fn task(id: &str, slug: &str, trader: &str, level: i64) -> TarkovTaskDetail {
        TarkovTaskDetail {
            task: TarkovTask {
                id: id.into(),
                normalized_name: slug.into(),
                name_zh: Some(format!("{slug} 中文")),
                name_en: None,
                trader: trader.into(),
                trader_name_zh: None,
                trader_image: format!("https://img/{trader}.png"),
                task_image: format!("https://img/{slug}.png"),
                map: "customs".into(),
                map_name_zh: None,
                min_level: level,
                is_kappa: level > 40,
                is_lightkeeper: false,
                experience: 1000,
                wiki_link: "https://x".into(),
                faction: None,
                restartable: false,
                updated_at: 0,
            },
            objectives: vec![TarkovObjective {
                objective_type: "shoot".into(),
                description_key: "6575a64d3fc09bdfb38b713d".into(),
                display_text: "击杀 5 个 Scav ×5".into(),
                marks: String::new(),
            }],
            rewards: vec![TarkovReward {
                kind: "standing".into(),
                ref_id: "5a7c2eca46aef81a7ca2145d".into(),
                name_zh: None,
                amount: 0.1,
                extra: None,
            }],
            prereqs: vec![TarkovPrereq {
                prereq_id: "p1".into(),
                status: "complete".into(),
            }],
            successors: vec![],
            keys: vec![],
            requirements: vec![],
            fails: vec![],
        }
    }

    #[tokio::test]
    async fn task_replace_and_search() {
        let s = store();
        assert_eq!(s.task_count().await.unwrap(), 0);

        s.replace_tasks(vec![
            task("k1", "gunsmith-part-1", "mechanic", 5),
            task("k2", "gunsmith-part-2", "mechanic", 10),
            task("k3", "first-in-line", "prapor", 1),
        ])
        .await
        .unwrap();
        assert_eq!(s.task_count().await.unwrap(), 3);

        assert_eq!(s.search_tasks(vec!["gunsmith".into()], 10).await.unwrap().len(), 2);
        let one = s.search_tasks(vec!["gunsmith".into(), "part-2".into()], 10).await.unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].normalized_name, "gunsmith-part-2");
        assert_eq!(one[0].trader, "mechanic");
        assert_eq!(one[0].min_level, 10);

        // 整表替换：第二次只剩一条。
        s.replace_tasks(vec![task("k9", "only-one", "prapor", 1)]).await.unwrap();
        assert_eq!(s.task_count().await.unwrap(), 1);
    }

    fn item(id: &str, slug: &str, avg: Option<i64>) -> TarkovItem {
        TarkovItem {
            id: id.into(),
            normalized_name: slug.into(),
            name_zh: None,
            base_price: 1000,
            last_low_price: avg.map(|v| v - 10),
            avg24h_price: avg,
            low24h_price: avg.map(|v| v - 100),
            high24h_price: avg.map(|v| v + 100),
            weight: 1.5,
        }
    }

    #[tokio::test]
    async fn item_replace_search_and_get_by_id() {
        let s = store();
        assert_eq!(s.item_count().await.unwrap(), 0);

        s.replace_items(vec![
            item("5447a9cd4bdc2dbd208b4567", "colt-m4a1-556x45-assault-rifle", Some(93642)),
            item("5c0e53c886f7744a13f54933", "slick-body-armor", None),
        ])
        .await
        .unwrap();
        assert_eq!(s.item_count().await.unwrap(), 2);

        let found = s.search_items(vec!["m4a1".into()], 10).await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].avg24h_price, Some(93642), "数值字段要完整取回");

        // 按 id 精确取：这是 B3 详情视图走的路。
        let by_id = s.get_item("5447a9cd4bdc2dbd208b4567").await.unwrap();
        assert_eq!(by_id.unwrap().normalized_name, "colt-m4a1-556x45-assault-rifle");
        assert!(s.get_item("不存在的id").await.unwrap().is_none());

        // 缺价要真的存成 NULL，不能变成 0。
        let slick = s.search_items(vec!["slick".into()], 10).await.unwrap();
        assert_eq!(slick[0].avg24h_price, None);
        assert_eq!(slick[0].base_price, 1000, "商人价仍在");
    }

    /// 子表要跟着主表一起进出，且顺序必须保住。
    #[tokio::test]
    async fn task_detail_roundtrips_children_in_order() {
        let s = store();
        let mut detail = task("k1", "first-in-line", "prapor", 1);
        // 顺序是渲染的序号来源，插进去乱序取出来必须还是原序。
        detail.objectives = vec![
            TarkovObjective {
                objective_type: "visit".into(),
                description_key: "o1".into(),
                display_text: "去海关看看".into(),
                marks: "可选".into(),
            },
            TarkovObjective {
                objective_type: "shoot".into(),
                description_key: "o2".into(),
                display_text: "击杀 3 个 Scav ×3".into(),
                marks: "战局内".into(),
            },
        ];
        // v12 新增的三张子表也要能进出。
        detail.keys = vec![TarkovTaskKey {
            map_name: "海关".into(),
            keys: "宿舍206房间钥匙".into(),
        }];
        detail.requirements = vec!["大老板 忠诚等级 >= 2".into()];
        detail.fails = vec![TarkovTaskFail {
            task_name: "第三只眼".into(),
            status: "complete".into(),
        }];
        // 后续任务也要能进出：它是导入时反转前置得到的。
        detail.successors = vec![TarkovSuccessor {
            successor_id: "k9".into(),
            status: "complete".into(),
        }];
        s.replace_tasks(vec![detail]).await.unwrap();

        let got = s.task_detail("k1".into()).await.unwrap().expect("应当能取回");
        assert_eq!(got.task.normalized_name, "first-in-line");
        assert_eq!(got.task.trader_image, "https://img/prapor.png");
        assert_eq!(got.objectives.len(), 2);
        assert_eq!(got.objectives[0].objective_type, "visit", "顺序要保住");
        assert_eq!(got.objectives[0].display_text, "去海关看看");
        assert_eq!(got.objectives[0].marks, "可选");
        assert_eq!(got.objectives[1].display_text, "击杀 3 个 Scav ×3");
        assert_eq!(got.objectives[1].marks, "战局内");
        assert_eq!(got.rewards.len(), 1);
        assert!((got.rewards[0].amount - 0.1).abs() < 1e-9, "声望是小数");
        assert_eq!(got.prereqs[0].prereq_id, "p1");
        assert_eq!(got.successors.len(), 1);
        assert_eq!(got.successors[0].successor_id, "k9");
        assert_eq!(got.keys.len(), 1, "钥匙要跟着任务一起回来");
        assert_eq!(got.keys[0].map_name, "海关");
        assert_eq!(got.keys[0].keys, "宿舍206房间钥匙");
        assert_eq!(got.requirements, vec!["大老板 忠诚等级 >= 2".to_string()]);
        assert_eq!(got.fails[0].task_name, "第三只眼");
        assert_eq!(got.fails[0].status, "complete");

        // 不存在的 id 是 None，而不是空壳。
        assert!(s.task_detail("nope".into()).await.unwrap().is_none());
    }

    /// 整表替换要连子表一起清掉 —— 否则删掉的任务会留下孤儿目标。
    #[tokio::test]
    async fn replace_tasks_clears_children_too() {
        let s = store();
        s.replace_tasks(vec![task("k1", "a", "prapor", 1)]).await.unwrap();
        assert_eq!(s.task_detail("k1".into()).await.unwrap().unwrap().objectives.len(), 1);

        s.replace_tasks(vec![task("k2", "b", "prapor", 1)]).await.unwrap();
        assert!(s.task_detail("k1".into()).await.unwrap().is_none(), "旧任务要没了");
        assert_eq!(s.task_detail("k2".into()).await.unwrap().unwrap().objectives.len(), 1);
    }

    /// 中文名与英文 slug 都要能查到 —— 用户打哪个取决于上游有没有语言包。
    #[tokio::test]
    async fn task_search_matches_both_slug_and_chinese_name() {
        let s = store();
        let mut a = task("k1", "first-in-line", "prapor", 1);
        a.task.name_zh = Some("彻夜难眠".into());
        let b = task("k2", "gunsmith-part-1", "mechanic", 5);
        s.replace_tasks(vec![a, b]).await.unwrap();

        // 英文 slug
        assert_eq!(s.search_tasks(vec!["first".into()], 10).await.unwrap().len(), 1);
        // 中文名
        let zh = s.search_tasks(vec!["彻夜".into()], 10).await.unwrap();
        assert_eq!(zh.len(), 1, "中文名也要能查到: {zh:?}");
        assert_eq!(zh[0].normalized_name, "first-in-line");
        // 没有中文名的条目不受影响
        assert_eq!(s.search_tasks(vec!["gunsmith".into()], 10).await.unwrap().len(), 1);
        // 查不到就是空
        assert!(s.search_tasks(vec!["不存在".into()], 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn upsert_then_get_roundtrip() {
        let s = store();
        let id = s
            .upsert(spec(ResourceScope::Group, "G1", "地图", "/img/map.png"))
            .await
            .unwrap();
        let got = s.get(id).await.unwrap().expect("应当能取回");
        assert_eq!(got.name, "地图");
        assert_eq!(got.scope, ResourceScope::Group);
        assert_eq!(got.owner_id, "G1");
        assert_eq!(got.path, PathBuf::from("/img/map.png"));
        assert_eq!(got.file_type, 1);
    }

    #[tokio::test]
    async fn upsert_same_name_updates_in_place() {
        let s = store();
        let first = s.upsert(spec(ResourceScope::Group, "G1", "地图", "/a.png")).await.unwrap();
        let second = s.upsert(spec(ResourceScope::Group, "G1", "地图", "/b.png")).await.unwrap();
        assert_eq!(first, second, "同名应当覆盖而不是新增");
        assert_eq!(s.get(second).await.unwrap().unwrap().path, PathBuf::from("/b.png"));
        assert_eq!(s.keywords().await.unwrap().len(), 1, "关键词不应重复登记");
    }

    #[tokio::test]
    async fn same_keyword_may_exist_in_different_groups() {
        let s = store();
        s.upsert(spec(ResourceScope::Group, "G1", "地图", "/g1.png")).await.unwrap();
        s.upsert(spec(ResourceScope::Group, "G2", "地图", "/g2.png")).await.unwrap();
        assert_eq!(s.keywords().await.unwrap().len(), 2, "两个群各自持有一份同名关键词");
    }

    #[tokio::test]
    async fn duplicate_keyword_in_same_scope_is_rejected() {
        let s = store();
        s.upsert(spec(ResourceScope::Group, "G1", "地图", "/a.png")).await.unwrap();
        let other = s.upsert(spec(ResourceScope::Group, "G1", "别的", "/b.png")).await.unwrap();

        let err = s
            .add_keyword("地图", ResourceScope::Group, "G1", other)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("已被占用"), "应拒绝同群内重复关键词: {err:#}");
    }

    #[tokio::test]
    async fn system_resources_share_one_owner_slot() {
        let s = store();
        s.upsert(spec(ResourceScope::System, SYSTEM_OWNER, "地图", "/sys.png"))
            .await
            .unwrap();
        let other = s
            .upsert(spec(ResourceScope::System, SYSTEM_OWNER, "别的", "/sys2.png"))
            .await
            .unwrap();

        // 这条断言是 NULL 陷阱的回归：owner_id 若用 NULL，
        // 三列主键会失效，下面这次插入就会悄悄成功。
        let err = s
            .add_keyword("地图", ResourceScope::System, SYSTEM_OWNER, other)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("已被占用"), "{err:#}");
    }

    #[tokio::test]
    async fn same_keyword_coexists_across_group_and_system() {
        let s = store();
        s.upsert(spec(ResourceScope::Group, "G1", "地图", "/g.png")).await.unwrap();
        s.upsert(spec(ResourceScope::System, SYSTEM_OWNER, "地图", "/s.png"))
            .await
            .unwrap();
        // 群与系统各持一份，触发时由插件决定优先级。
        assert_eq!(s.keywords().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn delete_removes_keywords_too() {
        let s = store();
        let id = s.upsert(spec(ResourceScope::Group, "G1", "地图", "/a.png")).await.unwrap();
        s.add_keyword("海关", ResourceScope::Group, "G1", id).await.unwrap();
        assert_eq!(s.keywords().await.unwrap().len(), 2);

        assert!(s.delete(ResourceScope::Group, "G1", "地图").await.unwrap());
        assert!(s.keywords().await.unwrap().is_empty(), "关键词应当随资源级联删除");
        assert!(s.get(id).await.unwrap().is_none());
        assert!(!s.delete(ResourceScope::Group, "G1", "地图").await.unwrap(), "重复删除应返回 false");
    }

    #[tokio::test]
    async fn list_is_scoped_and_sorted() {
        let s = store();
        // 排序用 ASCII 名验证：SQLite 默认 BINARY 排序，中文按 UTF-8 字节序，
        // 不是拼音序 —— 对「列表稳定可复现」够用，但别指望它符合中文语感。
        s.upsert(spec(ResourceScope::Group, "G1", "b", "/b.png")).await.unwrap();
        s.upsert(spec(ResourceScope::Group, "G1", "a", "/a.png")).await.unwrap();
        s.upsert(spec(ResourceScope::Group, "G2", "c", "/c.png")).await.unwrap();
        s.upsert(spec(ResourceScope::System, SYSTEM_OWNER, "d", "/d.png")).await.unwrap();

        let g1 = s.list(ResourceScope::Group, "G1").await.unwrap();
        let names: Vec<&str> = g1.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"], "只应包含本群资源，且按名称排序");
        assert_eq!(s.list(ResourceScope::System, SYSTEM_OWNER).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn settings_roundtrip() {
        let s = store();
        assert!(s.get_setting("nope").await.unwrap().is_none());
        s.set_setting("k", "v1").await.unwrap();
        assert_eq!(s.get_setting("k").await.unwrap().as_deref(), Some("v1"));
        s.set_setting("k", "v2").await.unwrap();
        assert_eq!(s.get_setting("k").await.unwrap().as_deref(), Some("v2"));
    }

    #[tokio::test]
    async fn seed_writes_default_only_once() {
        let conn = Connection::open_in_memory().unwrap();
        crate::schema::migrate(&conn).unwrap();
        let seeded: String = conn
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                [SYSTEM_CONTROLLERS_KEY],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(seeded, format!("[\"{DEFAULT_SYSTEM_CONTROLLER}\"]"));

        // 清空后再次 migrate 不应把默认值写回来 ——
        // 否则「删掉最后一个控制者」会是一个无法完成的动作。
        conn.execute("DELETE FROM settings WHERE key = ?1", [SYSTEM_CONTROLLERS_KEY])
            .unwrap();
        crate::schema::migrate(&conn).unwrap();
        let left: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM settings WHERE key = ?1",
                [SYSTEM_CONTROLLERS_KEY],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(left, 0, "已清空的控制者列表不该被重新播种");
    }
}

