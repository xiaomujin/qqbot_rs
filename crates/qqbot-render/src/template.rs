use minijinja::{AutoEscape, Environment};

use crate::error::RenderError;

/// 内置模板（编译期嵌入，保证单二进制自包含）。
pub const BUILTIN_TEMPLATES: &[(&str, &str)] = &[
    ("card.svg", include_str!("../templates/card.svg")),
    ("notice.svg", include_str!("../templates/notice.svg")),
];

/// 极简模板引擎，只做 SVG 文本替换。
///
/// 注意：SVG 模板**不做 HTML 转义**，否则 `&` 之类会破坏 XML。
pub struct TemplateEngine {
    env: Environment<'static>,
}

impl TemplateEngine {
    pub fn new() -> Self {
        let mut env = Environment::new();
        env.set_auto_escape_callback(|name| {
            if name.ends_with(".html") {
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

    #[test]
    fn builtin_card_renders_without_html_escaping() {
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

    #[test]
    fn ampersand_is_not_escaped_in_svg() {
        let e = TemplateEngine::new();
        let svg = e
            .render("notice.svg", &json!({ "title": "A&B", "body": "x<y", "width": 400, "height": 200 }))
            .unwrap();
        assert!(svg.contains("A&B"), "SVG 模板不应做 HTML 转义: {svg}");
    }

    #[test]
    fn missing_template_is_an_error() {
        let e = TemplateEngine::new();
        assert!(e.render("nope.svg", &json!({})).is_err());
    }
}
