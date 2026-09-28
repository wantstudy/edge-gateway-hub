//! Protobuf 代码生成（方案 a：prost-build + protoc-bin-vendored）。
//!
//! `.proto` 文件为唯一 schema 源；protoc 使用 vendored 二进制，不依赖系统安装。

use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protoc_path = protoc_bin_vendored::protoc_bin_path()?;
    // 构建脚本单线程执行，无并发写入环境变量的竞态。
    std::env::set_var("PROTOC", protoc_path);

    let proto_dir = PathBuf::from("proto");
    prost_build::Config::new()
        .compile_protos(&[proto_dir.join("telemetry.proto")], &[proto_dir])?;
    Ok(())
}
