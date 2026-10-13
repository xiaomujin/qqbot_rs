use serde_json::{json, Value};

/// 卡片默认宽度。
///
/// 曾经四个插件各写一个（980 / 900 / 1000 / 1000）—— 宽度不影响行高，
/// 所以谁也没错，只是同一件事有四种写法。统一之后卡片外观会变，
/// 这是有意接受的（见 docs/adr/0001-plugin-outbound-refactor.md 第 7 条）。
pub const CARD_DEFAULT_WIDTH: u32 = 1000;

// ---- 几何常量 ----
//
// 这四个数字就是 card.svg 与 sections.svg 里写死的排版：
//
// | 常量 | 模板里的位置 |
// |---|---|
// | 首行 y = 156 | `{{ 156 + loop.index0 * 48 }}` |
// | 行距 48 | 同上，`loop.index0 * 48` |
// | 页脚余量 40 | 页脚在 `height - 24`，比最后一行再低 16px |
//
// 两个模板**共用同一套几何**（sections.svg 里的小标题也占一行，所以它的
// `loop.index0` 与普通行是同一个基准）。原来这四个数字在 4 个插件里各写了一遍，
// 改模板的人不一定会去改插件 —— 于是模板改了、插件没改，页脚就压住最后一行，
// 而且照样渲染成功，只是难看。
//
// ⚠️ 改这里必须同步改 templates/*.svg 里的字面量：模板是数据，常量是代码，
// 编译器连不到一起。

/// 第一行基线的 y 坐标。
pub const CARD_FIRST_ROW_Y: u32 = 156;
/// 行距（每多一行增加的高度）。
pub const CARD_ROW_HEIGHT: u32 = 48;
/// 最后一行到卡片底边的余量，用来给页脚（`height - 24`）腾地方。
pub const CARD_FOOTER_MARGIN: u32 = 40;

/// 卡片上的一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CardRow {
    /// 普通行：左边标签，右边数值。
    Item { label: String, value: String },
    /// 分段小标题：独占一行，没有右值。用它的卡片走 `sections.svg`。
    Section { label: String },
}

impl CardRow {
    /// 左标签（两种行都有）。
    pub fn label(&self) -> &str {
        match self {
            CardRow::Item { label, .. } | CardRow::Section { label } => label,
        }
    }

    /// 右值；小标题没有右值，返回 `None`。
    pub fn value(&self) -> Option<&str> {
        match self {
            CardRow::Item { value, .. } => Some(value),
            CardRow::Section { .. } => None,
        }
    }

    /// 是否是分段小标题。
    fn is_section(&self) -> bool {
        matches!(self, CardRow::Section { .. })
    }

    /// 转成模板要的行对象。
    ///
    /// `sections.svg` 靠 `row.section` 的真假决定画成小标题还是普通行；
    /// 普通行**不写** `section` 键 —— MiniJinja 里未定义的键就是假值。
    fn to_value(&self) -> Value {
        match self {
            CardRow::Item { label, value } => json!({ "label": label, "value": value }),
            CardRow::Section { label } => json!({ "section": true, "label": label }),
        }
    }
}

/// 一张卡片：标题 + 若干行 + 可选页脚 + 宽度。
///
/// 调用方只管内容，几何（行距、页脚位置、用哪个模板）全在这里 ——
/// 这是本次重构要收拢的东西：原来每个调用点都得知道首行 y=156、行距 48、
/// 页脚在 `height-24` 才能把高度算对。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card {
    /// 卡片标题。
    pub title: String,
    /// 内容行，按插入顺序渲染。
    pub rows: Vec<CardRow>,
    /// 页脚；`None` 表示用模板默认值。
    pub footer: Option<String>,
    /// 卡片宽度（SVG 用户坐标）。
    pub width: u32,
}

