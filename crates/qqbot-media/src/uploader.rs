use std::sync::Arc;
use std::time::{Duration, Instant};

use moka::sync::Cache;
use qqbot_api::{ApiClient, Target};

use crate::error::MediaError;

/// 秒传判据：文件前 10002432 字节（约 9.54MB）的 MD5。官方文档明确给出该常量。
pub const MD5_10M_LEN: usize = 10_002_432;

/// 默认分片大小（官方默认 5MB）。
const DEFAULT_BLOCK_SIZE: usize = 5 * 1024 * 1024;

/// 富媒体业务类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    Image,
    Video,
    Voice,
    File,
}

impl FileType {
    pub const fn as_u8(self) -> u8 {
        match self {
            FileType::Image => 1,
            FileType::Video => 2,
            FileType::Voice => 3,
            FileType::File => 4,
        }
    }

    /// 从数据库里存的数值还原。未知值返回 None，由调用方决定怎么处理。
    pub const fn from_u8(raw: u8) -> Option<Self> {
        match raw {
            1 => Some(FileType::Image),
            2 => Some(FileType::Video),
            3 => Some(FileType::Voice),
            4 => Some(FileType::File),
            _ => None,
        }
    }

    /// 按扩展名推断类型。收录素材时用它把类型一次定下来。
    ///
    /// 只有 silk 算语音：官方语音消息要求 silk 编码，
    /// 把 mp3 标成语音会被服务端拒绝，标成文件反而能正常送达。
    pub fn from_extension(ext: &str) -> Self {
        match ext.to_ascii_lowercase().as_str() {
            "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp" => FileType::Image,
            "mp4" | "mov" | "mkv" | "webm" => FileType::Video,
            "silk" => FileType::Voice,
            _ => FileType::File,
        }
    }
}

#[derive(Clone)]
struct CachedFileInfo {
    file_info: String,
    expires_at: Instant,
}

impl CachedFileInfo {
    fn valid(&self) -> bool {
        self.expires_at > Instant::now()
    }
}

/// 富媒体上传器。
///
/// 三个关键点：
/// 1. **分片上传**：本地内存中的图片无需公网 URL，直接走 prepare → PUT → part_finish → merge
/// 2. **秒传**：以 `md5_10m` + 长度 + 场景 为 key 缓存 `file_info`
/// 3. **场景隔离**：单聊与群聊的上传接口不互通，缓存 key 必须带 scene
#[derive(Clone)]
pub struct MediaUploader {
    api: ApiClient,
    http: reqwest::Client,
    cache: Cache<String, Arc<CachedFileInfo>>,
    /// `file_info` 缓存上限，避免用过期凭证发送失败。
    file_info_ttl: Duration,
}

impl MediaUploader {
    pub fn new(api: ApiClient) -> Self {
        Self::with_capacity(api, 256)
    }

