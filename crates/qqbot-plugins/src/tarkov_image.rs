//! 塔科夫静态图（B9–B15）。
//!
//! 12 张地图 + 7 张速查图，文件名与 cq-bot 的 `TarKovMapPlugin` 一一对应。
//!
//! ## 为什么不走资源系统
//!
//! 资源的关键词是**整条消息精确相等**，而 `系统收录` 又按空格切分参数 ——
//! 所以 cq-bot 的 `地图 海关` 这种带空格的形式**根本存不进关键词表**。
//! cq-bot 自己是对整条消息做 `contains("地图")` + `contains("海关")`，
//! 也就是**子串**匹配。
//!
//! 这里按它的原样实现，但把匹配收紧成**三种确定写法**（见 [`resolve`]），
//! 而不是子串 —— 子串会让「今天海关真难打」这种闲聊也发图。
//!
//! ## 为什么直接读目录
//!
//! 与 cq-bot 一致：它读 `Constant.BASE_IMG_PATH + "tarkov_map/"`。
//! 这批图是**固定的 19 个**，不是用户随时增删的素材，
//! 走资源系统只会让用户多敲 19 次 `系统收录`。

use std::path::PathBuf;

use async_trait::async_trait;
use qqbot_core::{Ctx, Handled, Handler};
use qqbot_media::FileType;

/// 地图名 → 文件名。照抄 cq-bot 的 `keywordToImageMap`。
///
/// ⚠️ 顺序不要动：`疗养院` 与 `海岸线` 是两个不同的文件（`ShorelineHose.jpg`
/// 与 `Shoreline.jpg`），cq-bot 那边就是这么分的，别「顺手合并」。
pub const MAPS: &[(&str, &str)] = &[
    ("储备站", "Reserve.jpg"),
    ("灯塔", "Lighthouse.jpg"),
    ("工厂", "Factory.jpg"),
    ("海岸线", "Shoreline.jpg"),
    ("海关", "Customs.jpg"),
    ("街区", "StreetsOfTarKov.jpg"),
    ("立交桥", "Interchange.jpg"),
    ("森林", "Woods.jpg"),
    ("实验室", "TheLab.jpg"),
    ("疗养院", "ShorelineHose.jpg"),
    ("中心区", "Center.jpg"),
    ("迷宫", "Maze.jpg"),
];

/// 命令 → 文件名。照抄 cq-bot 的各个分支。
///
/// **少了 cq-bot 的 `boss刷新率`。** 那张 `bossRefreshRate.png` 是张静态图，
/// 而 B7 能给出**实时**的刷新率（`json.tarkov.dev` 的 `spawnChance`），
/// 静态图没有存在意义 —— 所以 `boss刷新率` 让给实时数据，
/// 那张图不再使用（文件留着不碍事）。
pub const STATIC: &[(&str, &str)] = &[
    ("任务流程图", "TaskProcess.jpg"),
    ("任务物品图", "TaskItem.png"),
    ("信誉栏位图", "reputation.png"),
    ("boss丢包时间", "bossLossWrap.png"),
    ("3x4道具", "3x4.png"),
    ("耳机强度", "headset.png"),
];

/// 解析出要发的文件名。不是本插件的命令就返回 `None`。
///
/// 地图认三种写法：
///
/// | 写法 | 例 |
/// |---|---|
/// | `地图 <名>` | `地图 海关`（cq-bot 原样） |
/// | `地图<名>` | `地图海关` |
/// | `<名>地图` | `海关地图` |
///
/// 另外 7 个是整串精确匹配。
///
/// ## `boss刷新率` 不在这里
///
/// cq-bot 在这件事上**自相矛盾**：它的实时刷新率正则 `^(?i)(boss(刷|概))`
/// 会连 `boss刷新率` 一起吞掉，而 `TarKovMapPlugin` 又用
/// `startsWith("boss刷新率")` 发静态图 —— 谁先跑谁赢。
///
/// 这里**不做静态图**：B7 能给出实时的 `spawnChance`，
/// 一张静态图没有存在意义。所以 `boss刷新率` / `boss刷` / `boss概`
/// 全部交给 B7 的实时数据，本插件的 `resolve` 对它们一律返回 `None`。
///
/// **要求「地图」两个字真的出现**，而不是像 cq-bot 那样只看名字 ——
/// 否则群里说一句「海关」就会蹦出一张图。
pub fn resolve(content: &str) -> Option<&'static str> {
    let content = content.trim();
    if let Some((_, file)) = STATIC.iter().find(|(cmd, _)| *cmd == content) {
        return Some(file);
    }

    // 先剥前缀再剥后缀；两个都没有就说明这条消息与地图无关。
    let name = content
        .strip_prefix("地图")
        .or_else(|| content.strip_suffix("地图"))
        .map(str::trim)?;
    MAPS.iter().find(|(n, _)| *n == name).map(|(_, file)| *file)
}

