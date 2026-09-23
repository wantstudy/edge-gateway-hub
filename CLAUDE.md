# iot-daq 工程协作约定（给 AI 助手 / 新成员的速查）

## 构建与测试

```bash
cargo build --workspace        # 构建
cargo test --workspace         # CI 门禁
cargo fmt --all -- --check     # 格式门禁
cargo clippy --workspace --all-targets -- -D warnings   # lint 门禁
```

## 环境前缀（每条 shell 命令必带）

```bash
export RUSTUP_HOME='D:\rust\rustup' CARGO_HOME='D:\rust\cargo' \
  PATH="/d/rust/cargo/bin:/d/rust/mingw64/bin:$PATH"
```

- 工具链：`stable-x86_64-pc-windows-gnu`（无 MSVC）；`/d/rust/mingw64/bin` 为 winlibs
  mingw-w64 独立发行版（gcc 16.2 + binutils 2.47），供 dlltool/raw-dylib 链接与后续 C 依赖编译。
- **顺序红线**：`mingw64/bin` 必须在 rustup 自带 self-contained 目录**之前**（实测正确的
  工作前缀直接省略 self-contained：若 self-contained 的 ld 先被解析，会因与 mingw gcc
  不匹配而报 `cannot find -lmsvcrt` 链接错误）。
- stdout 捕获在团队沙箱可能为空：重要输出用 `> /tmp/xxx.log 2>&1` 重定向后 `Read` 读取。

## 依赖红线（违反即返工）

1. **纯 Rust 依赖栈**：禁止 rusqlite / openssl-sys 等 C 编译依赖（Wave 2b/3 前提下；
   引入前确认 mingw 工具链与 `cargo deny`）。
2. **raw-dylib 敏感**：`windows-sys 0.60+` / `getrandom 0.4` 走 raw-dylib，必须经
   mingw dlltool 链接；若 mingw 不可用，依赖树需钉旧版（tempfile 已钉 3.14.0，见根 README）。
3. **密钥红线**：仓库不提交激活码 / 私钥 / 真实机器码；HMAC key 从配置 / env 注入；
   测试专用 key 以 `TEST_ONLY_` 前缀命名并在注释标注 test-only。
4. **客户端语义**：客户端代码不得出现「解绑 / 重置试用」入口（换机由厂商后台执行）。
5. **JSON 编码约定**：纳秒时间戳 / uint64 计数器在 JSON 路径必须字符串编码（2^53−1），
   见 `crates/protocol-proto/proto/telemetry.proto` 头注释。
6. **.omo/ 只读**；`docs/design/*.svg` 与 `*.md` 归架构师维护；`target/` 与
   `.omo/run-continuation/` 不入库。

## 提交口径

- `feat(scope): desc` / `chore(wave1): ...`，每 Wave 一组 commit；预提交跑 fmt + clippy + test。
- CI 仅 `windows-latest` / `ubuntu-latest` 双平台矩阵（**不含 macos**）。