impl Card {
    /// 新卡片：默认宽度、无页脚（页脚交给模板的 `default("qqbot")`）。
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            rows: Vec::new(),
            footer: None,
            width: CARD_DEFAULT_WIDTH,
        }
    }

    /// 追加一行「左标签 + 右值」。
    pub fn row(self, label: impl Into<String>, value: impl Into<String>) -> Self {
        self.push_row(CardRow::Item { label: label.into(), value: value.into() })
    }

    /// 追加一个分段小标题。
    ///
    /// 只要有一行是 Section，整张卡片就换成 `sections.svg` ——
    /// 调用方不需要再自己选模板名。
    pub fn section(self, label: impl Into<String>) -> Self {
        self.push_row(CardRow::Section { label: label.into() })
    }

    /// 覆盖页脚。不设则走模板默认值。
    pub fn footer(self, text: impl Into<String>) -> Self {
        Self { footer: Some(text.into()), ..self }
    }

    /// 覆盖宽度。
    pub fn width(self, width: u32) -> Self {
        Self { width, ..self }
    }

    /// 卡片高度（SVG 用户坐标）。
    ///
    /// 输出像素是这个值再乘渲染倍率（默认 2.0），所以断言尺寸时记得乘。
    /// 用 `saturating_*` 是为了「算不出来也不要 panic」——
    /// 行数多到回绕的卡片本来就渲染不出来，但让它在这里炸掉会连累整条消息链。
    pub fn height(&self) -> u32 {
        CARD_FIRST_ROW_Y
            .saturating_add((self.rows.len() as u32).saturating_mul(CARD_ROW_HEIGHT))
            .saturating_add(CARD_FOOTER_MARGIN)
    }

    /// 该用哪个模板：有分段行就 `sections.svg`，否则 `card.svg`。
    pub fn template(&self) -> &'static str {
        if self.has_section() {
            "sections.svg"
        } else {
            "card.svg"
        }
    }

    /// 没有内容行。调用点用它决定「上游没数据」的回复，
    /// 免得渲染出一张只有标题和页脚的空卡片。
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// 卡片数据。
    ///
    /// `footer` 为 `None` 时**不放**这个键：模板里写的是
    /// `{{ footer | default("qqbot") }}`，塞一个 `null` 进去不会走 default，
    /// 页脚会变成空串。
    pub fn to_data(&self) -> Value {
        let mut data = json!({
            "title": self.title,
            "rows": self.rows.iter().map(CardRow::to_value).collect::<Vec<_>>(),
            "width": self.width,
            "height": self.height(),
        });
        if let Some(footer) = &self.footer
            && let Some(obj) = data.as_object_mut()
        {
            obj.insert("footer".to_string(), Value::String(footer.clone()));
        }
        data
    }

    fn push_row(mut self, row: CardRow) -> Self {
        self.rows.push(row);
        self
    }

    fn has_section(&self) -> bool {
        self.rows.iter().any(CardRow::is_section)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 高度公式：156（首行）+ 行数 × 48（行距）+ 40（页脚余量）。
    #[test]
    fn height_counts_the_first_row_the_gaps_and_the_footer() {
        // 空卡片仍然是标题 + 分割线 + 页脚那么高，不是 0。
        assert_eq!(Card::new("t").height(), CARD_FIRST_ROW_Y + CARD_FOOTER_MARGIN);
        assert_eq!(Card::new("t").height(), 196);

        assert_eq!(Card::new("t").row("甲", "1").height(), 156 + 48 + 40);
        assert_eq!(Card::new("t").row("甲", "1").height(), 244);

        assert_eq!(
            Card::new("t").row("甲", "1").section("段").row("乙", "2").height(),
            156 + 3 * 48 + 40
        );
        assert_eq!(
            Card::new("t").row("甲", "1").section("段").row("乙", "2").height(),
            340
        );
    }

    /// 分段行决定模板：有 Section 就必须走 sections.svg，
    /// 否则小标题会被当成「左标签 + 右值」画出来。
    #[test]
    fn template_follows_the_presence_of_a_section() {
        assert_eq!(Card::new("t").template(), "card.svg");
        assert_eq!(Card::new("t").row("甲", "1").template(), "card.svg");
        assert_eq!(Card::new("t").section("段").template(), "sections.svg");
        assert_eq!(Card::new("t").row("甲", "1").section("段").template(), "sections.svg");
    }

    #[test]
    fn is_empty_only_when_there_are_no_rows() {
        assert!(Card::new("t").is_empty());
        assert!(!Card::new("t").row("甲", "1").is_empty());
        // 只有小标题也算有内容 —— 调用点不必区分两种行。
        assert!(!Card::new("t").section("段").is_empty());
    }

    /// 字段形状：模板靠这些键取值。
    #[test]
    fn to_data_carries_title_rows_and_geometry() {
        let data = Card::new("标题").row("甲", "1").section("段").to_data();
        assert_eq!(data["title"], "标题");
        assert_eq!(data["width"], CARD_DEFAULT_WIDTH);
        assert_eq!(data["height"], 156 + 2 * 48 + 40);

        let rows = data["rows"].as_array().expect("rows 必须是数组");
        assert_eq!(rows.len(), 2);
        // 普通行：label + value，**不写** section 键（未定义即假值）。
        assert_eq!(rows[0]["label"], "甲");
        assert_eq!(rows[0]["value"], "1");
        assert!(rows[0].get("section").is_none(), "普通行不该带 section: {}", rows[0]);
        // 分段行：section 为真，且没有 value。
        assert_eq!(rows[1]["section"], true);
        assert_eq!(rows[1]["label"], "段");
        assert!(rows[1].get("value").is_none(), "小标题不该有右值: {}", rows[1]);
    }

    /// footer 为 None 时**不含**这个键 —— 否则模板的 `default("qqbot")` 不生效，
    /// 页脚会渲染成空串。
    #[test]
    fn to_data_omits_the_footer_key_until_it_is_set() {
        let data = Card::new("t").to_data();
        assert!(
            data.get("footer").is_none(),
            "不设页脚时不能放 footer 键，否则模板 default 失效: {data}"
        );

        let with = Card::new("t").footer("共 3 条").to_data();
        assert_eq!(with["footer"], "共 3 条");
    }

    #[test]
    fn width_overrides_the_default() {
        assert_eq!(Card::new("t").width, CARD_DEFAULT_WIDTH);
        let card = Card::new("t").width(720);
        assert_eq!(card.width, 720);
        assert_eq!(card.to_data()["width"], 720);
        assert_eq!(card.height(), Card::new("t").height(), "宽度不影响高度");
    }

    #[test]
    fn row_accessors_expose_both_shapes() {
        let item = CardRow::Item { label: "甲".into(), value: "1".into() };
        assert_eq!(item.label(), "甲");
        assert_eq!(item.value(), Some("1"));

        let section = CardRow::Section { label: "段".into() };
        assert_eq!(section.label(), "段");
        assert_eq!(section.value(), None, "小标题没有右值");
    }
}
