//! `licensing-server` — 云端授权服务。
//!
//! Wave 1 已落地：
//! - `error`    LicenseError / LicenseResult 统一错误与错误码雏形（task 6）
//!
//! 职责（Wave 6 task 45-46 实现）：
//! - 激活码发放 / 绑定（一机一码）/ 废弃 / 重发
//! - Lease Token（Ed25519）签发与 kid 轮换
//! - 24h 心跳校验、7 天离线宽限、试用期管理
//! - B 档审计回执校验（序号区间 / 条数 / 摘要哈希，不含业务数值）

pub mod error;
