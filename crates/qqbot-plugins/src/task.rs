//! 塔科夫任务查询（B4）与数据导入（B5）。
//!
//! # 数据源：静态 JSON + 语言包，**不走 GraphQL**
//!
//! api.tarkov.dev/graphql 自 2026-09 起对所有查询返回 422
//! （实测 GET/POST、带 UA 与 Origin、间隔重试多次，一律是
//! GraphQL server unavailable. Try again later.）。而它的中文本来就来自
//! json.tarkov.dev 的语言包，所以这里直接取语言包：
//!
//! | 端点 | 作用 |
//! |---|---|
//! | regular/tasks | 515 条任务：目标、奖励、前置、地图、经验… |
//! | regular/traders | 商人的 slug 与头像 |
//! | regular/tasks_zh | 任务名（「<id> name」）/ 目标描述（「<目标 id>」）/ 技能名 |
//! | regular/items_zh | 奖励物品名与目标武器名（「<id> Name」） |
//! | regular/traders_zh | 商人名（「<id> Nickname」） |
//! | regular/maps_zh | 地图名（「<id> Name」） |
//!
//! 实测覆盖率：任务名 515/515、目标 1441/1441、奖励物品 901/901、
//! 目标武器 730/730、商人 515/515、地图 263/263。
//! 所以**中文是必需项**：覆盖率低于 MIN_COVERAGE 就整体拒绝写库、
//! 保留旧数据 —— 宁可不更新，也不给用户看英文 slug。
//!
//! # 后续任务
//!
//! 上游**没有** successor 字段，由 taskRequirements 反转得到：
//! A 的前置里有 B ⇒ B 的后续里就有 A。实测 219 条任务有前置（238 条边），
//! 反转后 200 条任务带后续。
//!
//! # 卡片形态
//!
//! 一条 Markdown 消息把任务讲完（名称 / 概要 / 目标 / 奖励 / 前置 / 后续 …），
//! **空段不出现**。前置与后续任务做成**可点击文本**（`qqbot-cmd-input`，点击只是把
//! 「查任务 <中文名>」插进输入框）：群聊不支持 `qqbot-cmd-enter`（点击直接发送），
//! 而键盘按钮会被客户端截断成「惩…」，所以两者都没用。
//!
//! # 分层：导入时解析成人话，渲染时只管排版
//!
//! 渲染层**不做任何语义判断** —— 它不知道「数量」「可选」「战局内」这些概念，
//! 只拿到 `display_text` 与 `marks`（见 `qqbot_store::TarkovObjective`）。
//! 数量该并进正文还是单列一行、×1 要不要显示、武器清单要不要写，
//! 全都在导入时决定一次，而不是每加一条排版规则就在渲染函数里多一个 `if`。
//!
//! 渲染层唯一要决定的是**版式**：目标少时逐条列出、提示走块引用；
//! 目标多时整段塞进代码块 —— QQ 客户端会把代码块折叠起来，卡片不至于被撑爆。

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;

