//! L3 适配器层：实现 L2 声明的端口。
//!
//! M0 每个端口都提供**一个空实现**（返回"什么都不做/不支持"，用于解耦与占位）
//! 与**一个最小实现**（能走通契约测试的最小真实逻辑）。后续新增实现只需在本层
//! 增加类型并注册，不必改动 L2。

pub mod media_source;
pub mod metadata;
pub mod mpv;
pub mod online_site;
pub mod player;
pub mod subtitle;
pub mod subtitle_proto;
pub mod tag_dimension;
