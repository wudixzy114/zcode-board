//! zcode（Z.ai / GLM Coding Plan）额度看板的核心逻辑。
//!
//! 只负责数据与调度，不含任何界面代码，可被原生界面或服务端复用。

pub mod app;
pub mod autostart;
pub mod client;
pub mod discover;
pub mod rate;
pub mod scheduler;
pub mod store;
pub mod util;
