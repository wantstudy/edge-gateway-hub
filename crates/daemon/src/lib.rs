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
//! Wave 2b 已落地模块：
//! - `pipeline` 逐点数据处理内核：点位映射 / 单位换算 / 死区过滤 / 时间戳统一（task 15）
//!
//! 规划模块（后续任务逐步填充）：
//! - `north`    北向 MQTT 转发（Wave 3 task 19+）

pub mod alarm;
pub mod audit;
pub mod auth;
pub mod backpressure;
pub mod bootstrap;
pub mod codec;
pub mod config;
/// task 34 敏感配置加密（AES-256-GCM + HKDF(机器码)，fail-closed）。
/// 模块本身仅提供加解密原语；接入 config 加载/落盘路径见 config.rs 后续集成任务。
pub mod config_crypto;
/// task #140 控制指令下发链路（管理面写指令 → 真实南向设备；Modbus
/// write_register / write_coil；幂等 + 审计 + 诚实投递语义）。
pub mod ctrl;
/// 数据面接线（D-14 修复）：南向采集样本 → 管线变换 → 北向投递。
pub mod dataplane;
pub mod driver;
pub mod error;
pub mod formula;
pub mod hardening;
/// 设备心跳上报端点（task 142）：模块源文件在 `mgmt/heartbeat_api.rs`，经 `#[path]`
/// 在此注册（不改动 `mgmt/mod.rs`；路由接线由 wave2 的 `be-port` 统一做）。
#[path = "mgmt/heartbeat_api.rs"]
pub mod heartbeat_api;
pub mod license;
pub mod logging;
/// 吞吐与运行指标自包含采集器（task 141 / BE-METRICS）：环形时间序列 + 快照序列化。
/// 仅暴露录入入口与 `snapshot()`，不做埋点（埋点由 wave2 统一接线）。
pub mod metrics;
pub mod mgmt;
pub mod migrations;
pub mod north;
pub mod offline_queue;
pub mod ota;
pub mod pipeline;
pub mod platform;
pub mod rules;
pub mod scheduler;
/// 点位模拟源（`sim_*` 配置的真实执行体；南向采集的仿真分支）。
pub mod sim;
/// 南向采集装配（生产 `PollHandler`，bootstrap 生产路径接线，D-12 修复）。
pub mod southbound;
pub mod telemetry_store;

/// 便捷再导出：`daemon::modbus` ≡ `daemon::driver::modbus`（task 9）。
pub use driver::modbus;