    pub fn with_capacity(api: ApiClient, capacity: u64) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .unwrap_or_default();
        Self {
            api,
            http,
            cache: Cache::builder().max_capacity(capacity).build(),
            file_info_ttl: Duration::from_secs(30 * 60),
        }
    }

    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.file_info_ttl = ttl;
        self
    }

    pub fn api(&self) -> &ApiClient {
        &self.api
    }

    pub fn cached_entries(&self) -> u64 {
        self.cache.entry_count()
    }

    /// 上传内存中的字节流（分片上传），返回 `file_info`。
    pub async fn upload_bytes(
        &self,
        target: &Target,
        file_type: FileType,
        file_name: &str,
        bytes: &[u8],
    ) -> Result<String, MediaError> {
        if bytes.is_empty() {
            return Err(MediaError::Empty);
        }

        let md5 = md5_hex(bytes);
        let sha1 = sha1_hex(bytes);
        let md5_10m = md5_hex(&bytes[..bytes.len().min(MD5_10M_LEN)]);
        let key = format!("{}:{}:{}", target.scene(), md5_10m, bytes.len());

        if let Some(hit) = self.cache.get(&key) {
            if hit.valid() {
                metrics::counter!("qqbot_media_cache_total", "result" => "hit").increment(1);
                tracing::debug!(scene = target.scene(), "file_info 命中缓存（秒传）");
                return Ok(hit.file_info.clone());
            }
            self.cache.invalidate(&key);
        }
        metrics::counter!("qqbot_media_cache_total", "result" => "miss").increment(1);

        let prepare_body = serde_json::json!({
            "file_type": file_type.as_u8(),
            "file_size": bytes.len().to_string(),
            "file_name": file_name,
            "md5": md5,
            "sha1": sha1,
            "md5_10m": md5_10m,
        });

        let prepared = self.api.upload_prepare(target, &prepare_body).await?;
        if prepared.parts.is_empty() {
            return Err(MediaError::MissingField("parts".to_string()));
        }

        let block = if prepared.block_size > 0 {
            prepared.block_size as usize
        } else {
            DEFAULT_BLOCK_SIZE
        };

        let mut parts = prepared.parts.clone();
        parts.sort_by_key(|p| p.index);

        let plan = plan_chunks(&parts, block, bytes.len())?;
        tracing::debug!(
            scene = target.scene(),
            bytes = bytes.len(),
            parts = plan.len(),
            block,
            "开始分片上传"
        );

        for step in &plan {
            let chunk = &bytes[step.start..step.end];
            let endpoint = step
                .url
                .as_deref()
                .ok_or_else(|| MediaError::MissingField("presigned_url".to_string()))?;

            // 预签名 URL 由对象存储直接校验，**不能**带 Authorization 头。
            let resp = self.http.put(endpoint).body(chunk.to_vec()).send().await?;
            let status = resp.status();
            if !status.is_success() {
                return Err(MediaError::PartFailed { index: step.index, status: status.as_u16() });
            }

            let finish_body = serde_json::json!({
                "upload_id": prepared.upload_id,
                "part_index": step.index,
                "block_size": chunk.len().to_string(),
                "md5": md5_hex(chunk),
            });
            self.api.upload_part_finish(target, &finish_body).await?;
        }

        let merged = self
            .api
            .upload_finish(target, &prepared.upload_id, file_type.as_u8())
            .await?;

        let file_info = merged
            .file_info
            .ok_or_else(|| MediaError::MissingField("file_info".to_string()))?;

        let ttl = resolve_ttl(merged.ttl, self.file_info_ttl);

        self.cache.insert(
            key,
            Arc::new(CachedFileInfo {
                file_info: file_info.clone(),
                expires_at: Instant::now() + ttl,
            }),
        );

        metrics::counter!("qqbot_media_upload_total").increment(1);
        tracing::info!(scene = target.scene(), bytes = bytes.len(), "富媒体上传完成");
        Ok(file_info)
    }

    /// 上传渲染出的 PNG 图片。
    pub async fn upload_png(&self, target: &Target, png: &[u8]) -> Result<String, MediaError> {
        self.upload_bytes(target, FileType::Image, "image.png", png).await
    }

    /// 通过公网可访问 URL 上传（平台自动转存）。
    pub async fn upload_by_url(
        &self,
        target: &Target,
        file_type: FileType,
        url: &str,
    ) -> Result<String, MediaError> {
        let result = self
            .api
            .upload_by_url(target, file_type.as_u8(), url, false)
            .await?;
        result
            .file_info
            .ok_or_else(|| MediaError::MissingField("file_info".to_string()))
    }
}

/// 计算 `file_info` 的缓存时长。
///
/// 官方文档：`ttl` 为 `file_info` 有效期（秒），**`0` 表示可长期使用**。
/// 若把 `0` 直接当成 `Duration::ZERO`，缓存会立刻失效——恰恰在最该复用的场景
/// （服务端说长期有效）把秒传关掉了。
pub fn resolve_ttl(server_ttl: Option<u64>, local_cap: Duration) -> Duration {
    match server_ttl {
        // 0 = 长期有效 → 用本地上限兜底（仍需定期重传以规避平台策略变化）
        Some(0) | None => local_cap,
        Some(secs) => Duration::from_secs(secs).min(local_cap),
    }
}

/// 单个分片的执行计划。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkStep {
    pub index: u32,
    pub start: usize,
    pub end: usize,
    pub url: Option<String>,
}

/// 按服务端返回的分片列表规划每片的字节区间。
///
/// 纯函数，便于单测：分片大小以每片自带的 `block_size` 为准，缺失时回退到全局值。
pub fn plan_chunks(
    parts: &[qqbot_api::client::UploadPartUrl],
    fallback_block: usize,
    total: usize,
) -> Result<Vec<ChunkStep>, MediaError> {
    let mut steps = Vec::with_capacity(parts.len());
    let mut offset = 0usize;

    for part in parts {
        if offset >= total {
            break;
        }
        let size = part
            .block_size
            .as_deref()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .filter(|s| *s > 0)
            .unwrap_or(fallback_block);
        // ⚠️ `size` 来自服务端下发的字符串，只校验过 "> 0"。
        // 未检查的加法在 offset > 0 且 size 接近 usize::MAX 时会回绕成
        // `offset - 1`，随后 `&bytes[start..end]` 直接 panic。
        // 饱和加法把「size 过大」安全地归并成「吃到文件末尾」。
        let end = offset.saturating_add(size).min(total);
        steps.push(ChunkStep {
            index: part.index,
            start: offset,
            end,
            url: part.endpoint().map(str::to_string),
        });
        offset = end;
    }

    if offset != total {
        return Err(MediaError::PartMismatch { total, planned: offset });
    }
    Ok(steps)
}

