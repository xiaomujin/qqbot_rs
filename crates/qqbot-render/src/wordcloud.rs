//! 词云 SVG 生成。
//!
//! 不依赖模板：布局是算法问题，直接在 Rust 里算好坐标再拼 SVG，
//! 比「模板 + 前端 JS 布局」快一个数量级。

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct WordItem {
    pub text: String,
    pub weight: u32,
}

impl WordItem {
    pub fn new(text: impl Into<String>, weight: u32) -> Self {
        Self { text: text.into(), weight }
    }
}

const PALETTE: &[&str] = &[
    "#38bdf8", "#22d3ee", "#a78bfa", "#f472b6", "#facc15", "#4ade80", "#fb923c", "#e879f9",
];

const MIN_FONT: f32 = 20.0;
const MAX_FONT: f32 = 78.0;
/// 词频映射到透明度的下界。留一点可见度，否则低频词会淡到看不见。
const MIN_ALPHA: f32 = 0.35;
const MAX_ALPHA: f32 = 1.0;
const FONT_STACK: &str = "Microsoft YaHei, SimHei, sans-serif";

/// 标题区高度，布局时避开。
const HEADER_HEIGHT: f32 = 92.0;
/// 画布内边距。
const MARGIN: f32 = 18.0;
/// 词与词之间的额外间隙系数（按字号缩放）。
const GAP_RATIO: f32 = 0.16;

