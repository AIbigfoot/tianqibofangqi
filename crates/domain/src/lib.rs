//! L2 领域层（Domain）
//!
//! 只放**纯逻辑与端口契约声明**，绝不引入 IO、框架或运行时依赖。
//!
//! 分层约定（见架构文档第 3 节）：
//! ```text
//! L0 React + TS  →  L1 应用服务  →  L2 领域（声明 trait，零 IO）  ←  L3 适配器（实现端口）  →  L4 基础设施
//! ```
//! 五个端口（P-01 ~ P-05）在 [`ports`] 中声明；跨层共享的数据类型在 [`model`]；
//! 与 IO 无关的规则（用户锁定、音频焦点、轨道优先级、检索组合）在 [`rules`]。

pub mod error;
pub mod model;
pub mod portability;
pub mod ports;
pub mod rules;
pub mod subtitle;
pub mod tagging;

pub use error::{PortError, Result};
