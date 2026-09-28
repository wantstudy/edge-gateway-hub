//! 北向转发（Wave 3：task 19 MQTT 客户端 + 后续出口）。
//!
//! - [`encoder`]：载荷编码器实现（task 62）。
//! - [`mqtt`]：MQTT 客户端 / 连接池 / task 54 背压接线原语（`NorthOutlet` 等）。
//! - [`runtime`]：北向**运行期**接线（task 19 + task 54 的「最后一跳」）——
//!   按 `[[outlets]]` 建出口、起驱动任务、停机收口；由 `bootstrap` 装配。

pub mod encoder;
pub mod mqtt;
pub mod runtime;
