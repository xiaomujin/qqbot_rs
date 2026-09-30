use minijinja::{AutoEscape, Environment};

use crate::error::RenderError;

/// 内置模板（编译期嵌入，保证单二进制自包含）。
pub const BUILTIN_TEMPLATES: &[(&str, &str)] = &[
    ("card.svg", include_str!("../templates/card.svg")),
    ("notice.svg", include_str!("../templates/notice.svg")),
    ("sections.svg", include_str!("../templates/sections.svg")),
];

/// 极简模板引擎，只做 SVG 文本替换。
///
/// ⚠️ **SVG 必须转义**：它就是 XML，`&`、`<` 不转义才会让解析失败。
/// 这里曾经写着「不做 HTML 转义，否则 `&` 会破坏 XML」，那是反的 ——
/// 症状是数据里出现一个 `&`（比如番剧名 `柔光魔女 & 公司`）就整张卡片渲染不出来。
pub struct TemplateEngine {
    env: Environment<'static>,
}

impl TemplateEngine {
    pub fn new() -> Self {
        let mut env = Environment::new();
        // 只转义 `{{ }}` 里的**值**，模板自身的标签不受影响，
        // 所以像 `{% for %}` 这样的控制结构照常工作。
        env.set_auto_escape_callback(|name| {
            if name.ends_with(".svg") || name.ends_with(".html") {
                AutoEscape::Html
            } else {
                AutoEscape::None
            }
        });
        for (name, src) in BUILTIN_TEMPLATES {
            env.add_template(name, src)
                .expect("内置模板必须能编译");
        }
        Self { env }
    }

    /// 注册额外模板（来自磁盘）。
    pub fn add_template(&mut self, name: &'static str, src: &'static str) -> Result<(), RenderError> {
        self.env.add_template(name, src)?;
        Ok(())
    }

    pub fn template_names(&self) -> Vec<String> {
        self.env.templates().map(|(n, _)| n.to_string()).collect()
    }

    /// 按名渲染。
    pub fn render(&self, name: &str, data: &serde_json::Value) -> Result<String, RenderError> {
        let tpl = self.env.get_template(name)?;
        Ok(tpl.render(data)?)
    }

    /// 直接渲染一段模板源码（用于插件自定义布局）。
    pub fn render_source(&self, source: &str, data: &serde_json::Value) -> Result<String, RenderError> {
        Ok(self.env.render_str(source, data)?)
    }
}

impl Default for TemplateEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 数据里的 `&` 必须转义成 `&amp;`，否则 XML 解析直接失败 ——
    /// 症状是**整张卡片渲染不出来**，而错误只说 `malformed entity reference`。
    #[test]
    fn ampersands_are_escaped_so_the_svg_stays_valid() {
        let e = TemplateEngine::new();
        let svg = e
            .render(
                "card.svg",
                &json!({
                    "title": "A & B",
                    "rows": [{"label": "柔光魔女 & 公司", "value": "第12集"}],
                    "width": 720,
                    "height": 240
                }),
            )
            .unwrap();
        assert!(svg.contains("A &amp; B"), "{svg}");
        assert!(svg.contains("柔光魔女 &amp; 公司"), "{svg}");
        assert!(!svg.contains("A & B"), "不该留下裸 &：{svg}");
    }

    /// 转义同时挡住了标签注入：数据里的 `<` 不能变成真的标签。
    #[test]
    fn angle_brackets_in_data_cannot_inject_markup() {
        let e = TemplateEngine::new();
        let svg = e
            .render(
                "card.svg",
                &json!({
                    "title": "</text><script>alert(1)</script>",
                    "rows": [],
                    "width": 720,
                    "height": 240
                }),
            )
            .unwrap();
        assert!(!svg.contains("<script>"), "数据不能注入标签: {svg}");
    }

    #[test]
    fn builtin_card_renders_plain_values() {
        let e = TemplateEngine::new();
        let svg = e
            .render(
                "card.svg",
                &json!({
                    "title": "签到",
                    "rows": [{"label": "小明", "value": "3 天"}],
                    "width": 720,
                    "height": 240
                }),
            )
            .unwrap();
        assert!(svg.contains("签到"));
        assert!(svg.contains("小明"));
        assert!(svg.starts_with("<svg"));
    }

    /// 这条测试**曾经断言「SVG 不做转义」**，那是错的：
    /// 不转义的 `&` 会让 SVG 解析直接失败，整张卡片渲染不出来。
    /// 保留在这里是为了说明为什么改了行为，而不是删掉了事。
    #[test]
    fn ampersand_and_angle_brackets_are_escaped_in_svg() {
        let e = TemplateEngine::new();
        let svg = e
            .render("notice.svg", &json!({ "title": "A&B", "body": "x<y", "width": 400, "height": 200 }))
            .unwrap();
        assert!(svg.contains("A&amp;B"), "{svg}");
        assert!(svg.contains("x&lt;y"), "{svg}");
        assert!(!svg.contains("A&B"), "裸 & 会让 XML 解析失败: {svg}");
    }

    /// 分段卡片同样必须转义 —— 数据来自第三方接口，里面出现 `&` 完全可能。
    #[test]
    fn sections_template_escapes_and_marks_sections() {
        let e = TemplateEngine::new();
        let svg = e
            .render(
                "sections.svg",
                &json!({
                    "title": "三角洲",
                    "rows": [
                        { "section": true, "label": "密码门" },
                        { "label": "零号大坝", "value": "05&33" },
                    ],
                    "width": 1000,
                    "height": 300,
                }),
            )
            .unwrap();
        assert!(svg.contains("密码门"));
        assert!(svg.contains("05&amp;33"), "分段卡片也要转义: {svg}");
        assert!(!svg.contains("05&33"), "裸 & 会让 XML 解析失败: {svg}");
    }

    /// 缺 `section` 字段时按普通行渲染 —— MiniJinja 里未定义的键是假值。
    #[test]
    fn sections_template_treats_missing_flag_as_a_normal_row() {
        let e = TemplateEngine::new();
        let svg = e
            .render(
                "sections.svg",
                &json!({
                    "title": "t",
                    "rows": [{ "label": "甲", "value": "乙" }],
                    "width": 600,
                    "height": 240,
                }),
            )
            .unwrap();
        assert!(svg.contains("甲"));
        assert!(svg.contains("乙"));
    }

    #[test]
    fn missing_template_is_an_error() {
        let e = TemplateEngine::new();
        assert!(e.render("nope.svg", &json!({})).is_err());
    }
}