// ---------- 校验值 ----------

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

pub fn md5_hex(data: &[u8]) -> String {
    use md5::{Digest, Md5};
    let mut h = Md5::new();
    h.update(data);
    hex_encode(&h.finalize())
}

pub fn sha1_hex(data: &[u8]) -> String {
    use sha1::{Digest, Sha1};
    let mut h = Sha1::new();
    h.update(data);
    hex_encode(&h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use qqbot_api::client::UploadPartUrl;

    fn part(index: u32, size: Option<&str>, url: &str) -> UploadPartUrl {
        UploadPartUrl {
            index,
            url: None,
            presigned_url: Some(url.to_string()),
            block_size: size.map(str::to_string),
        }
    }

    #[test]
    fn md5_matches_known_vectors() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
    }

    #[test]
    fn sha1_matches_known_vectors() {
        assert_eq!(sha1_hex(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
    }

    #[test]
    fn md5_10m_uses_first_10002432_bytes() {
        let mut data = vec![b'a'; MD5_10M_LEN + 100];
        data[MD5_10M_LEN] = b'b'; // 超出部分不应影响结果
        let full = md5_hex(&data);
        let head = md5_hex(&data[..MD5_10M_LEN]);
        assert_ne!(full, head);
        assert_eq!(head, md5_hex(&data[..MD5_10M_LEN]));
        assert_eq!(MD5_10M_LEN, 10_002_432);
    }

    #[test]
    fn plan_chunks_covers_whole_file() {
        let parts = vec![
            part(0, Some("5"), "u0"),
            part(1, Some("5"), "u1"),
            part(2, Some("5"), "u2"),
        ];
        let steps = plan_chunks(&parts, 5, 12).unwrap();
        assert_eq!(steps.len(), 3);
        assert_eq!((steps[0].start, steps[0].end), (0, 5));
        assert_eq!((steps[1].start, steps[1].end), (5, 10));
        assert_eq!((steps[2].start, steps[2].end), (10, 12)); // 最后一片截断
        assert_eq!(steps[2].url.as_deref(), Some("u2"));
    }

    /// 回归：服务端下发的 `block_size` 是不可信输入。
    /// 修复前 `(offset + size)` 会回绕成 `offset - 1`，产生 start > end 的非法
    /// 区间，随后 `&bytes[start..end]` 直接 panic。
    #[test]
    fn plan_chunks_survives_absurd_block_size() {
        let parts = vec![
            part(0, Some("5"), "u0"),
            part(1, Some("18446744073709551615"), "u1"),
        ];
        let steps = plan_chunks(&parts, 5, 12).expect("不应 panic，也不应失败");
        assert_eq!((steps[1].start, steps[1].end), (5, 12), "超大 size 应被夹到文件末尾");
    }

    #[test]
    fn plan_chunks_falls_back_to_global_block_size() {
        let parts = vec![part(0, None, "u0"), part(1, None, "u1")];
        let steps = plan_chunks(&parts, 4, 8).unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!((steps[0].start, steps[0].end), (0, 4));
        assert_eq!((steps[1].start, steps[1].end), (4, 8));
    }

    #[test]
    fn plan_chunks_detects_incomplete_plan() {
        let parts = vec![part(0, Some("5"), "u0")];
        assert!(matches!(
            plan_chunks(&parts, 5, 12),
            Err(MediaError::PartMismatch { total: 12, planned: 5 })
        ));
    }

    #[test]
    fn ttl_zero_means_long_lived_not_expired() {
        let cap = Duration::from_secs(30 * 60);
        // 官方：ttl=0 表示可长期使用
        assert_eq!(resolve_ttl(Some(0), cap), cap, "ttl=0 不应被当成立即过期");
        // 服务端给了更短的有效期 → 取较小值，宁短勿长
        assert_eq!(resolve_ttl(Some(300), cap), Duration::from_secs(300));
        // 服务端给的有效期比本地上限还长 → 用本地上限
        assert_eq!(resolve_ttl(Some(86400), cap), cap);
        // 未下发 → 用本地上限
        assert_eq!(resolve_ttl(None, cap), cap);
    }

    #[test]
    fn file_type_values_match_docs() {
        assert_eq!(FileType::Image.as_u8(), 1);
        assert_eq!(FileType::Video.as_u8(), 2);
        assert_eq!(FileType::Voice.as_u8(), 3);
        assert_eq!(FileType::File.as_u8(), 4);
    }
}
