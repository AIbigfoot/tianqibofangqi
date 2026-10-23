//! 五个端口（P-01 ~ P-05）的契约声明。
//!
//! 契约定义在 L2、实现放 L3；前端可见的类型通过代码生成同步到 TS（M0 暂不含生成器）。
//! 除 P-05 外，其余均为 trait；P-05 是「配置 + 数据驱动」的注册表，不设枚举。

pub mod javdb;
pub mod media_source;
pub mod metadata;
pub mod player;
pub mod subtitle;
pub mod tag_dimension;

/// 事件流出口。P-03 的 [`subtitle::SessionHandle`] 与 P-04 的
/// [`player::PlayerCore`] 共用同一抽象。
///
/// M0 用「拉取式」事件源而非 async 通道：骨架里没有任何异步运行时依赖，
/// 便于契约测试在没有 IO 的情况下跑通。后续替换为推送式不改变 trait 形态。
pub trait EventSource<T>: Send {
    /// 取下一个事件；无事件时返回 `None`。
    fn try_next(&self) -> Option<T>;
}

/// 事件流句柄。
pub type EventStream<T> = Box<dyn EventSource<T>>;

/// 一个永远没有事件的空事件流，供「空实现」复用。
pub struct EmptyEvents<T> {
    _marker: std::marker::PhantomData<T>,
}

impl<T> EmptyEvents<T> {
    pub fn new() -> Self {
        Self {
            _marker: std::marker::PhantomData,
        }
    }
}

impl<T> Default for EmptyEvents<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Send> EventSource<T> for EmptyEvents<T> {
    fn try_next(&self) -> Option<T> {
        None
    }
}