/// 生成词云 SVG。
///
/// 采用**等弧长阿基米德螺线** + 矩形碰撞检测布局：从中心向外扫描，
/// 半径按 \`2π·SPIRAL_A\` 的圈距增长，因此大词居中、小词自然铺开到外围。
/// 放不下的词直接丢弃，保证结果永远可渲染，不会越界或重叠。
pub fn build_wordcloud_svg(words: &[WordItem], width: u32, height: u32, title: &str) -> String {
    let width = width.clamp(240, 2048);
    let height = height.clamp(240, 2048);
    let (fw, fh) = (width as f32, height as f32);

    let mut items: Vec<&WordItem> = words.iter().filter(|w| !w.text.trim().is_empty()).collect();
    // ⚠️ 必须带上 text 作为次级排序键：调用方可能从 HashMap 迭代得到词表，
    // 若仅按权重排序，同权重词的顺序会随机化 → SVG 变化 → 渲染缓存永不命中。
    items.sort_by(|a, b| b.weight.cmp(&a.weight).then_with(|| a.text.cmp(&b.text)));
    items.truncate(80);

    let max_weight = items.iter().map(|w| w.weight).max().unwrap_or(1).max(1) as f32;

    let cx = fw / 2.0;
    let cy = HEADER_HEIGHT + (fh - HEADER_HEIGHT) / 2.0;
    let max_radius = ((fw.min(fh - HEADER_HEIGHT)) / 2.0 - MARGIN).max(20.0);

    let mut placed: Vec<(f32, f32, f32, f32)> = Vec::with_capacity(items.len());
    let mut body = String::new();

    for (index, item) in items.iter().enumerate() {
        // 让「面积 ∝ 词频」：线性维度因此取平方根。
        // 真实词频是长尾分布，若直接线性映射，绝大多数词会挤在最小字号上，
        // 整个词云看起来一片死板。
        let t = (item.weight as f32 / max_weight).sqrt().clamp(0.0, 1.0);
        let font = MIN_FONT + t * (MAX_FONT - MIN_FONT);
        // 透明度复用同一维度：越常出现的词越实，越罕见的越淡。
        // 于是「重要性」有了大小与浓淡两个正交的视觉通道。
        let alpha = MIN_ALPHA + t * (MAX_ALPHA - MIN_ALPHA);
        let text_w = estimate_width(&item.text, font);
        let text_h = font * 1.2;

        if let Some((x, y)) = place(cx, cy, max_radius, fw, fh, text_w, text_h, font, &placed) {
            placed.push((x, y, text_w, text_h));
            let color = PALETTE[index % PALETTE.len()];
            // ⚠️ SVG 的 <text y> 是**基线**而非盒子顶边：盒子顶边是 y，
            // 基线约在 y + font 处。这里必须换算，否则文字会整体下沉一个字高，
            // 造成肉眼可见的重叠。
            let baseline = y + font;
            body.push_str(&format!(
                "  <text x=\"{x:.1}\" y=\"{baseline:.1}\" font-family=\"{FONT_STACK}\" font-size=\"{font:.1}\" font-weight=\"bold\" fill=\"{color}\" fill-opacity=\"{alpha:.2}\">{}</text>\n",
                escape_xml(&item.text)
            ));
        }
    }

    let mut out = String::with_capacity(body.len() + 1024);
    out.push_str("<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"");
    out.push_str(&width.to_string());
    out.push_str("\" height=\"");
    out.push_str(&height.to_string());
    out.push_str("\" viewBox=\"0 0 ");
    out.push_str(&width.to_string());
    out.push(' ');
    out.push_str(&height.to_string());
    out.push_str("\">\n");
    out.push_str("  <defs>\n    <radialGradient id=\"wcbg\" cx=\"50%\" cy=\"45%\" r=\"72%\">\n");
    out.push_str("      <stop offset=\"0%\" stop-color=\"#16213a\"/>\n");
    out.push_str("      <stop offset=\"100%\" stop-color=\"#0b1120\"/>\n");
    out.push_str("    </radialGradient>\n  </defs>\n");
    out.push_str(&format!(
        "  <rect x=\"0\" y=\"0\" width=\"{width}\" height=\"{height}\" rx=\"24\" fill=\"url(#wcbg)\"/>\n"
    ));
    out.push_str(&format!(
        "  <text x=\"32\" y=\"58\" font-family=\"{FONT_STACK}\" font-size=\"30\" font-weight=\"bold\" fill=\"#7dd3fc\">{}</text>\n",
        escape_xml(title)
    ));
    out.push_str(&format!(
        "  <line x1=\"32\" y1=\"78\" x2=\"{}\" y2=\"78\" stroke=\"#334155\" stroke-width=\"2\"/>\n",
        width - 32
    ));
    out.push_str(&body);
    out.push_str("</svg>\n");
    out
}

/// 等弧长阿基米德螺线搜索位置。
///
/// 半径 \`r = A·θ\`，相邻两圈间距约 \`2πA\`；θ 的步长取 \`STEP / r\`，
/// 使相邻候选点的**弧长**基本恒定，从而在大半径处也能均匀采样。
#[allow(clippy::too_many_arguments)]
fn place(
    cx: f32,
    cy: f32,
    max_radius: f32,
    canvas_w: f32,
    canvas_h: f32,
    w: f32,
    h: f32,
    font: f32,
    placed: &[(f32, f32, f32, f32)],
) -> Option<(f32, f32)> {
    /// 每弧度增长的半径 → 圈距 ≈ 2π·A ≈ 75px
    const SPIRAL_A: f32 = 12.0;
    /// 相邻候选点的弧长步长（像素）
    const STEP_PX: f32 = 5.0;
    const MAX_STEPS: usize = 6000;

    let gap = font * GAP_RATIO;

    let mut theta = 0.0f32;
    for _ in 0..MAX_STEPS {
        let r = SPIRAL_A * theta;
        if r > max_radius {
            return None;
        }

        let x = cx + r * theta.cos() - w / 2.0;
        let y = cy + r * theta.sin() - h / 2.0;

        let inside = x >= MARGIN
            && y >= HEADER_HEIGHT
            && x + w <= canvas_w - MARGIN
            && y + h <= canvas_h - MARGIN;

        if inside {
            let rect = (x - gap, y - gap, w + gap * 2.0, h + gap * 2.0);
            if !placed.iter().any(|p| overlaps(*p, rect)) {
                return Some((x, y));
            }
        }

        theta += (STEP_PX / r.max(6.0)).min(0.6);
    }
    None
}

fn overlaps(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> bool {
    a.0 < b.0 + b.2 && a.0 + a.2 > b.0 && a.1 < b.1 + b.3 && a.1 + a.3 > b.1
}

/// 估算文本宽度：CJK 按 1.05 em（粗体略宽），其余按 0.6 em。
fn estimate_width(text: &str, font_size: f32) -> f32 {
    text.chars()
        .map(|c| if is_wide(c) { font_size * 1.05 } else { font_size * 0.6 })
        .sum()
}

fn is_wide(c: char) -> bool {
    matches!(c as u32,
        0x1100..=0x115F
        | 0x2E80..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE6F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x2_0000..=0x3_FFFD
    )
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 从生成的 SVG 中解析出所有 <text> 的位置与字号，用于几何校验。
    fn parse_placed(svg: &str) -> Vec<(f32, f32, f32)> {
        let mut out = Vec::new();
        for chunk in svg.split("<text ").skip(1) {
            let attr = |name: &str| -> Option<f32> {
                let key = format!("{name}=\"");
                let start = chunk.find(&key)? + key.len();
                let rest = &chunk[start..];
                let end = rest.find('"')?;
                rest[..end].parse::<f32>().ok()
            };
            if let (Some(x), Some(y), Some(fs)) = (attr("x"), attr("y"), attr("font-size")) {
                out.push((x, y, fs));
            }
        }
        out
    }

    /// 解析正文词的透明度（标题没有 fill-opacity，天然被排除）。
    fn parse_alpha(svg: &str) -> Vec<f32> {
        svg.split(r#"fill-opacity=""#)
            .skip(1)
            .filter_map(|chunk| chunk[..chunk.find('"')?].parse::<f32>().ok())
            .collect()
    }

    /// 只取正文词（排除标题：标题在 HEADER_HEIGHT 之上）。
    fn body_fonts(svg: &str) -> Vec<f32> {
        parse_placed(svg)
            .into_iter()
            .filter(|(_, y, _)| *y >= HEADER_HEIGHT)
            .map(|(_, _, fs)| fs)
            .collect()
    }

    #[test]
    fn opacity_tracks_frequency() {
        let words = vec![WordItem::new("高频", 100), WordItem::new("低频", 1)];
        let svg = build_wordcloud_svg(&words, 800, 600, "t");
        let alphas = parse_alpha(&svg);
        assert_eq!(alphas.len(), 2, "每个词都应带 fill-opacity");

        let hi = alphas.iter().cloned().fold(f32::MIN, f32::max);
        let lo = alphas.iter().cloned().fold(f32::MAX, f32::min);
        assert!((hi - MAX_ALPHA).abs() < 0.01, "最高频词应完全不透明: {alphas:?}");
        assert!(lo < hi, "低频词应更透明: {alphas:?}");
        assert!(lo >= MIN_ALPHA - 0.01, "透明度不应低于下界: {alphas:?}");
    }

    #[test]
    fn font_uses_sqrt_scaling_so_long_tail_stays_visible() {
        // 面积 ∝ 词频 ⇒ 线性维度 ∝ sqrt(词频)。
        // 若改成线性映射，权重 1/100 的词会直接掉到最小字号。
        let words = vec![WordItem::new("头", 100), WordItem::new("尾", 1)];
        let svg = build_wordcloud_svg(&words, 900, 700, "t");
        let fonts = body_fonts(&svg);
        assert_eq!(fonts.len(), 2);

        let tail = fonts.iter().cloned().fold(f32::MAX, f32::min);
        let expected = MIN_FONT + 0.1 * (MAX_FONT - MIN_FONT); // sqrt(0.01) = 0.1
        assert!(
            (tail - expected).abs() < 1.5,
            "期望 sqrt 映射得到 {expected:.1}，实际 {tail:.1}"
        );
        assert!(tail > MIN_FONT + 1.0, "长尾词不应掉到最小字号");
    }

    #[test]
    fn equal_weights_get_identical_style() {
        let words = vec![WordItem::new("甲", 5), WordItem::new("乙", 5)];
        let svg = build_wordcloud_svg(&words, 800, 600, "t");
        let alphas = parse_alpha(&svg);
        assert_eq!(alphas.len(), 2);
        assert!((alphas[0] - alphas[1]).abs() < 1e-6, "同频词样式应一致: {alphas:?}");
        let fonts = body_fonts(&svg);
        assert!((fonts[0] - fonts[1]).abs() < 1e-6, "同频词字号应一致: {fonts:?}");
    }

    #[test]
    fn builds_svg_with_all_words_when_space_allows() {
        let words = vec![
            WordItem::new("签到", 10),
            WordItem::new("骰子", 8),
            WordItem::new("词云", 6),
        ];
        let svg = build_wordcloud_svg(&words, 800, 600, "群聊词云");
        assert!(svg.starts_with("<svg"));
        assert!(svg.contains("签到"));
        assert!(svg.contains("词云"));
        assert!(svg.contains("群聊词云"));
        assert!(svg.ends_with("</svg>\n"));
    }

    #[test]
    fn escapes_special_characters() {
        let words = vec![WordItem::new("a<b>&c", 1)];
        let svg = build_wordcloud_svg(&words, 400, 300, "t");
        assert!(svg.contains("a&lt;b&gt;&amp;c"));
        assert!(!svg.contains("a<b>&c"));
    }

    #[test]
    fn empty_input_still_produces_valid_svg() {
        let svg = build_wordcloud_svg(&[], 400, 300, "空");
        assert!(svg.contains("</svg>"));
    }

    #[test]
    fn wide_char_detection() {
        assert!(is_wide('中'));
        assert!(!is_wide('a'));
        assert!(estimate_width("中文", 20.0) > estimate_width("ab", 20.0));
    }

    #[test]
    fn layout_stays_inside_canvas_and_avoids_header() {
        let words: Vec<WordItem> = (0..40)
            .map(|i| WordItem::new(format!("词{i:02}"), 40 - i))
            .collect();
        let (w, h) = (900u32, 640u32);
        let svg = build_wordcloud_svg(&words, w, h, "标题");

        // 排除标题本身（它在 HEADER_HEIGHT 之上）
        let placed: Vec<(f32, f32, f32)> = parse_placed(&svg)
            .into_iter()
            .filter(|(_, y, _)| *y >= HEADER_HEIGHT)
            .collect();
        assert!(placed.len() > 5, "应放置多个词，实际 {}", placed.len());

        for (x, y, fs) in &placed {
            // y 已是基线；盒子顶边 = y - fs
            assert!(*y - *fs >= HEADER_HEIGHT - 2.0, "词越过标题区: y={y} fs={fs}");
            assert!(*y <= h as f32 - MARGIN, "词越过下边界: y={y}");
            assert!(*x >= MARGIN - 1.0 && *x < w as f32, "词越过左右边界: x={x}");
        }
    }

    #[test]
    fn placed_boxes_do_not_overlap() {
        let words: Vec<WordItem> = (0..30)
            .map(|i| WordItem::new(format!("词{i:02}"), 30 - i))
            .collect();
        let svg = build_wordcloud_svg(&words, 900, 640, "t");
        let placed: Vec<(f32, f32, f32)> = parse_placed(&svg)
            .into_iter()
            .filter(|(_, y, _)| *y >= HEADER_HEIGHT)
            .collect();

        let boxes: Vec<(f32, f32, f32, f32)> = placed
            .iter()
            .map(|(x, y, fs)| (*x, y - fs, estimate_width("词00", *fs), fs * 1.2))
            .collect();

        for i in 0..boxes.len() {
            for j in (i + 1)..boxes.len() {
                assert!(!overlaps(boxes[i], boxes[j]), "词 {i} 与 {j} 重叠: {:?} {:?}", boxes[i], boxes[j]);
            }
        }
    }

    #[test]
    fn layout_is_deterministic_regardless_of_input_order() {
        let words: Vec<WordItem> = ["甲", "乙", "丙", "丁", "戊", "己"]
            .into_iter()
            .map(|t| WordItem::new(t, 5))
            .collect();
        let mut shuffled = words.clone();
        shuffled.reverse();

        assert_eq!(
            build_wordcloud_svg(&words, 800, 600, "t"),
            build_wordcloud_svg(&shuffled, 800, 600, "t"),
            "同权重词的顺序必须稳定，否则渲染缓存永不命中"
        );
    }

    #[test]
    fn bigger_weight_gets_bigger_font_and_center_placement() {
        let words = vec![WordItem::new("大词", 100), WordItem::new("小词", 1)];
        let svg = build_wordcloud_svg(&words, 800, 600, "t");
        let placed = parse_placed(&svg);
        let big = placed.iter().find(|(_, _, fs)| *fs > 60.0);
        assert!(big.is_some(), "权重最高的词应获得最大字号: {placed:?}");
    }
}
