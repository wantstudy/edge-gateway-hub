//! `auth` — 机器身份与授权域（Wave 1 起步）。
//!
//! - `machine_id`：多源锚点机器码指纹（task 3）
//! - `signing`：AuthBlock 签名（Ed25519 业务语义哈希，task 21）
//!
//! 后续任务填充：授权状态机与本地租约校验（Wave 6）。

pub mod client;
pub mod clock;
pub mod limits;
pub mod machine_id;
pub mod receipt_reporter;
pub mod signing;
pub mod trial;
