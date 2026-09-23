# iot-daq 工程约定（AI Agent 上下文）

工业边缘数据汇聚与统一分发网关。Rust workspace（`crates/daemon` + `crates/licensing-server` + `crates/protocol-proto`）；
Windows = Tauri 桌面；Linux = Docker 容器（主推）+ headless；前端 Vue 3（web-console / admin-console 两套独立应用）。

## 当前进度快照（2026-09-23）

- **Wave 0 设计已定稿**（用户确认）：全部设计资产在 `docs/design/`，入口索引 = `docs/design/README.md`
  （每份资产标注对应计划任务号）。客户端界面实现基准 = `docs/design/prototype/gateway-v2a-glacier.html`，
  管理后台基准 = `docs/design/prototype/admin-console.html`。
- **Wave 1（task 1-7）已完成**：36 测试全绿。
- **Wave 2 / 2b 已完成**：task 8/9（驱动 trait + Modbus TCP/RTU）、task 15（数据处理内核 DataProcessor）。
- **Wave 3 已完成并入库**（commit `bdabb6e` / `96e0d2a` / `150d148` / `fd3800f`）：
  task 16 组轮询调度器、task 19 MQTT 客户端（rumqttc+rustls）、task 20 声明式 JSON 规则引擎、
  task 21 Ed25519 AuthBlock 签名。daemon 143 测试全绿，clippy `--all-targets -D warnings` 零告警。
- **进行中（Wave 3 尾 / Wave 6）**：task 62 北向双编码器（`north/encoder.rs`）、
  task 17 断网续传队列（`offline_queue.rs`）、task 45 云授权服务（`crates/licensing-server/`）。
- **下一步**：task 18 加密存储（依赖 17）、task 46 激活码生命周期（依赖 45）、
  task 22 云授权客户端（依赖 45）、task 23/24 试用与降级、task 25/26 安全。

## 唯一计划契约（按需读取，禁止整读）

`.omo/plans/iot-daq-gateway.md`（约 2900 行 / 69 任务 + F1-F4）是唯一需求与验收契约。**永远不要整文件读取**，工作流：

1. 先 Grep 定位任务标题行号：模式 `- [ ] <task编号>\.`（或 `- [x]`，已完成任务已勾选）
2. 只 Read 该任务段落（通常 30-50 行：What to do / Must NOT do / Acceptance / Evidence / Commit）
3. 需要背景时同理局部查「访谈决议汇总」表（Context 区）与 Wave 划分（约 217-267 行）

## 构建与测试（每条 cargo 命令必须带环境前缀）

本机 rustup gnu 工具链 + mingw-w64 位于 `D:\rust`，**不在系统 PATH**：

```bash
export RUSTUP_HOME='D:\rust\rustup' CARGO_HOME='D:\rust\cargo' \
  PATH="/d/rust/cargo/bin:/d/rust/mingw64/bin:$PATH"
```

- **顺序红线**：`/d/rust/mingw64/bin` 必须排在 rustup self-contained 目录之前（顺序错则 raw-dylib/dlltool
  构建失败，报 `cannot find -lmsvcrt`）。
- 三道门禁（任何交付前必须全绿）：
  - `cargo fmt --all -- --check`
  - `cargo clippy --workspace --all-targets -- -D warnings`
  - `cargo test --workspace`
- 团队沙箱 stdout 可能为空：重要输出重定向 `> /tmp/xxx.log 2>&1` 后用 Read 读取。

## 依赖红线（违反即返工）

1. **纯 Rust 依赖栈（口径已澄清 2026-09-23）**：红线禁的是**需要系统 C 库 / 系统构建工具链**的依赖
   （openssl-sys 非 vendored、paho-mqtt、aws-lc-rs 需 cmake+NASM）。**vendored C 源**（随 crate 一起编译、
   不链接系统库）不在禁止之列，但引入前必须实测 mingw 构建通过：
   - ✅ `rusqlite 0.40.2 + bundled`（libsqlite3-sys 0.38.2，task 17/18 用）—— 已实测 `cargo check` EXIT=0
   - ❌ `aws-lc-rs`（cmake/NASM）；rustls 的 crypto provider 统一选 **`ring`**（已显式启用）
   - ❌ `paho-mqtt`（C 库）→ 用 `rumqttc`；❌ `openssl-sys` → 用 rustls 系
   - ⚠️ vendored C 的两条固化约束（2026-09-23 裁决，QA 提出后主理人接受）：
     ① 多架构镜像（task 60 的 amd64/arm64）**必须在目标架构容器内构建**（buildx 原生或 qemu），
     禁止从 Windows 宿主交叉编译 vendored C（交叉时 `cc` 需目标 C 工具链，必然碎）；
     ② CI 门禁须显式跑 `cargo check -p daemon --all-targets`，把 vendored C 构建失败挡在合并前
2. **raw-dylib 敏感**：`windows-sys 0.60+` / `getrandom 0.4` 走 raw-dylib，须 mingw dlltool 链接；
   mingw 已就绪（`D:\rust\mingw64`），tempfile 无需钉版
3. **密钥红线**：仓库不提交激活码 / 私钥 / 真实机器码；HMAC key 从配置 / env 注入；
   测试专用 key 以 `TEST_ONLY_` 前缀命名并注释 test-only
4. **客户端语义**：客户端侧代码（crates/daemon 等）不得出现解绑 / 重置试用 / 废弃激活码入口
   （换机由厂商总管理后台经云端执行）
5. **JSON 编码**：纳秒时间戳 / uint64 计数器在 JSON 路径必须编码为字符串（IEEE754 double 上限 2^53−1），
   见 `crates/protocol-proto/proto/telemetry.proto` 头注释
6. **`.omo/` 只读**：计划文档的更新（勾选 / 偏差记录）须先向用户说明；`target/` 与 `.omo/run-continuation/` 不入库

## 任务执行纪律

1. 开工前：读目标 task 段落 + 相关设计资产（`docs/design/` 内对应文件）
2. **设计先行红线**：凡任务要求「先出设计图 / 界面设计」，必须等用户 explicit 确认后才允许写实现代码
3. 小步提交：每完成一个 task 跑三道门禁 → 单独 commit
   （`feat(scope): desc` / `docs(design): desc` / `chore(waveN): desc`），提交信息风格参照 git log
   - ⚠️ **提交纪律（血泪教训，2026-09-24）**：**禁止用 `git add -A` / `git add .`**。多 agent 并行时，
     工作区里随时存在**其它成员尚未完成的中途文件**，`add -A` 会把它们一并提交，
     使「HEAD 的内容」与「成员心中的最新版本」不一致；此后任何 `git checkout <file>`
     都会**静默丢失成员未提交的工作**（本机已发生一次：`service.rs` 42 行测试辅助函数被卷走）。
     **正确做法：只 `git add <本次实际交付的显式路径>`**（逐个文件或逐个目录列出），
     提交前用 `git status --short` 复核暂存区，确认不含非本次交付的文件。
   - 同理，**撤回临时改动时禁止 `git checkout <file>`**（会回退到 HEAD 而非「改之前」）。
     改用：先 `cp <file> <file>.bak` 再改；或改完只删自己加的那几行。
4. 完成自检：逐条对照该 task 的 Acceptance Criteria；如实际实现与计划有偏差，先向用户说明理由获批，
   再把偏差记入计划文档对应任务段
5. 全局不做清单：不硬编码激活码/私钥；不直接签 Protobuf 序列化字节（签业务语义确定性哈希）；
   不做内核驱动级防护 / 硬件加密狗；防逆向只做 Tier-1
