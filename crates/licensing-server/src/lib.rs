//! `licensing-server` — 云端授权服务。
//!
//! # 职责
//!
//! - **task 45**：激活 / 心跳 / 服务端二次校验 / B 档审计回执；Lease Token（Ed25519）签发与 kid 轮换；
//!   授权白名单与配额；试用期记录；统一错误码。
//! - **task 46**：激活码生命周期（发放 / 绑定 / 废弃 / 重发）与一机一码绑定。
//!
//! # 模块地图
//!
//! | 模块 | 职责 |
//! |------|------|
//! | [`error`] | `LicenseError` / `LicenseResult` / 错误码分段 |
//! | [`keys`] | `KeyRing`：多 kid 并存与轮换（active → retiring → retired） |
//! | [`token`] | `LeaseClaims` / 三段式 Token 签发与验签 |
//! | [`model`] | 9 表数据模型与状态枚举（对齐 `docs/design/licensing-data-model.md`） |
//! | [`store`] | 仓储层：全部 SQL 集中于此，API/业务层不写 SQL |
//! | [`proto`] | 端点请求 / 响应结构（对齐 `docs/design/licensing-api.md`） |
//! | [`service`] | 业务规则：配额、幂等、时序、跳空检测、一机一码判定 |
//!
//! # 授权判定位置（不可动摇的边界）
//!
//! 本 crate 是**云端**授权服务。**授权判定最终仍在网关 Rust 侧**（daemon），
//! 本服务只负责签发与稽核。daemon 侧任何时刻不得依赖本服务的实时可用性——
//! 离线宽限 7 天、断网继续采集与转发是硬要求（设计 §1.5）。
//!
//! # 私钥纪律
//!
//! 签名私钥**只经环境变量注入**（[`keys::KeyRing::register_from_b64`]），
//! 绝不写入数据库、绝不写入日志、绝不进仓库（[`model::SigningKey`] 只存公钥与 `hsm_ref`）。

pub mod error;
pub mod keys;
pub mod model;
pub mod proto;
pub mod service;
pub mod store;
pub mod token;
