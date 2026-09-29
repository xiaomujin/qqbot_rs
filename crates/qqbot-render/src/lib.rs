//! SVG → PNG 渲染子系统（纯 Rust，无 Chromium / 无 Node）。
//!
//! 设计要点：
//! - [`SvgRenderer`]：`usvg` 解析 + `resvg` 光栅化 + `tiny-skia` 输出 PNG
//! - [`TemplateEngine`]：内置 SVG 模板，编译期嵌入，单二进制自包含
//! - [`RenderService`]：有界队列 + 并发上限 + 超时 + 结果缓存
//! - [`wordcloud`]：布局算法直接产出 SVG

pub mod error;
pub mod service;
pub mod svg;
pub mod template;
pub mod wordcloud;

pub use error::RenderError;
pub use service::{RenderConfig, RenderRequest, RenderService, RenderSource, RenderStats};
pub use svg::{RenderedImage, SvgRenderer, MAX_DIMENSION};
pub use template::{TemplateEngine, BUILTIN_TEMPLATES};
pub use wordcloud::{build_wordcloud_svg, WordItem};
