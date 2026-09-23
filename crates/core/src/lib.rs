//! 采集核心的库形态。
//!
//! 单独暴露出来是为了让 CLI 和桌面看板共用**同一份**读路径：看板显示的数字
//! 必须和 `classagent-core export` 算出来的一致，否则 UI 就是一个独立的解释器。

pub mod digest;
pub mod protocol;
pub mod store;
pub mod supervisor;
pub mod timeline;
