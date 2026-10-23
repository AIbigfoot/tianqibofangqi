//! L1 应用服务层：编排用例、治理资源、把 L2 规则落到具体流程。
//!
//! 本层只依赖 L2 领域层；具体端口实现（L3）由组合根注入。

pub mod decode_quota;
pub mod import_service;
pub mod metadata_service;
pub mod playback_service;
pub mod portability_service;
pub mod progress_service;
pub mod search_service;
pub mod subtitle_service;
pub mod tag_service;
