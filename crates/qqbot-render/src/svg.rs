use crate::error::RenderError;

/// 单张渲染结果。
#[derive(Debug, Clone)]
pub struct RenderedImage {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

impl RenderedImage {
    pub fn len(&self) -> usize {
        self.png.len()
    }

    pub fn is_empty(&self) -> bool {
        self.png.is_empty()
    }
}

/// 画布单边上限，防止模板写出天文数字导致 OOM。
pub const MAX_DIMENSION: u32 = 4096;

/// 基于 `resvg` 的纯 Rust SVG → PNG 渲染器。
///
/// 无 Chromium、无 Node：常驻内存约几 MB，单张 5–20ms。
pub struct SvgRenderer {
    options: usvg::Options<'static>,
}

impl SvgRenderer {
    pub fn new() -> Self {
        let mut options = usvg::Options::default();
        {
            let db = options.fontdb_mut();
            // usvg 在 system-fonts feature 下可能已加载；仅在为空时加载，避免重复开销。
            if db.is_empty() {
                db.load_system_fonts();
            }
        }
        // 中文字体优先，避免 SVG 里未指定 font-family 时中文变方块。
        options.font_family =
            "Microsoft YaHei, SimHei, Noto Sans CJK SC, WenQuanYi Micro Hei, sans-serif".to_string();
        Self { options }
    }

    /// 已加载的字体数量（用于启动自检）。
    pub fn font_count(&self) -> usize {
        self.options.fontdb.len()
    }

    /// 渲染 SVG 字符串为 PNG。
    ///
    /// `scale` 用于输出高清图（建议 2.0），等价于 deviceScaleFactor。
    pub fn render_png(&self, svg: &str, scale: f32) -> Result<RenderedImage, RenderError> {
        let tree = usvg::Tree::from_str(svg, &self.options)
            .map_err(|e| RenderError::Svg(e.to_string()))?;

        let size = tree.size().to_int_size();
        let scale = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };

        let width = ((size.width() as f32) * scale).round().max(1.0) as u32;
        let height = ((size.height() as f32) * scale).round().max(1.0) as u32;

        if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
            return Err(RenderError::BadSize { width, height });
        }

        // tiny_skia 由 resvg 再导出，无需单独依赖。
        let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height)
            .ok_or(RenderError::BadSize { width, height })?;

        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::from_scale(scale, scale),
            &mut pixmap.as_mut(),
        );

        let png = pixmap
            .encode_png()
            .map_err(|e| RenderError::Encode(e.to_string()))?;

        Ok(RenderedImage { png, width, height })
    }
}

impl Default for SvgRenderer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIMPLE: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="50"><rect width="100" height="50" fill="#ff0000"/></svg>"##;

    #[test]
    fn renders_png_with_expected_size() {
        let r = SvgRenderer::new();
        let img = r.render_png(SIMPLE, 1.0).unwrap();
        assert_eq!((img.width, img.height), (100, 50));
        // PNG 魔数
        assert_eq!(&img.png[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
    }

    #[test]
    fn scale_doubles_output_size() {
        let r = SvgRenderer::new();
        let img = r.render_png(SIMPLE, 2.0).unwrap();
        assert_eq!((img.width, img.height), (200, 100));
    }

    #[test]
    fn rejects_oversized_canvas() {
        let r = SvgRenderer::new();
        let big = r##"<svg xmlns="http://www.w3.org/2000/svg" width="9000" height="9000"></svg>"##;
        assert!(matches!(r.render_png(big, 1.0), Err(RenderError::BadSize { .. })));
    }

    #[test]
    fn malformed_svg_is_an_error_not_a_panic() {
        let r = SvgRenderer::new();
        assert!(r.render_png("<svg", 1.0).is_err());
    }

    #[test]
    fn renders_text_with_system_fonts() {
        // 中文文本渲染：确保字体库可用（Windows 上应能加载系统字体）。
        let r = SvgRenderer::new();
        assert!(r.font_count() > 0, "未加载到任何系统字体");
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="200" height="60">
            <text x="10" y="40" font-size="30" font-family="Microsoft YaHei">你好世界</text></svg>"#;
        let img = r.render_png(svg, 1.0).unwrap();
        assert!(img.png.len() > 100);
    }
}
