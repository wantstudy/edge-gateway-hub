//! `daemon` — 工业边缘数据汇聚与统一分发网关核心库。
//!
//! Wave 1 已落地模块：
//! - `auth`     机器码指纹（多源锚点 + N-of-M + HMAC-SHA256，task 3）
//! - `config`   TOML 强类型配置 + notify 热重载（防抖 + 版本号，task 4）
//! - `logging`  tracing 分级 + JSON 结构化可选 + 按天轮转（task 5）
//! - `error`    DaemonError / DaemonResult 统一错误与错误码（task 6）
//!
//! Wave 2 已落地模块：
//! - `driver`   南向驱动 trait + PointAddressParser + Reconnector 指数退避（task 8）
//! - `driver::modbus` Modbus TCP / RTU-over-TCP 驱动（task 9；`daemon::modbus` 为便捷再导出）
//!
//! 规划模块（后续任务逐步填充）：
//! - `pipeline` 数据处理链路（Wave 2b task 15+）
//! - `north`    北向 MQTT 转发（Wave 3 task 19+）

pub mod auth;
pub mod config;
pub mod driver;
pub mod error;
pub mod logging;

/// 便捷再导出：`daemon::modbus` ≡ `daemon::driver::modbus`（task 9）。
pub use driver::modbus;