use async_trait::async_trait;
use qqbot_core::{Ctx, Handled, Handler};
use qqbot_store::{
    now_unix, ResourceStore, SYSTEM_CONTROLLERS_KEY, TarkovObjective, TarkovPrereq, TarkovReward,
    TarkovSuccessor, TarkovTask, TarkovTaskDetail, TarkovTaskFail, TarkovTaskKey,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::ammo::query_tokens;
use crate::timewin::{SHANGHAI_OFFSET, format_datetime};

/// 一条列表消息最多列几条候选。
///
/// **不做翻页**：群聊里官方只支持「参数指令」文本（点击后插入输入框），
/// 「回车指令」群聊不支持，而按钮会被客户端挤成「惩…」。候选太多时
/// 直接提示补关键词，比翻页干净。
const LIST_LIMIT: usize = 20;
/// 检索最多取多少条候选。
const SEARCH_LIMIT: usize = 100;
/// 中文覆盖率下限。低于它说明语言包没拉到、或上游改了路径。
const MIN_COVERAGE: f64 = 0.98;

/// 任务插件配置。
#[derive(Clone)]
pub struct TaskConfig {
    /// 任务主数据。
    pub tasks_url: String,
    /// 商人的 slug 与头像。
    pub traders_url: String,
    /// 任务名 / 目标描述 / 技能名的语言包。
    pub tasks_zh_url: String,
    /// 物品名语言包（奖励物品、目标武器、钥匙）。
    pub items_zh_url: String,
    /// 商人名语言包。
    pub traders_zh_url: String,
    /// 地图名语言包。
    pub maps_zh_url: String,
    pub store: Option<Arc<ResourceStore>>,
}

impl Default for TaskConfig {
    fn default() -> Self {
        Self {
            tasks_url: "https://json.tarkov.dev/regular/tasks".into(),
            traders_url: "https://json.tarkov.dev/regular/traders".into(),
            tasks_zh_url: "https://json.tarkov.dev/regular/tasks_zh".into(),
            items_zh_url: "https://json.tarkov.dev/regular/items_zh".into(),
            traders_zh_url: "https://json.tarkov.dev/regular/traders_zh".into(),
            maps_zh_url: "https://json.tarkov.dev/regular/maps_zh".into(),
            store: None,
        }
    }
}

// ---- 上游 JSON 的形状 ----

#[derive(Debug, Deserialize)]
struct TasksResponse {
    #[serde(default)]
    data: Option<TasksData>,
}

#[derive(Debug, Deserialize)]
struct TasksData {
    #[serde(default)]
    tasks: HashMap<String, TaskEntry>,
}

#[derive(Debug, Deserialize)]
struct TaskEntry {
    /// **翻译键**（「<id> name」），不是任务名。
    #[serde(default)]
    name: String,
    #[serde(default, rename = "normalizedName")]
    normalized_name: String,
    #[serde(default)]
    trader: String,
    #[serde(default, rename = "minPlayerLevel")]
    min_player_level: i64,
    #[serde(default, rename = "kappaRequired")]
    kappa_required: bool,
    #[serde(default, rename = "lightkeeperRequired")]
    lightkeeper_required: bool,
    #[serde(default)]
    experience: i64,
    #[serde(default)]
    objectives: Vec<ObjectiveEntry>,
    #[serde(default, rename = "startRewards")]
    start_rewards: Option<RewardsEntry>,
    #[serde(default, rename = "finishRewards")]
    finish_rewards: Option<RewardsEntry>,
    #[serde(default, rename = "taskRequirements")]
    task_requirements: Vec<RequirementEntry>,
    #[serde(default, rename = "failConditions")]
    fail_conditions: Vec<RequirementEntry>,
    #[serde(default, rename = "taskImageLink")]
    task_image_link: String,
    /// ⚠️ 可为 null —— 实测 515 条里有 252 条不是地图任务。
    #[serde(default)]
    map: Option<String>,
    #[serde(default, rename = "wikiLink")]
    wiki_link: String,
    #[serde(default, rename = "factionName")]
    faction_name: String,
    #[serde(default)]
    restartable: bool,
    #[serde(default, rename = "neededKeys")]
    needed_keys: Vec<NeededKeyEntry>,
    #[serde(default, rename = "traderRequirements")]
    trader_requirements: Vec<TraderRequirementEntry>,
    // 上游还有 availableDelaySecondsMin/Max（接取冷却）。卡片从头到尾没显示过它，
    // v12 索性不声明 —— serde 默认忽略没声明的字段，少一个「存了但没人读」
    // 的字段，就少一处将来会被读错的真相。
}

#[derive(Debug, Deserialize)]
struct ObjectiveEntry {
    #[serde(default)]
    id: String,
    /// 实测**就是目标自己的 id**，一个翻译键。
    #[serde(default)]
    description: String,
    #[serde(default, rename = "type")]
    objective_type: String,
    #[serde(default)]
    optional: bool,
    #[serde(default)]
    count: Option<i64>,
    #[serde(default, rename = "foundInRaid")]
    found_in_raid: bool,
    // 上游还有 usingWeapon（限定武器）/ targetNames（目标代号）/ maps（限定地图）。
    // 它们**故意不读**：上游会给四五把同类武器，拼出来比正文还长，
    // 而描述文本本身已经写了「使用 AKS-74U」「在海关…」。取舍理由见 objective_view。
    /// ⚠️ 显式可为 null —— 实测有 1 个目标就是 null，
    /// 声明成 i64 会让整个导入在那一条上失败。
    #[serde(default, rename = "timeFromHour")]
    time_from_hour: Option<i64>,
    #[serde(default, rename = "timeUntilHour")]
    time_until_hour: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
struct RewardsEntry {
    #[serde(default, rename = "traderStanding")]
    trader_standing: Vec<StandingEntry>,
    #[serde(default)]
    items: Vec<RewardItemEntry>,
    #[serde(default, rename = "skillLevelReward")]
    skill_level_reward: Vec<SkillEntry>,
    #[serde(default, rename = "craftUnlock")]
    craft_unlock: Vec<CraftUnlockEntry>,
}

#[derive(Debug, Deserialize)]
struct StandingEntry {
    #[serde(default)]
    trader: String,
    #[serde(default)]
    standing: f64,
}

#[derive(Debug, Deserialize)]
struct RewardItemEntry {
    #[serde(default)]
    item: String,
    #[serde(default)]
    count: f64,
}

/// 技能奖励。
///
/// ⚠️ 上游字段名是 **skill**（{"level":2,"skill":"Surgery"}）。
/// 早先这里声明成 name，于是 136 条技能奖励的名字**恒为空** ——
/// 反序列化不会报错，只会静默给空串。
#[derive(Debug, Deserialize)]
struct SkillEntry {
    #[serde(default)]
    skill: String,
    #[serde(default)]
    level: f64,
}

#[derive(Debug, Deserialize)]
struct CraftUnlockEntry {
    #[serde(default)]
    level: i64,
    #[serde(default)]
    station: String,
    #[serde(default)]
    item: String,
    #[serde(default)]
    count: f64,
}

#[derive(Debug, Deserialize)]
struct NeededKeyEntry {
    #[serde(default)]
    map: String,
    #[serde(default)]
    keys: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct TraderRequirementEntry {
    #[serde(default)]
    trader: String,
    #[serde(default, rename = "requirementType")]
    requirement_type: String,
    #[serde(default, rename = "compareMethod")]
    compare_method: String,
    #[serde(default)]
    value: f64,
}

#[derive(Debug, Deserialize)]
struct RequirementEntry {
    #[serde(default)]
    task: String,
    #[serde(default)]
    status: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct TradersResponse {
    #[serde(default)]
    data: Option<HashMap<String, TraderEntry>>,
}

#[derive(Debug, Deserialize)]
struct TraderEntry {
    #[serde(default, rename = "normalizedName")]
    normalized_name: String,
    #[serde(default, rename = "imageLink")]
    image_link: String,
}

// ---- 语言包 ----

/// 一份语言包：翻译键 → 中文。
pub type LangPack = HashMap<String, String>;

#[derive(Debug, Default, Deserialize)]
struct LangPackResponse {
    #[serde(default)]
    data: Option<LangPack>,
}

/// 解析一份语言包。
pub fn parse_lang_pack(body: &str) -> Result<LangPack, String> {
    let parsed: LangPackResponse =
        serde_json::from_str(body).map_err(|err| format!("解析语言包失败：{err}"))?;
    parsed.data.ok_or_else(|| "语言包缺少 data".to_string())
}

/// 导入时用到的四份语言包。
#[derive(Debug, Default, Clone)]
pub struct LangPacks {
    pub tasks: LangPack,
    pub items: LangPack,
    pub traders: LangPack,
    pub maps: LangPack,
}

impl LangPacks {
    fn task_name(&self, key: &str) -> Option<String> {
        lookup(&self.tasks, key)
    }

    fn objective(&self, key: &str) -> Option<String> {
        lookup(&self.tasks, key)
    }

    fn skill(&self, key: &str) -> Option<String> {
        lookup(&self.tasks, key)
    }

    fn item(&self, id: &str) -> Option<String> {
        lookup(&self.items, &format!("{id} Name"))
    }

    fn trader(&self, id: &str) -> Option<String> {
        lookup(&self.traders, &format!("{id} Nickname"))
    }

    fn map(&self, id: &str) -> Option<String> {
        lookup(&self.maps, &format!("{id} Name"))
    }
}

fn lookup(pack: &LangPack, key: &str) -> Option<String> {
    pack.get(key)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// 中文覆盖率。导入回执与「不达标就不写库」都靠它。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Coverage {
    pub names_hit: usize,
    pub names_total: usize,
    pub objectives_hit: usize,
    pub objectives_total: usize,
    pub items_hit: usize,
    pub items_total: usize,
}

impl Coverage {
    /// 任务名 + 目标的命中率 —— 这两项决定要不要写库。
    pub fn ratio(&self) -> f64 {
        let total = self.names_total + self.objectives_total;
        if total == 0 {
            return 0.0;
        }
        (self.names_hit + self.objectives_hit) as f64 / total as f64
    }

    /// 回执里那半句话。
    pub fn summary(&self) -> String {
        format!(
            "中文覆盖 {:.0}%（任务 {}/{}，目标 {}/{}）",
            self.ratio() * 100.0,
            self.names_hit,
            self.names_total,
            self.objectives_hit,
            self.objectives_total
        )
    }
}

// ---- 解析 ----

/// 解析任务、商人与四份语言包，产出可入库的完整任务列表与覆盖率。
///
/// 名字对不上的任务会被**丢掉**（留着只能显示 slug），
/// 覆盖率因此下降，最终由调用方决定是否写库。
pub fn parse_tasks(
    tasks_body: &str,
    traders_body: &str,
    packs: &LangPacks,
) -> Result<(Vec<TarkovTaskDetail>, Coverage), String> {
    let traders: TradersResponse =
        serde_json::from_str(traders_body).map_err(|err| format!("解析商人数据失败：{err}"))?;
    let trader_info: HashMap<String, (String, String)> = traders
        .data
        .unwrap_or_default()
        .into_iter()
        .filter(|(_, t)| !t.normalized_name.trim().is_empty())
        .map(|(id, t)| (id, (t.normalized_name, t.image_link)))
        .collect();

    let parsed: TasksResponse =
        serde_json::from_str(tasks_body).map_err(|err| format!("解析任务数据失败：{err}"))?;
    let tasks = parsed.data.ok_or("任务响应缺少 data")?.tasks;

    // 先建一份 id → 中文名 的表：失败条件、后续任务都要拿它把 id 变成名字。
    let names: HashMap<String, String> = tasks
        .iter()
        .filter_map(|(id, t)| packs.task_name(&t.name).map(|n| (id.clone(), n)))
        .collect();

    let mut coverage = Coverage::default();
    let updated_at = now_unix();
    let mut out: Vec<TarkovTaskDetail> = Vec::new();

    for (id, task) in tasks {
        let normalized_name = task.normalized_name.trim().to_string();
        if normalized_name.is_empty() {
            continue;
        }
        coverage.names_total += 1;
        let Some(name_zh) = packs.task_name(&task.name) else {
            continue;
        };
        coverage.names_hit += 1;

        let (trader, trader_image) = trader_info.get(&task.trader).cloned().unwrap_or_default();
        let trader_name_zh = packs.trader(&task.trader);
        let map_id = task.map.clone().unwrap_or_default();
        let map_name_zh = packs.map(&map_id);

        // 三张子表的内容在这里**一次算好**（名字用语言包解析），渲染层只管排版。
        // 下面几行之后 task 会被逐字段搬走，所以必须在这里借。
        let keys: Vec<TarkovTaskKey> = task
            .needed_keys
            .iter()
            .filter_map(|k| {
                let keys = k
                    .keys
                    .iter()
                    .map(|id| packs.item(id).unwrap_or_else(|| id.clone()))
                    .collect::<Vec<_>>()
                    .join("、");
                // 上游给过空列表就是真的没有钥匙 —— 别渲染出一行「海关：」。
                // 实测 57 行里没有这种，但上游换数据结构时不该靠运气。
                if keys.is_empty() {
                    return None;
                }
                Some(TarkovTaskKey {
                    map_name: packs.map(&k.map).unwrap_or_default(),
                    keys,
                })
            })
            .collect();
        let requirements: Vec<String> = task
            .trader_requirements
            .iter()
            .map(|r| {
                let trader = packs.trader(&r.trader).unwrap_or_else(|| r.trader.clone());
                let kind = match r.requirement_type.as_str() {
                    "level" => "忠诚等级",
                    "standing" => "声望",
                    other => other,
                };
                format!("{trader} {kind} {} {}", r.compare_method, format_amount(r.value))
            })
            .collect();
        let fails: Vec<TarkovTaskFail> = task
            .fail_conditions
            .iter()
            .filter(|f| !f.task.is_empty())
            .map(|f| TarkovTaskFail {
                task_name: display_name(&names, &f.task),
                status: f.status.join(","),
            })
            .collect();
        // 上游的 `Any` 等于「不限阵营」，存成 NULL 比存字符串 `Any` 干净。
        let faction = match task.faction_name.as_str() {
            "" | "Any" => None,
            other => Some(other.to_string()),
        };

        let mut objectives = Vec::new();
        for o in task.objectives {
            let key = objective_key(&o);
            coverage.objectives_total += 1;
            let description_zh = packs.objective(&key);
            if description_zh.is_some() {
                coverage.objectives_hit += 1;
            }
            // 正文与标记在**这里**定下来 —— 渲染层拿到的就是最终形态。
            let (display_text, marks) = objective_view(&o, description_zh.as_deref());
            objectives.push(TarkovObjective {
                objective_type: o.objective_type,
                description_key: key,
                display_text,
                marks,
            });
        }

        let mut rewards = Vec::new();
        if let Some(r) = task.start_rewards {
            push_rewards(&mut rewards, r, packs, &mut coverage, "start_item", "start_standing");
        }
        if let Some(r) = task.finish_rewards {
            push_rewards(&mut rewards, r, packs, &mut coverage, "item", "standing");
        }

        let prereqs = task
            .task_requirements
            .into_iter()
            .filter(|r| !r.task.is_empty())
            .map(|r| TarkovPrereq {
                prereq_id: r.task,
                // 上游给的是数组（如 ["complete"]），拍平存下来。
                status: r.status.join(","),
            })
            .collect();

        out.push(TarkovTaskDetail {
            task: TarkovTask {
                id,
                normalized_name: normalized_name.clone(),
                name_zh: Some(name_zh),
                name_en: Some(english_name(&task.wiki_link, &normalized_name)),
                trader,
                trader_name_zh,
                trader_image,
                task_image: task.task_image_link,
                map: map_id,
                map_name_zh,
                min_level: task.min_player_level,
                is_kappa: task.kappa_required,
                is_lightkeeper: task.lightkeeper_required,
                experience: task.experience,
                wiki_link: task.wiki_link,
                faction,
                restartable: task.restartable,
                updated_at,
            },
            objectives,
            rewards,
            prereqs,
            // 下面统一反转前置时填。
            successors: Vec::new(),
            keys,
            requirements,
            fails,
        });
    }

    build_successors(&mut out, &names);
    out.sort_by(|a, b| a.task.normalized_name.cmp(&b.task.normalized_name));
    Ok((out, coverage))
}

/// 把一份奖励块里的各类奖励拍成行。
fn push_rewards(
    out: &mut Vec<TarkovReward>,
    r: RewardsEntry,
    packs: &LangPacks,
    coverage: &mut Coverage,
    item_kind: &str,
    standing_kind: &str,
) {
    for s in r.trader_standing {
        out.push(TarkovReward {
            kind: standing_kind.to_string(),
            name_zh: packs.trader(&s.trader),
            ref_id: s.trader,
            amount: s.standing,
            extra: None,
        });
    }
    for i in r.items {
        coverage.items_total += 1;
        let name_zh = packs.item(&i.item);
        if name_zh.is_some() {
            coverage.items_hit += 1;
        }
        out.push(TarkovReward {
            kind: item_kind.to_string(),
            ref_id: i.item,
            name_zh,
            amount: i.count,
            extra: None,
        });
    }
    for k in r.skill_level_reward {
        out.push(TarkovReward {
            kind: "skill".to_string(),
            name_zh: packs.skill(&k.skill),
            ref_id: k.skill,
            amount: k.level,
            extra: None,
        });
    }
    for c in r.craft_unlock {
        out.push(TarkovReward {
            kind: "unlock".to_string(),
            name_zh: packs.item(&c.item),
            ref_id: c.item,
            amount: c.count,
            extra: Some(json!({ "station": c.station, "level": c.level }).to_string()),
        });
    }
}

/// 反转前置得到后续。顺带按显示名排序，让输出稳定。
fn build_successors(out: &mut [TarkovTaskDetail], names: &HashMap<String, String>) {
    let mut by_prereq: HashMap<String, Vec<TarkovSuccessor>> = HashMap::new();
    for d in out.iter() {
        for p in &d.prereqs {
            by_prereq.entry(p.prereq_id.clone()).or_default().push(TarkovSuccessor {
                successor_id: d.task.id.clone(),
                status: p.status.clone(),
            });
        }
    }
    for d in out.iter_mut() {
        let Some(mut list) = by_prereq.remove(&d.task.id) else {
            continue;
        };
        list.sort_by(|a, b| {
            display_name(names, &a.successor_id).cmp(&display_name(names, &b.successor_id))
        });
        d.successors = list;
    }
}

/// 目标的语言包键。
///
/// 静态 JSON 里的 `description` 实测就是目标自己的 id（一个翻译键）；
/// 万一为空，退回 id 本身。
fn objective_key(o: &ObjectiveEntry) -> String {
    if o.description.is_empty() { o.id.clone() } else { o.description.clone() }
}

/// 一条目标在**导入时**就该定下来的形态：正文 + 标记。
///
/// 这是「渲染层不做语义判断」的落点：数量怎么摆、×1 要不要写、哪些字段算提示，
/// 全在这里决定一次，存进 `display_text` / `marks`。
fn objective_view(o: &ObjectiveEntry, description_zh: Option<&str>) -> (String, String) {
    // 正文：语言包优先；缺了就按 objective_type 拼一句通用的（实测走不到）。
    let mut text = match description_zh {
        Some(t) => t.to_string(),
        None => objective_label(&o.objective_type).to_string(),
    };

    // 数量并进正文 —— 玩家读的就是「消灭 Scav ×25」。
    // 只有 >1 才写：实测 1441 条目标里 768 条的 count 就是 1，
    // 「上交文件 ×1」是纯噪音。
    if let Some(n) = o.count
        && n > 1
    {
        let _ = write!(text, " ×{n}");
    }

    // 标记只留描述文本**没有覆盖到**的信息。
    //
    // 武器清单与限定地图故意丢掉：上游会给四五把同类武器，拼出来比正文还长，
    // 而描述本身已经写了「使用 AKS-74U」「在海关…」。这个取舍放在这里而不是
    // 渲染层，是为了让「存什么」与「印什么」是同一个决定。
    let mut marks: Vec<String> = Vec::new();
    if o.optional {
        marks.push("可选".to_string());
    }
    if o.found_in_raid {
        marks.push("战局内".to_string());
    }
    if let (Some(from), Some(until)) = (o.time_from_hour, o.time_until_hour)
        && (from != 0 || until != 0)
    {
        marks.push(format!("{from}:00-{until}:00"));
    }
    // 分隔符用 `|` 而不是 ` · `：怎么呈现是排版的事。
    (text, marks.join("|"))
}

/// 把 wiki 链接或 slug 还原成人看的英文名。
///
/// 优先用 wiki 链接的最后一段（The_Punisher_-_Part_1 → The Punisher - Part 1），
/// 没有就按 - 切分首字母大写。
pub fn english_name(wiki_link: &str, normalized_name: &str) -> String {
    if let Some(rest) = wiki_link.split("/wiki/").nth(1) {
        let decoded = rest.replace('_', " ").replace("%20", " ");
        if !decoded.trim().is_empty() {
            return decoded.trim().to_string();
        }
    }
    normalized_name
        .split('-')
        .filter(|s| !s.is_empty())
        .map(|s| {
            let mut chars = s.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// id → 显示名（中文名优先，退回 id）。
fn display_name(names: &HashMap<String, String>, id: &str) -> String {
    names.get(id).cloned().unwrap_or_else(|| id.to_string())
}

// ---- 渲染 ----

/// 目标类型 → 中文。
///
/// **兜底用**：语言包实测覆盖 1441/1441，只有上游加了新目标类型而语言包还没跟上时
/// 才会走到这里。认不出来就原样返回，不显示成空白。
pub fn objective_label(objective_type: &str) -> &str {
    match objective_type {
        "visit" => "探访地点",
        "shoot" => "击杀",
        "giveItem" => "交付物品",
        "findItem" => "找到物品",
        "findQuestItem" => "找到任务物品",
        "giveQuestItem" => "交付任务物品",
        "plantItem" => "放置物品",
        "plantQuestItem" => "放置任务物品",
        "extract" => "撤离",
        "mark" => "标记",
        "buildWeapon" => "组装武器",
        "weaponAssembly" => "武器改装",
        "sellItem" => "出售物品",
        "useItem" => "使用物品",
        "skill" => "技能等级",
        "traderLevel" => "商人等级",
        "traderStanding" => "商人声望",
        "playerLevel" => "玩家等级",
        "experience" => "经验",
        "taskStatus" => "任务状态",
        other => other,
    }
}

/// 起一个新内容块：`***` 分割线 + 二级标题。
///
/// 官方 markdown 用 `***` 作水平分割线（见协议文档的「Markdown 消息支持的语法」）。
fn section(out: &mut String, title: &str) {
    out.push_str("\n***\n\n");
    let _ = writeln!(out, "## {title}");
}

/// 把导入时用 `|` 拼好的标记换成显示用的分隔符。
///
/// 只在真的要写出去时调一次 —— 判断「标记是否相同」直接比原文，
/// 不必先拆成 Vec 再比（那样每条目标都要分配一次）。
fn display_marks(marks: &str) -> String {
    marks.replace('|', " · ")
}

/// 超过这个条数就把整段目标塞进代码块。
///
/// QQ 客户端会把代码块折叠起来（点「展开」才铺开），所以目标再多也不会把卡片
/// 撑成一屏半。**代价是代码块里的 `>` 不再是块引用** —— 这正是标记要按版式
/// 换写法、而不能在导入时就写死成 `> …` 的原因。
const OBJECTIVE_COLLAPSE_AT: usize = 3;

/// 渲染「任务目标」小节 —— 渲染层**唯一**要做的判断。
///
/// 数量、可选、战局内这些语义早在导入时就定好了（见 `objective_view`），
/// 这里只决定怎么摆：
///
/// - 目标 ≤ 3：逐条列出，标记挂在各自那条下面走 `>` 块引用；
/// - 目标 > 3：整段塞进代码块，标记改写成行内括号（代码块里 `>` 不渲染）；
/// - 所有目标标记相同：标记提到标题下写一次，逐条不再重复。
///   这与前两条正交：「小本生意 - 1」有 18 条目标全是「战局内」，
///   逐条重复 18 行纯属噪音，提到标题下写一次就够。
fn render_objectives(out: &mut String, objectives: &[TarkovObjective]) {
    // 直接比 `marks` 原文：拆开只是为了显示，判断是否相同不需要拆。
    // 只有一条目标时不合并：「全部目标」配一条目标读着别扭。
    let shared = objectives
        .first()
        .filter(|first| {
            objectives.len() > 1
                && !first.marks.is_empty()
                && objectives.iter().all(|o| o.marks == first.marks)
        })
        .map(|first| display_marks(&first.marks));
    if let Some(m) = &shared {
        let _ = writeln!(out, "> 全部目标：{m}");
    }

    let collapsed = objectives.len() > OBJECTIVE_COLLAPSE_AT;
    if collapsed {
        // 前面若是标题或块引用，围栏代码块要空一行才认得出来。
        out.push('\n');
        out.push_str("```\n");
    }
    for (i, o) in objectives.iter().enumerate() {
        // 标记上提之后，逐条就不用再写一遍了。
        let marks = if shared.is_some() { "" } else { o.marks.as_str() };
        if collapsed && !marks.is_empty() {
            // 代码块里 `>` 不渲染成块引用，标记只能并回正文那一行。
            let _ = writeln!(out, "{}. {}（{}）", i + 1, o.display_text, display_marks(marks));
        } else {
            let _ = writeln!(out, "{}. {}", i + 1, o.display_text);
            if !collapsed && !marks.is_empty() {
                let _ = writeln!(out, "> {}", display_marks(marks));
            }
        }
    }
    if collapsed {
        out.push_str("```\n");
    }
}

/// 任务详情的 Markdown 卡片。**空段不出现**。
///
/// 排版按官方 markdown 规范（见 `docs/qq-bot-api-v2-protocol.md`
/// 的「Markdown 消息支持的语法」）：一级标题放任务名、顶部配图、
/// `***` 分隔内容块、`>` 块引用放目标提示。
pub fn render_detail(detail: &TarkovTaskDetail, names: &HashMap<String, String>) -> String {
    let t = &detail.task;
    let mut out = String::new();
    let title = t.name_zh.as_deref().unwrap_or(&t.normalized_name);
    // 一级标题 + 任务配图。官方支持 `![alt #宽px #高px](公网 url)`，
    // 开放平台会自己下载转存（实测任务图 314×177）。
    let _ = writeln!(out, "# {title}");
    if !t.task_image.is_empty() {
        let _ = writeln!(out, "![{title} #320px #180px]({})", t.task_image);
    }
    if let Some(en) = t.name_en.as_deref()
        && !en.is_empty()
    {
        let _ = writeln!(out, "{en}");
    }
    out.push('\n');

    // 概要行：有什么写什么。
    let mut brief: Vec<String> = Vec::new();
    let trader = t.trader_name_zh.as_deref().unwrap_or(&t.trader);
    if !trader.is_empty() {
        brief.push(format!("商人：{trader}"));
    }
    // 282/515 条任务的等级就是 0（没有等级门槛），显示「等级：0」是噪音。
    if t.min_level > 0 {
        brief.push(format!("等级：{}", t.min_level));
    }
    if t.experience > 0 {
        brief.push(format!("经验：{}", format_amount(t.experience as f64)));
    }
    if let Some(map) = t.map_name_zh.as_deref()
        && !map.is_empty()
    {
        brief.push(format!("地图：{map}"));
    }
    if t.is_kappa {
        brief.push("3x4 任务".to_string());
    }
    if t.is_lightkeeper {
        brief.push("灯塔商人".to_string());
    }
    if let Some(faction) = t.faction.as_deref()
        && !faction.is_empty()
    {
        brief.push(format!("阵营：{faction}"));
    }
    if t.restartable {
        brief.push("可重复接取".to_string());
    }
    // 商人头像（128×128，缩到 32px）放在概要行前面。
    if t.trader_image.is_empty() {
        let _ = writeln!(out, "{}", brief.join(" · "));
    } else {
        let _ = writeln!(
            out,
            "![{trader} #32px #32px]({}) {}",
            t.trader_image,
            brief.join(" · ")
        );
    }

    if !detail.objectives.is_empty() {
        section(&mut out, "任务目标");
        render_objectives(&mut out, &detail.objectives);
    }

    let finish: Vec<&TarkovReward> =
        detail.rewards.iter().filter(|r| !r.kind.starts_with("start_")).collect();
    if !finish.is_empty() {
        section(&mut out, "任务奖励");
        for r in finish {
            let _ = writeln!(out, "- {}", reward_label(r));
        }
    }

    let start: Vec<&TarkovReward> =
        detail.rewards.iter().filter(|r| r.kind.starts_with("start_")).collect();
    if !start.is_empty() {
        section(&mut out, "起始奖励");
        for r in start {
            let _ = writeln!(out, "- {}", reward_label(r));
        }
    }

    if !detail.prereqs.is_empty() {
        section(&mut out, "前置任务");
        for p in &detail.prereqs {
            let name = display_name(names, &p.prereq_id);
            let _ = writeln!(
                out,
                "- {}（{}）",
                cmd_input(&format!("查任务 {name}"), &name),
                status_label(&p.status)
            );
        }
    }

    if !detail.successors.is_empty() {
        section(&mut out, "后续任务");
        for s in &detail.successors {
            let name = display_name(names, &s.successor_id);
            let _ = writeln!(out, "- {}", cmd_input(&format!("查任务 {name}"), &name));
        }
    }

    if !detail.fails.is_empty() {
        section(&mut out, "失败条件");
        for f in &detail.fails {
            let _ = writeln!(out, "- {}（{}）", f.task_name, status_label(&f.status));
        }
    }

    if !detail.requirements.is_empty() {
        section(&mut out, "商人要求");
        for text in &detail.requirements {
            let _ = writeln!(out, "- {text}");
        }
    }

    if !detail.keys.is_empty() {
        section(&mut out, "需要钥匙");
        for k in &detail.keys {
            if k.map_name.is_empty() {
                let _ = writeln!(out, "- {}", k.keys);
            } else {
                let _ = writeln!(out, "- {}：{}", k.map_name, k.keys);
            }
        }
    }

    if !t.wiki_link.is_empty() {
        out.push_str("\n***\n\n");
        // 用 timewin 的格式化，不用 qqbot_store::fmt_unix —— 后者是给日志看的
        // 「day+N」形状，不是给人看的日期。
        let _ = writeln!(
            out,
            "[维基]({}) · 数据更新于 {}",
            t.wiki_link,
            format_datetime(t.updated_at, SHANGHAI_OFFSET)
        );
    }
    out
}

/// 一行奖励的文字。
fn reward_label(r: &TarkovReward) -> String {
    let name = r.name_zh.as_deref().unwrap_or(&r.ref_id);
    match r.kind.as_str() {
        "standing" | "start_standing" => format!("{name} 声望 {}", signed(r.amount)),
        "skill" => format!("{name} 技能 +{}", format_amount(r.amount)),
        "unlock" => {
            let level = r
                .extra
                .as_deref()
                .and_then(|e| serde_json::from_str::<Value>(e).ok())
                .and_then(|v| v.get("level").and_then(Value::as_i64));
            match level {
                Some(l) => format!("解锁 {name} ×{}（工艺 Lv{l}）", format_amount(r.amount)),
                None => format!("解锁 {name} ×{}", format_amount(r.amount)),
            }
        }
        _ => format!("{name} ×{}", format_amount(r.amount)),
    }
}

/// 数量：声望是小数，物品是整数，别把 0.1 显示成 0。
fn format_amount(amount: f64) -> String {
    if (amount.fract()).abs() < f64::EPSILON {
        format!("{}", amount as i64)
    } else {
        format!("{amount}")
    }
}

/// 带符号的数量，声望用得上。
fn signed(amount: f64) -> String {
    if amount >= 0.0 {
        format!("+{}", format_amount(amount))
    } else {
        format_amount(amount)
    }
}

/// 前置任务的状态。上游给的是英文枚举。
fn status_label(status: &str) -> String {
    status
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| match s {
            "complete" => "需完成",
            "active" => "需进行中",
            "failed" => "需失败",
            other => other,
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// 检索结果列表。
///
/// 每条候选都是**可点击的文本指令**：点一下把「任务 <中文名>」插进输入框，
/// 用户按发送即可展开那一条（官方「参数指令」，群聊可用）。
pub fn render_list(query: &str, items: &[TarkovTask], total: usize) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# 任务 · {query}");
    if total > items.len() {
        let _ = writeln!(out, "共 {total} 条，先列前 {} 条", items.len());
    } else {
        let _ = writeln!(out, "共 {total} 条");
    }
    out.push('\n');
    for t in items {
        let name = t.name_zh.as_deref().unwrap_or(&t.normalized_name);
        let mut parts: Vec<String> = Vec::new();
        let trader = t.trader_name_zh.as_deref().unwrap_or(&t.trader);
        if !trader.is_empty() {
            parts.push(trader.to_string());
        }
        // 282/515 条任务的等级就是 0，写「0 级」是噪音。
        if t.min_level > 0 {
            parts.push(format!("{} 级", t.min_level));
        }
        if t.is_kappa {
            parts.push("3x4".to_string());
        }
        let _ = writeln!(
            out,
            "- {} {}",
            cmd_input(&format!("查任务 {name}"), name),
            parts.join(" · ")
        );
    }
    if total > items.len() {
        out.push_str("\n结果较多，补充关键词可以缩小范围。\n");
    }
    out
}

/// 「参数指令」文本标签：点击后把指令**插进输入框**，用户自己按发送。
///
/// ⚠️ 指令必须以 **查任务** 开头：路由表只注册了 `查任务` / `更新任务`，
/// `任务` 这种更短的写法没有路由，发出去只会石沉大海。
///
/// 官方文档（文本交互）：指令操作只在 markdown 里生效，
/// 且 `qqbot-cmd-enter`（点击直接发送）**群聊不支持**，
/// 所以群聊只能用 `qqbot-cmd-input`。
/// 两个属性都必须 urlencode，各自上限 100 字符。
pub fn cmd_input(command: &str, show: &str) -> String {
    format!(
        "<qqbot-cmd-input text=\"{}\" show=\"{}\" reference=\"false\" />",
        url_encode(command),
        url_encode(show)
    )
}

/// 官方要求指令标签的取值走 urlencode。只放过 unreserved 字符。
fn url_encode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char);
            }
            other => {
                let _ = write!(out, "%{other:02X}");
            }
        }
    }
    out
}

// ---- 插件 ----

/// 是否是系统控制者。
///
/// 名单在 settings.system_controllers 里，与「系统控制者」命令同一份数据 ——
/// 全量导入是重操作（6 个端点、约 3.6 MB），只让系统控制者触发。
async fn is_system_controller(ctx: &Ctx, store: &ResourceStore) -> bool {
    let Some(openid) = ctx.sender_openid() else {
        return false;
    };
    match store.get_setting(SYSTEM_CONTROLLERS_KEY).await {
        Ok(Some(raw)) => serde_json::from_str::<Vec<String>>(&raw)
            .map(|list| list.iter().any(|c| c == openid))
            .unwrap_or(false),
        Ok(None) => false,
        Err(err) => {
            tracing::warn!(error = %err, "读取系统控制者失败");
            false
        }
    }
}

/// 塔科夫任务插件。
#[derive(Clone)]
pub struct TaskPlugin {
    config: TaskConfig,
    http: reqwest::Client,
}

impl TaskPlugin {
    pub fn new(config: TaskConfig, http: reqwest::Client) -> Self {
        Self { config, http }
    }

    /// 「更新任务」：六个端点 → 语言包 → 反转后续 → 五表同事务替换。
    async fn reload(&self, ctx: &Ctx) -> Handled {
        let Some(store) = &self.config.store else {
            let _ = ctx.reply_text("未启用持久化，任务功能不可用").await;
            return Handled::Consumed;
        };
        if !is_system_controller(ctx, store).await {
            let _ = ctx.reply_text("只有系统控制者能更新任务数据").await;
            return Handled::Consumed;
        }

        let fetched = tokio::try_join!(
            self.fetch(&self.config.tasks_url),
            self.fetch(&self.config.traders_url),
            self.fetch(&self.config.tasks_zh_url),
            self.fetch(&self.config.items_zh_url),
            self.fetch(&self.config.traders_zh_url),
            self.fetch(&self.config.maps_zh_url),
        );
        let (tasks_body, traders_body, tasks_zh, items_zh, traders_zh, maps_zh) = match fetched {
            Ok(v) => v,
            Err(reason) => {
                tracing::warn!(error = %reason, "下载任务数据失败");
                let _ = ctx.reply_text(format!("下载失败：{reason}")).await;
                return Handled::Consumed;
            }
        };

        let packs = match (
            parse_lang_pack(&tasks_zh),
            parse_lang_pack(&items_zh),
            parse_lang_pack(&traders_zh),
            parse_lang_pack(&maps_zh),
        ) {
            (Ok(tasks), Ok(items), Ok(traders), Ok(maps)) => LangPacks { tasks, items, traders, maps },
            (t, i, tr, m) => {
                let reason = t.err().or(i.err()).or(tr.err()).or(m.err()).unwrap_or_default();
                tracing::warn!(error = %reason, "语言包解析失败");
                let _ = ctx.reply_text(format!("语言包解析失败：{reason}")).await;
                return Handled::Consumed;
            }
        };

        let (items, coverage) = match parse_tasks(&tasks_body, &traders_body, &packs) {
            Ok(pair) => pair,
            Err(reason) => {
                tracing::warn!(error = %reason, "解析任务数据失败");
                let _ = ctx.reply_text(format!("解析失败：{reason}")).await;
                return Handled::Consumed;
            }
        };
        if items.is_empty() {
            let _ = ctx.reply_text("解析出 0 条任务，上游格式可能变了").await;
            return Handled::Consumed;
        }
        // 覆盖率不达标就**不写库**：留着旧数据总比换成一半英文的强。
        if coverage.ratio() < MIN_COVERAGE {
            tracing::warn!(
                ratio = coverage.ratio(),
                names = format!("{}/{}", coverage.names_hit, coverage.names_total),
                objectives = format!("{}/{}", coverage.objectives_hit, coverage.objectives_total),
                "中文覆盖率不达标，已放弃本次导入"
            );
            let _ = ctx
                .reply_text(format!(
                    "中文覆盖率只有 {:.0}%，低于 {:.0}% 的底线，已放弃导入（旧数据保持不变）\n{}",
                    coverage.ratio() * 100.0,
                    MIN_COVERAGE * 100.0,
                    coverage.summary()
                ))
                .await;
            return Handled::Consumed;
        }

        let prereqs: usize = items.iter().map(|d| d.prereqs.len()).sum();
        let successors: usize = items.iter().map(|d| d.successors.len()).sum();
        match store.replace_tasks(items).await {
            Ok(count) => {
                let _ = ctx
                    .reply_text(format!(
                        "任务数据已更新，共 {count} 条（{}；前置 {prereqs} 条，后续 {successors} 条）",
                        coverage.summary()
                    ))
                    .await;
            }
            Err(err) => {
                tracing::warn!(error = %err, "写入任务数据失败");
                let _ = ctx.reply_text("写入失败，稍后再试").await;
            }
        }
        Handled::Consumed
    }

    /// 「查任务 <关键词>」/「任务 <关键词>」。
    async fn search(&self, ctx: &Ctx, query: &str) -> Handled {
        let Some(store) = &self.config.store else {
            let _ = ctx.reply_text("未启用持久化，任务功能不可用").await;
            return Handled::Consumed;
        };
        let query = query.trim();
        if query.is_empty() {
            let _ = ctx.reply_text("用法：查任务 <名称片段>，例如 查任务 惩罚者").await;
            return Handled::Consumed;
        }

        // 按钮点进来的是**完整中文名**，先精确匹配一次 ——
        // 否则「惩罚者 - 1」会被 LIKE 命中「惩罚者 - 10」之类。
        if let Ok(Some(id)) = store.task_id_by_name(query.to_string()).await {
            return self.send_detail(ctx, store, id).await;
        }

        let tokens = query_tokens(query);
        if tokens.is_empty() {
            let _ = ctx.reply_text("用法：查任务 <名称片段>，例如 查任务 惩罚者").await;
            return Handled::Consumed;
        }
        let mut items = match store.search_tasks(tokens, SEARCH_LIMIT).await {
            Ok(items) => items,
            Err(err) => {
                tracing::warn!(error = %err, "检索任务失败");
                let _ = ctx.reply_text("查询失败，稍后再试").await;
                return Handled::Consumed;
            }
        };

        if items.is_empty() {
            let hint = match store.task_count().await {
                Ok(0) => "任务数据还没导入，请系统控制者发「更新任务」",
                _ => "没有匹配的任务，换个关键词试试",
            };
            let _ = ctx.reply_text(hint).await;
            return Handled::Consumed;
        }

        if items.len() == 1 {
            return self.send_detail(ctx, store, items.remove(0).id).await;
        }
        self.send_list(ctx, query, &items).await
    }

    /// 渲染并发送候选列表。
    async fn send_list(&self, ctx: &Ctx, query: &str, items: &[TarkovTask]) -> Handled {
        let total = items.len();
        let md = render_list(query, &items[..total.min(LIST_LIMIT)], total);
        if let Err(err) = ctx.reply_markdown(md).await {
            tracing::warn!(error = %err, "任务列表发送失败");
        }
        Handled::Consumed
    }

    /// 渲染并发送一条任务的详情。
    async fn send_detail(&self, ctx: &Ctx, store: &ResourceStore, id: String) -> Handled {
        let detail = match store.task_detail(id).await {
            Ok(Some(detail)) => detail,
            Ok(None) => {
                // 检索刚命中、详情却没了：只可能是并发重导。不是错误。
                let _ = ctx.reply_text("任务数据正在更新，请稍后再试").await;
                return Handled::Consumed;
            }
            Err(err) => {
                tracing::warn!(error = %err, "读取任务详情失败");
                let _ = ctx.reply_text("查询失败，稍后再试").await;
                return Handled::Consumed;
            }
        };

        // 前置 / 后续任务多半不在检索结果里，单独按 id 取一次名字。
        let mut ids: Vec<String> = detail.prereqs.iter().map(|p| p.prereq_id.clone()).collect();
        ids.extend(detail.successors.iter().map(|s| s.successor_id.clone()));
        let names = store.task_names(ids).await.unwrap_or_default();

        let md = render_detail(&detail, &names);
        if let Err(err) = ctx.reply_markdown(md).await {
            tracing::warn!(error = %err, "任务详情发送失败");
        }
        Handled::Consumed
    }

    async fn fetch(&self, url: &str) -> Result<String, String> {
        let res = self
            .http
            .get(url)
            .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) qqbot-rs")
            .send()
            .await
            .map_err(|err| format!("{url} 请求失败：{err}"))?;
        let status = res.status();
        if !status.is_success() {
            return Err(format!("{url} 返回 HTTP {status}"));
        }
        res.text().await.map_err(|err| format!("{url} 读取失败：{err}"))
    }
}

#[async_trait]
impl Handler for TaskPlugin {
    async fn handle(&self, ctx: &Ctx) -> Handled {
        let content = ctx.content().trim();
        let (head, rest) = match content.split_once(char::is_whitespace) {
            Some((head, rest)) => (head, rest.trim()),
            None => (content, ""),
        };
        match head {
            "更新任务" => self.reload(ctx).await,
            "查任务" => self.search(ctx, rest).await,
            _ => Handled::Next,
        }
    }

    fn name(&self) -> &'static str {
        "塔科夫任务"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRADERS: &str = r#"{"data":{"t1":{"normalizedName":"prapor","imageLink":"https://img/prapor.png"},"t2":{"normalizedName":""}}}"#;

    fn tasks_json() -> String {
        concat!(
            r#"{"data":{"tasks":{"#,
            r#""k1":{"name":"k1 name","normalizedName":"gunsmith-part-1","trader":"t1","minPlayerLevel":5,"#,
            r#""kappaRequired":true,"lightkeeperRequired":false,"experience":3000,"#,
            r#""taskImageLink":"https://img/k1.webp","map":"m1","#,
            r#""wikiLink":"https://x/wiki/Gunsmith_-_Part_1","factionName":"BEAR","restartable":true,"#,
            r#""availableDelaySecondsMin":3600,"availableDelaySecondsMax":7200,"#,
            r#""neededKeys":[{"map":"m1","keys":["i9"]}],"#,
            r#""traderRequirements":[{"trader":"t1","requirementType":"level","compareMethod":">=","value":2}],"#,
            r#""objectives":["#,
            r#"{"id":"o1","description":"o1","type":"shoot","optional":false,"count":5,"foundInRaid":true,"#,
            r#""usingWeapon":["i2"],"targetNames":["Savage"],"maps":["m1"],"timeFromHour":21,"timeUntilHour":6},"#,
            r#"{"id":"o2","description":"o2","type":"visit","optional":true}],"#,
            r#""startRewards":{"items":[{"item":"i3","count":20000}]},"#,
            r#""finishRewards":{"traderStanding":[{"trader":"t1","standing":0.1}],"#,
            r#""items":[{"item":"i1","count":80000}],"#,
            r#""skillLevelReward":[{"skill":"Surgery","level":3}],"#,
            r#""craftUnlock":[{"level":2,"station":"s1","item":"i4","count":120}]},"#,
            r#""taskRequirements":[{"task":"k2","status":["complete"]}],"#,
            r#""failConditions":[{"task":"k3","status":["failed"]}]},"#,
            r#""k2":{"name":"k2 name","normalizedName":"first-in-line","trader":"t9","minPlayerLevel":1,"#,
            r#""kappaRequired":false,"lightkeeperRequired":true,"experience":500,"objectives":[]},"#,
            r#""k3":{"name":"k3 name","normalizedName":"third","trader":"t1","minPlayerLevel":1,"objectives":[]},"#,
            r#""k4":{"name":"","normalizedName":"","minPlayerLevel":1}"#,
            r#"}}}"#,
        )
        .to_string()
    }

    fn packs() -> LangPacks {
        let tasks: LangPack = [
            ("k1 name", "枪匠 - 第一部"),
            ("k2 name", "第一梯队"),
            ("k3 name", "第三"),
            ("o1", "击杀 5 个 Scav"),
            ("o2", "探访地点"),
            ("Surgery", "手术"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let items: LangPack = [
            ("i1 Name", "卢布"),
            ("i2 Name", "AKS-74U"),
            ("i3 Name", "初始资金"),
            ("i4 Name", "5.45 BS"),
            ("i9 Name", "宿舍钥匙"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let traders: LangPack = [("t1 Nickname", "大老板")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let maps: LangPack = [("m1 Name", "海关")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        LangPacks { tasks, items, traders, maps }
    }

    fn parsed() -> (Vec<TarkovTaskDetail>, Coverage) {
        parse_tasks(&tasks_json(), TRADERS, &packs()).unwrap()
    }

    fn find<'a>(items: &'a [TarkovTaskDetail], slug: &str) -> &'a TarkovTaskDetail {
        items.iter().find(|d| d.task.normalized_name == slug).unwrap()
    }

    #[test]
    fn resolves_names_maps_and_traders_from_the_language_packs() {
        let (items, coverage) = parsed();
        assert_eq!(items.len(), 3, "没有 slug 的条目要被丢掉: {items:?}");
        assert_eq!(coverage.names_hit, 3);
        assert_eq!(coverage.names_total, 3);
        assert_eq!(coverage.objectives_hit, 2);
        assert_eq!(coverage.objectives_total, 2);
        // k1 有完成奖励 i1 与起始奖励 i3，两个都要能解析出中文。
        assert_eq!(coverage.items_hit, 2);
        assert_eq!(coverage.items_total, 2);
        assert!(coverage.ratio() > 0.99);

        let g = find(&items, "gunsmith-part-1");
        assert_eq!(g.task.name_zh.as_deref(), Some("枪匠 - 第一部"));
        assert_eq!(g.task.name_en.as_deref(), Some("Gunsmith - Part 1"));
        assert_eq!(g.task.trader, "prapor", "商人 id 要换成 slug");
        assert_eq!(g.task.trader_name_zh.as_deref(), Some("大老板"));
        assert_eq!(g.task.map_name_zh.as_deref(), Some("海关"));
        assert!(g.task.updated_at > 0);
    }

    /// v12 的核心：正文与标记在**导入时**就拼好，渲染层不再判断数量与提示。
    #[test]
    fn objectives_are_composed_at_import_time() {
        let (items, _) = parsed();
        let g = find(&items, "gunsmith-part-1");
        assert_eq!(g.objectives.len(), 2);
        // 数量并进正文，标记按 `|` 分隔、顺序稳定。
        assert_eq!(g.objectives[0].display_text, "击杀 5 个 Scav ×5");
        assert_eq!(g.objectives[0].marks, "战局内|21:00-6:00");
        assert_eq!(g.objectives[1].display_text, "探访地点");
        assert_eq!(g.objectives[1].marks, "可选");
    }

    /// 数量只在 >1 时写进正文。
    ///
    /// 实测 1441 条目标里有 768 条的 count 就是 1，「上交文件 ×1」是纯噪音；
    /// 而「消灭 Scav ×25」才是玩家要读的。这条规则属于**数据**，所以它在这里
    /// 而不是在渲染层 —— 换个版式不该改变一句话怎么写。
    #[test]
    fn unit_count_of_one_is_not_written_out() {
        let o = |count| ObjectiveEntry {
            id: "o1".into(),
            description: "o1".into(),
            objective_type: "giveItem".into(),
            optional: false,
            count,
            found_in_raid: false,
            time_from_hour: None,
            time_until_hour: None,
        };
        assert_eq!(objective_view(&o(Some(1)), Some("上交文件")).0, "上交文件");
        assert_eq!(objective_view(&o(Some(25)), Some("消灭 Scav")).0, "消灭 Scav ×25");
        // `None` 是「本来就没有数量」，不是「数量为 0」。
        assert_eq!(objective_view(&o(None), Some("以幸存状态撤离")).0, "以幸存状态撤离");
    }

    /// 标记与数量是两回事：标记**不**并进正文，它由渲染层按版式摆。
    #[test]
    fn objective_marks_are_split_not_baked_into_text() {
        let (items, _) = parsed();
        let g = find(&items, "gunsmith-part-1");
        assert!(!g.objectives[0].display_text.contains("战局内"), "标记不该进正文");
        assert!(!g.objectives[0].display_text.contains("21:00"), "时间窗口也不进正文");
    }

    /// >3 条目标时整段塞进代码块（QQ 客户端可折叠）。
    ///
    /// 代码块里 `>` 不渲染成块引用，所以标记只能改成行内括号 ——
    /// 这正是标记存 `|` 分隔、由渲染层按版式决定写法，
    /// 而不在导入时就写死成 `> …` 的原因。
    #[test]
    fn many_objectives_collapse_into_a_code_block() {
        // 取一份**自有**的详情再改：find 借的是临时 Vec，改不动也活不够久。
        let mut detail = find(&parsed().0, "gunsmith-part-1").clone();
        detail.objectives = (1..=4)
            .map(|i| TarkovObjective {
                objective_type: "visit".into(),
                description_key: format!("o{i}"),
                display_text: format!("第 {i} 件事"),
                marks: format!("标记{i}"),
            })
            .collect();
        let md = render_detail(&detail, &HashMap::new());
        assert!(md.contains("```"), "目标多时要包进代码块:\n{md}");
        assert!(md.contains("1. 第 1 件事（标记1）"), "{md}");
        assert!(md.contains("4. 第 4 件事（标记4）"), "{md}");
        assert!(!md.contains("> 标记1"), "代码块里不该再用块引用:\n{md}");
    }

    /// 全部目标标记相同 → 提到标题下写一次。
    ///
    /// 「小本生意 - 1」有 18 条目标全是「战局内」，逐条重复 18 行纯属噪音；
    /// 折叠起来一样多余，所以上提之后逐条**不再**重复。
    #[test]
    fn identical_marks_are_hoisted_once() {
        let mut detail = find(&parsed().0, "gunsmith-part-1").clone();
        detail.objectives = (1..=4)
            .map(|i| TarkovObjective {
                objective_type: "visit".into(),
                description_key: format!("o{i}"),
                display_text: format!("第 {i} 件事"),
                marks: "战局内".into(),
            })
            .collect();
        let md = render_detail(&detail, &HashMap::new());
        assert!(md.contains("> 全部目标：战局内"), "{md}");
        assert_eq!(md.matches("战局内").count(), 1, "相同的标记只该出现一次:\n{md}");
    }

    /// 3 条及以下保持逐条块引用 —— 折叠是给长列表用的，不该动短列表。
    #[test]
    fn few_objectives_stay_inline_with_blockquotes() {
        let mut detail = find(&parsed().0, "gunsmith-part-1").clone();
        detail.objectives = (1..=3)
            .map(|i| TarkovObjective {
                objective_type: "visit".into(),
                description_key: format!("o{i}"),
                display_text: format!("第 {i} 件事"),
                marks: format!("标记{i}"),
            })
            .collect();
        let md = render_detail(&detail, &HashMap::new());
        assert!(!md.contains("```"), "3 条不该折叠:
{md}");
        assert!(md.contains("> 标记1"), "{md}");
        assert!(md.contains("> 标记3"), "{md}");
    }

    /// 回归：上游技能奖励的字段是 skill，早先读 name 导致名字恒为空。
    #[test]
    fn skill_rewards_read_the_skill_field() {
        let (items, _) = parsed();
        let g = find(&items, "gunsmith-part-1");
        let skill = g.rewards.iter().find(|r| r.kind == "skill").unwrap();
        assert_eq!(skill.ref_id, "Surgery");
        assert_eq!(skill.name_zh.as_deref(), Some("手术"));
        assert!((skill.amount - 3.0).abs() < f64::EPSILON);
    }

    #[test]
    fn rewards_cover_items_standing_unlock_and_start() {
        let (items, _) = parsed();
        let g = find(&items, "gunsmith-part-1");
        let kinds: Vec<&str> = g.rewards.iter().map(|r| r.kind.as_str()).collect();
        for kind in ["standing", "item", "skill", "unlock", "start_item"] {
            assert!(kinds.contains(&kind), "缺少 {kind}: {kinds:?}");
        }
        let item = g.rewards.iter().find(|r| r.kind == "item").unwrap();
        assert_eq!(item.name_zh.as_deref(), Some("卢布"));
        let unlock = g.rewards.iter().find(|r| r.kind == "unlock").unwrap();
        assert!(unlock.extra.as_deref().unwrap().contains("\"level\":2"));
    }

    #[test]
    fn inverts_prerequisites_into_successors() {
        let (items, _) = parsed();
        // k1 的前置是 k2 ⇒ k2 的后续是 k1。
        let k2 = find(&items, "first-in-line");
        assert_eq!(k2.successors.len(), 1);
        assert_eq!(k2.successors[0].successor_id, "k1");
        assert_eq!(k2.successors[0].status, "complete");
        assert!(find(&items, "gunsmith-part-1").successors.is_empty());
    }

    /// 任务级的阵营 / 钥匙 / 门槛 / 失败条件也是类型化的，不走 JSON 往返。
    #[test]
    fn task_metadata_is_typed_not_json() {
        let (items, _) = parsed();
        let g = find(&items, "gunsmith-part-1");
        assert_eq!(g.task.faction.as_deref(), Some("BEAR"));
        assert!(g.task.restartable);
        assert_eq!(g.keys.len(), 1);
        assert_eq!(g.keys[0].map_name, "海关");
        assert_eq!(g.keys[0].keys, "宿舍钥匙", "钥匙名要解析成中文");
        assert_eq!(g.requirements, vec!["大老板 忠诚等级 >= 2".to_string()]);
        assert_eq!(g.fails.len(), 1);
        assert_eq!(g.fails[0].task_name, "第三", "失败条件里的任务要换成名字");
        assert_eq!(g.fails[0].status, "failed");
    }

    #[test]
    fn parse_lang_pack_rejects_broken_input() {
        assert!(parse_lang_pack("不是 JSON").is_err());
        assert!(parse_lang_pack(r#"{"data":null}"#).is_err());
        assert_eq!(parse_lang_pack(r#"{"data":{"a":"b"}}"#).unwrap().get("a").unwrap(), "b");
    }

    #[test]
    fn english_name_prefers_the_wiki_slug() {
        assert_eq!(
            english_name("https://x/wiki/Gunsmith_-_Part_1", "gunsmith-part-1"),
            "Gunsmith - Part 1"
        );
        assert_eq!(english_name("", "the-punisher-part-1"), "The Punisher Part 1");
    }

    #[test]
    fn detail_renders_every_section_that_has_data() {
        let (items, _) = parsed();
        let g = find(&items, "gunsmith-part-1");
        let names: HashMap<String, String> =
            [("k2".to_string(), "第一梯队".to_string())].into_iter().collect();
        let md = render_detail(g, &names);
        for section in [
            "# 枪匠 - 第一部",
            "Gunsmith - Part 1",
            "## 任务目标",
            "击杀 5 个 Scav",
            "> 战局内",
            "## 任务奖励",
            "卢布 ×80000",
            "大老板 声望 +0.1",
            "手术 技能 +3",
            "## 起始奖励",
            "初始资金 ×20000",
            "## 前置任务",
            "（需完成）",
            "## 失败条件",
            "## 需要钥匙",
            "## 商人要求",
            "[维基](",
            "***",
        ] {
            assert!(md.contains(section), "缺少 {section}:\n{md}");
        }
    }

    #[test]
    fn detail_skips_empty_sections() {
        let (items, _) = parsed();
        let k3 = find(&items, "third");
        let md = render_detail(k3, &HashMap::new());
        for absent in ["## 任务目标", "## 任务奖励", "## 前置任务", "## 后续任务"] {
            assert!(!md.contains(absent), "空段不该出现 {absent}:\n{md}");
        }
    }

    /// 前置/后续任务必须是**可点击的指令文本**，不是按钮。
    #[test]
    fn detail_links_are_clickable_command_text() {
        let (items, _) = parsed();
        let k2 = find(&items, "first-in-line");
        let names: HashMap<String, String> =
            [("k1".to_string(), "枪匠 - 第一部".to_string())].into_iter().collect();
        let md = render_detail(k2, &names);
        assert!(md.contains("## 后续任务"), "{md}");
        let want = cmd_input("查任务 枪匠 - 第一部", "枪匠 - 第一部");
        assert!(md.contains(&want), "缺少可点击指令：{md}");
        // 取值必须 urlencode（官方要求），中文与空格都不能原样写进去。
        assert!(want.contains("text=\"%E6%9F%A5%E4%BB%BB%E5%8A%A1%20"), "指令要 urlencode: {want}");
        assert!(want.contains("show=\"%E6%9E%AA%E5%8C%A0"), "{want}");
    }

    #[test]
    fn list_rows_are_clickable_command_text() {
        let (items, _) = parsed();
        let tasks: Vec<TarkovTask> = items.into_iter().map(|d| d.task).collect();
        let md = render_list("惩罚者", &tasks, tasks.len());
        assert!(md.contains(&cmd_input("查任务 枪匠 - 第一部", "枪匠 - 第一部")), "{md}");
        // 等级 0 不显示（实测 282/515 条任务的等级就是 0）。
        assert!(!md.contains("0 级"), "{md}");
        // 没有翻页文案了。
        assert!(!md.contains("第 1/"), "{md}");
    }

    #[test]
    fn command_text_is_urlencoded() {
        assert_eq!(
            url_encode("查任务 惩罚者 - 1"),
            "%E6%9F%A5%E4%BB%BB%E5%8A%A1%20%E6%83%A9%E7%BD%9A%E8%80%85%20-%201"
        );
        assert_eq!(url_encode("abc-_.~"), "abc-_.~", "unreserved 字符不要编码");
    }

    #[test]
    fn amount_and_status_formatting() {
        assert_eq!(format_amount(80000.0), "80000");
        assert_eq!(format_amount(0.1), "0.1", "声望不能显示成 0");
        assert_eq!(signed(0.01), "+0.01");
        assert_eq!(signed(-0.02), "-0.02");
        assert_eq!(status_label("complete"), "需完成");
        assert_eq!(status_label("complete,failed"), "需完成 · 需失败");
        assert_eq!(status_label(""), "");
    }

    #[test]
    fn objective_label_covers_the_common_types_and_passes_through_the_rest() {
        assert_eq!(objective_label("shoot"), "击杀");
        assert_eq!(objective_label("giveItem"), "交付物品");
        assert_eq!(objective_label("brandNewType"), "brandNewType");
    }

    #[test]
    fn broken_input_is_an_error_not_a_panic() {
        assert!(parse_tasks("不是 JSON", TRADERS, &packs()).is_err());
        assert!(parse_tasks(&tasks_json(), "不是 JSON", &packs()).is_err());
        assert!(parse_tasks(r#"{}"#, TRADERS, &packs()).is_err(), "缺 data 要报错");
    }

    /// 实跑验证：打**真实**的静态端点与语言包。
    ///
    ///     cargo test -p qqbot-plugins --lib -- --ignored live_tasks
    #[tokio::test]
    #[ignore = "需要网络"]
    async fn live_tasks_endpoint_parses() {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(90))
            .build()
            .unwrap();
        let plugin = TaskPlugin::new(TaskConfig::default(), http);
        let cfg = TaskConfig::default();
        let (tasks, traders, tasks_zh, items_zh, traders_zh, maps_zh) = tokio::try_join!(
            plugin.fetch(&cfg.tasks_url),
            plugin.fetch(&cfg.traders_url),
            plugin.fetch(&cfg.tasks_zh_url),
            plugin.fetch(&cfg.items_zh_url),
            plugin.fetch(&cfg.traders_zh_url),
            plugin.fetch(&cfg.maps_zh_url),
        )
        .expect("静态端点与语言包都应当可用");

        let packs = LangPacks {
            tasks: parse_lang_pack(&tasks_zh).unwrap(),
            items: parse_lang_pack(&items_zh).unwrap(),
            traders: parse_lang_pack(&traders_zh).unwrap(),
            maps: parse_lang_pack(&maps_zh).unwrap(),
        };
        let (items, coverage) = parse_tasks(&tasks, &traders, &packs).expect("真实数据应当能解析");
        assert!(items.len() > 400, "实测 515 条，解析出 {} 条", items.len());
        assert_eq!(coverage.names_hit, coverage.names_total, "任务名必须全中文");
        assert!(
            coverage.objectives_hit * 100 >= coverage.objectives_total * 98,
            "目标中文覆盖 {}/{}",
            coverage.objectives_hit,
            coverage.objectives_total
        );
        let with_successors = items.iter().filter(|d| !d.successors.is_empty()).count();
        assert!(with_successors > 150, "实测 200 条带后续，实际 {with_successors}");
        assert!(items.iter().all(|d| d.task.name_zh.is_some()), "不该有任务缺中文名");
        assert!(
            items.iter().flat_map(|d| &d.objectives).all(|o| !o.display_text.is_empty()),
            "每条目标都要有正文（语言包缺失时退回通用文案）"
        );
    }

    /// 实跑：把真实数据的「惩罚者 - 1」渲染成人眼可校验的卡片。
    ///
    ///     cargo test -p qqbot-plugins --lib -- --ignored --nocapture live_render_punisher
    #[tokio::test]
    #[ignore = "需要网络"]
    async fn live_render_punisher() {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(90))
            .build()
            .unwrap();
        let plugin = TaskPlugin::new(TaskConfig::default(), http);
        let cfg = TaskConfig::default();
        let (tasks, traders, tasks_zh, items_zh, traders_zh, maps_zh) = tokio::try_join!(
            plugin.fetch(&cfg.tasks_url),
            plugin.fetch(&cfg.traders_url),
            plugin.fetch(&cfg.tasks_zh_url),
            plugin.fetch(&cfg.items_zh_url),
            plugin.fetch(&cfg.traders_zh_url),
            plugin.fetch(&cfg.maps_zh_url),
        )
        .unwrap();
        let packs = LangPacks {
            tasks: parse_lang_pack(&tasks_zh).unwrap(),
            items: parse_lang_pack(&items_zh).unwrap(),
            traders: parse_lang_pack(&traders_zh).unwrap(),
            maps: parse_lang_pack(&maps_zh).unwrap(),
        };
        let (items, _) = parse_tasks(&tasks, &traders, &packs).unwrap();
        let names: HashMap<String, String> = items
            .iter()
            .filter_map(|d| d.task.name_zh.clone().map(|n| (d.task.id.clone(), n)))
            .collect();
        let punisher = items
            .iter()
            .find(|d| d.task.normalized_name == "the-punisher-part-1")
            .expect("应当有 the-punisher-part-1");
        println!("{}", render_detail(punisher, &names));
    }
}