/// 塔科夫静态图插件。
#[derive(Clone)]
pub struct TarkovImagePlugin {
    dir: PathBuf,
}

impl TarkovImagePlugin {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }
}

#[async_trait]
impl Handler for TarkovImagePlugin {
    async fn handle(&self, ctx: &Ctx) -> Handled {
        let Some(file) = resolve(ctx.content()) else {
            return Handled::Next;
        };
        let path = self.dir.join(file);
        let bytes = match tokio::fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(err) => {
                // 把路径打出来：这类失败几乎都是配置的目录不对，
                // 只说「读取失败」用户无从下手。
                tracing::warn!(error = %err, path = %path.display(), "读取塔科夫静态图失败");
                let _ = ctx
                    .reply_text(format!("图片读取失败：{}（{}）", err, path.display()))
                    .await;
                return Handled::Consumed;
            }
        };
        if let Err(err) = ctx.reply_media(FileType::Image, file, &bytes).await {
            tracing::warn!(error = %err, hint = err.hint().unwrap_or("-"), file, "塔科夫静态图发送失败");
        }
        Handled::Consumed
    }

    fn name(&self) -> &'static str {
        "塔科夫静态图"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_the_three_map_forms() {
        // cq-bot 的原样写法，带空格。
        assert_eq!(resolve("地图 海关"), Some("Customs.jpg"));
        assert_eq!(resolve("地图海关"), Some("Customs.jpg"));
        assert_eq!(resolve("海关地图"), Some("Customs.jpg"));
        // 名字里的空格无所谓。
        assert_eq!(resolve("地图   海关  "), Some("Customs.jpg"));
    }

    #[test]
    fn covers_every_map_in_the_table() {
        for (name, file) in MAPS {
            assert_eq!(resolve(&format!("地图 {name}")), Some(*file), "地图 {name}");
            assert_eq!(resolve(&format!("{name}地图")), Some(*file), "{name}地图");
        }
        assert_eq!(MAPS.len(), 12, "cq-bot 的 keywordToImageMap 是 12 条");
    }

    #[test]
    fn shoreline_and_sanatorium_are_different_files() {
        // 两个名字看着像，文件却是两个 —— 别「顺手合并」。
        assert_eq!(resolve("地图 海岸线"), Some("Shoreline.jpg"));
        assert_eq!(resolve("地图 疗养院"), Some("ShorelineHose.jpg"));
    }

    #[test]
    fn covers_every_static_command() {
        for (cmd, file) in STATIC {
            assert_eq!(resolve(cmd), Some(*file), "{cmd}");
        }
        assert_eq!(STATIC.len(), 6, "cq-bot 那张 bossRefreshRate.png 不再使用");
    }

    /// `boss刷新率` 归 B7 的实时数据，本插件不该截走它。
    #[test]
    fn boss_spawn_rate_is_left_to_the_live_data() {
        for cmd in ["boss刷新率", "boss刷", "boss概", "boss概率", "boss概览"] {
            assert_eq!(resolve(cmd), None, "{cmd} 应当交给 B7");
        }
    }

    #[test]
    fn bare_map_name_does_not_trigger() {
        // cq-bot 是子串匹配，「海关」两个字单独出现也会发图；
        // 那在群里太吵，这里要求「地图」真的出现。
        assert_eq!(resolve("海关"), None);
        assert_eq!(resolve("今天海关真难打"), None);
    }

    #[test]
    fn lookalikes_are_rejected() {
        assert_eq!(resolve("地图"), None, "只有前缀不该发图");
        assert_eq!(resolve("地图 不存在的地方"), None);
        assert_eq!(resolve("任务流程图 "), Some("TaskProcess.jpg"), "首尾空白要忽略");
        assert_eq!(resolve("任务流程"), None, "少一个字不算");
        assert_eq!(resolve(""), None);
    }

    #[test]
    fn every_file_name_is_flat() {
        // 文件名会被直接 join 到目录上，带分隔符就能跳出配置的目录。
        for (_, file) in MAPS.iter().chain(STATIC) {
            assert!(!file.contains('/') && !file.contains('\\'), "{file} 不该带路径");
            assert!(!file.contains(".."), "{file} 不该带 ..");
        }
    }
}
