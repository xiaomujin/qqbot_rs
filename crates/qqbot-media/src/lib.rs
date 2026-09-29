//! 富媒体上传与 `file_info` 缓存。
//!
//! 官方约束（已核对文档）：
//! - 上传分四步：`upload_prepare` → PUT 预签名 URL → `upload_part_finish` → `files` 合并
//! - 预上传需要 `md5` + `sha1` + `md5_10m` 三个校验值
//! - `file_info` 有 TTL，过期需重传；**单聊与群聊的上传接口互不相通**

pub mod error;
pub mod uploader;

pub use error::MediaError;
pub use uploader::{
    md5_hex, plan_chunks, resolve_ttl, sha1_hex, ChunkStep, FileType, MediaUploader, MD5_10M_LEN,
};
