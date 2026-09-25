//! LSM 领域内核：内部键顺序、snapshot 可见性、point/range tombstone
//! 在 compaction 中的保留与下沉规则。纯逻辑、无 I/O，可被测试直接调用。

pub mod basis;
pub mod digest;
pub mod error;
pub mod model;
pub mod read;
pub mod compaction;
pub mod budget;

pub use basis::{BasisId, BasisKind, KeyRep, Quantity, UnitKind};
pub use error::{AppError, Diagnostic, Pos};
pub use model::*;
pub use read::{read_point, ReadVerdict};
pub use compaction::{compact, CompactionOutcome};
