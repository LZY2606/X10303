//! 层合迹：LSM tombstone、范围删除与压实选择审阅
//!
//! 分层：
//! - [`model`]：不可变材料（WAL / flush / SSTable 摘要 / snapshot / point value / 删除记录）
//!   与“带单位/坐标基准、保留原始表示”的领域量。
//! - [`engine`]：LSM 内部键顺序、snapshot 可见性、tombstone 保留下沉、compaction 计划/执行/切分。
//! - [`scenario`]：反例档案七情景（重叠 L0、旧 snapshot、范围端点、同序号多类型、切分、中断、重复安装）。
//! - [`archive`]：事件溯源档案（不可变事件、分支、决定历史、投影）。
//! - [`storage`]：SQLite 持久化、半写恢复、导出/导入往返。
//! - [`api`]：Axum HTTP 层；业务规则全部可在服务端/存储层直接被测试调用。

pub mod engine;
pub mod model;
pub mod archive;
pub mod storage;
pub mod scenario;
pub mod api;
pub mod seed;

pub use model::*;
pub use archive::{Archive, BranchId, EventId};
pub use engine::*;
pub use storage::Store;
