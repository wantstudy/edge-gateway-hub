# iot-daq-gateway 学习笔记（learnings）

## task 8 — Driver trait + PointAddressParser + Reconnector（2026-09-23）

### 关键设计决策

1. **`Driver` trait 签名**：`#[async_trait]` 四方法 `connect / read(&[ReadPoint]) / write(&[WritePoint]) / disconnect`，
   统一返回 `DaemonResult<T>`；supertrait `Send + Sync`（daemon 侧任务化运行需要）。
   - `read` 契约：返回与请求等长、按下标一一对应的 `Vec<PointSample>`，任一点失败整体 `Err`（不产部分结果）。
   - `write` 的 `value` 为原始字节 `Vec<u8>`，编码语义（字序/宽度）由具体协议驱动解释 —— 保持协议无关，
     类型解码留给数据处理链路（task 15+）。
   - `ReadPoint.count` 承载连续读取长度（寄存器/字/字节/位，单位由协议解释），供 Modbus 批量读等场景。
   - 未加 `is_connected()` 等方法 —— 最小可用，避免过度设计。

2. **`PointAddress` 用扁平结构而非枚举**：`{ db: u32, area: Option<char>, start: u32, bit: bool, bit_index: u8 }`。
   - 理由：QA 断言形状 `{db:1, start:0, bit:true}` 直接对应字段；后续 Modbus（task 9）等新格式
     只需扩展字段/解析规则，不破坏既有驱动 match。
   - 字段语义随协议解释：S7 用 `db`+`start`(字节偏移)+`bit`+`bit_index`；MC 用 `area`+`start`(元件号)+`bit`。

3. **解析器范围（task 8 明确边界）**：仅支持 S7 位访问 `DBn.DBX<start>.<bit>`（bit 0-7）与 MC `M<no>`/`D<no>`。
   - `DBB/DBW/DBD` 字/字节形式**显式拒绝**（错误消息说明留待 S7 驱动任务扩展）—— 避免丢失访问宽度信息
     造成 S7 驱动无法区分 DBW/DBD。
   - Modbus `40001` 风格地址同样拒绝（task 9 扩展）。
   - 解析前 `trim()` + `to_ascii_uppercase()`（配置书写容错，有测试锁定）。

4. **`Reconnector` 纯逻辑无 IO**：`next_delay()` 返回 `Duration`，由驱动自行 sleep —— 不引入 tokio 依赖。
   - 状态推进用 `current.saturating_mul(multiplier).min(max)`，天然防溢出。
   - 非法参数钳制（`initial` 下限 1ms 防忙重试、`max >= initial`、`multiplier >= 1`），与
     `MachineIdentity::new` 抬升 `min_anchors` 的既有风格一致。
   - 默认 1s -> 2s -> ... -> 60s 封顶。

5. **依赖**：`async-trait = "0.1"`（正式依赖，纯 Rust）；`tokio = { features = ["rt","macros"] }` 仅作
   dev-dependency 供 `#[tokio::test]` 执行 mock 驱动的异步方法 —— task 9+ 具体协议驱动接入时升为正式依赖。

### 踩坑记录

- `spec.split_at(1)` 在 `"DB1.DB"`（访问类型缺失）时对空串 panic（`end byte index 1 out of bounds`）——
  已加 `spec.is_empty()` 前置检查，测试 `parse_malformed_addresses_rejected` 覆盖。
- PowerShell 重定向 `>` 产生 UTF-16 文件，Read 工具无法读取；改用 `cmd /c "cargo ... > file 2>&1"` 得到 UTF-8 字节流。
- PowerShell 的 PATH 分隔符是 `;`（非 bash 的 `:`），且 `/tmp/` 不存在 —— 重定向目标用仓库内相对路径。

### 测试覆盖（13 个 driver 测试，45 全绿）

- 地址解析 happy：`DB1.DBX0.0`（QA）、`DB12.DBX34.7`、`M100`（QA）、`D100`、trim/大小写容错。
- 地址解析 error：`INVALID`（QA，断言 `ProtocolError` + `error_code()==ERR_PROTOCOL`）、空串/空白、
  18 个格式错误用例（缺 DB 号/缺位号/位号越界/多余分段/字访问/负号/字母/未支持区/Modbus 地址）。
- Reconnector：默认序列 1,2,4,8,16,32,60,60,60；自定义 500ms/2s/3；reset 归零；非法配置钳制。
- Driver trait：mock 端到端（connect->read->write->disconnect + 状态断言）、`Box<dyn Driver>` 对象安全、
  未连接 read 返回 `ProtocolError`。

## task 9 — Modbus TCP / RTU-over-TCP 驱动（2026-09-23）

### 关键设计决策

1. **tokio-modbus 0.17 导入路径**：crate root 只导出 `Slave/Error/ProtocolError/Request/Response/ExceptionCode`，
   **没有 `pub mod tcp`/`pub mod rtu`** —— 客户端模块在 `tokio_modbus::client::{tcp, rtu}`。
   `tcp::connect_slave(SocketAddr, Slave) -> io::Result<Context>`；`rtu::attach_slave(transport, Slave) -> Context`
   （`T: AsyncRead+AsyncWrite+Unpin+Send+'static`）；`tcp::attach_slave` 还要求 `T: fmt::Debug`。
   `Context::disconnect()` 是 `Client` trait 方法，必须显式 `use tokio_modbus::client::Client`。

2. **`Context` 非 `Sync` → `tokio::sync::Mutex<Context>`**：`Context` 内部 `Box<dyn Client>` 仅 `Send`，
   而 `Driver` trait 要求 `Send + Sync`。用 `tokio::sync::Mutex` 包裹后 `Mutex<Context>: Sync` 满足对象安全。
   每次请求 `ctx.lock().await` 取 guard，`&mut guard` 经 Deref 强转成 `&mut Context`（clippy 会提示
   `explicit_auto_deref`，直接写 `&mut guard` 即可）。

3. **闭包式请求分发有生命周期陷阱**：`F: Fn(&mut Context) -> Fut` 中 `Fut` 不能依赖参数生命周期
   （`|ctx| async move {...}` 返回的 future 捕获 `&mut Context`，HRTB 不满足）。改为**枚举分发**：
   `RequestOp { ReadHolding(u16,u16), ReadInput(u16,u16), WriteSingle(u16,u16) }`（`Copy`，重试可复用）
   + `async fn execute(ctx: &mut Context, op: RequestOp)` + `RequestOutcome { Words(Vec<u16>), Written }`。

4. **guard 借用与 `&mut self` 冲突**：`run_request` 里 guard 持有 `self.ctx` 的不可变借用，match 分支调用
   `self.recover(...)`（需 `&mut self`）会 E0502。解法：把 `timeout(...)` 包进块作用域，guard 在块尾 drop，
   再 match 结果。`recover` 里同样处理，且 guard 需 `let mut`（`&mut *guard` 要 DerefMut）。

5. **错误映射**：拨号失败/传输中断 → `NetworkError`(6000)；请求超时（驱动层 `tokio::time::timeout` 包装）/
   异常响应/帧校验 → `ProtocolError`(1000)。请求中 `Error::Transport` 视为连接丢失走重连。
   重连语义：`connect()` 成功 `reset()`；恢复重连不 reset；重试仍失败再推进一次。

6. **测试断言退避状态用 clone-peek**：`driver.reconnector().clone().next_delay()` 读当前值不推进，
   避免给 `Reconnector` 加 `current_delay()`（更外科手术）。

7. **mock 服务器**：`tokio_modbus::server::tcp::{Server, accept_tcp_connection}` + 自定义 `Service` impl
   （共享 `Arc<Mutex<MockState>>` 记录请求日志/写日志）。`new_service` 闭包捕获 Arc 引用克隆，
   `on_connected = move |stream, addr| { let ns = new_service.clone(); async move { accept_tcp_connection(stream, addr, ns) } }`。
   flaky 服务器：先 accept 一次立即 drop，再起正常 server（模拟设备崩溃恢复）。

### 踩坑记录

- **「静默服务器」测试不能「读空即退出」**：服务器读到请求后继续读，客户端超时后重连，旧连接被服务器
  读到 EOF 关闭 → 客户端收到 `Transport`(10054) 而非超时。正确做法：accept 后**不读不写**，`sleep` 保持
  连接打开，且循环 accept（重连会建新连接）。
- **clippy `await_holding_lock`**：测试里 `let state = state.lock()` 后跨 `driver.disconnect().await` 报错。
  解法：断言块用 `{ }` 包裹，guard 在 await 前 drop。
- **clippy `cloned_ref_to_slice_refs`**：`&[point.clone()]` → `std::slice::from_ref(&point)`。
- **`write` 工具对已存在文件报错**：整文件重写需先 `Read` 再 `Write`，否则报 `File already exists`。

### 测试覆盖（13 个 modbus 测试，58 全绿）

- 解析：40001/30001 归一化、非零字段、拒绝列表（00001/10001/4/40000/40001A/465537/90001）。
- TCP happy：FC03 读保持（大端拼接+请求日志）、FC04 读输入、FC06 写单个（寄存器更新+写日志）。
- 错误路径：异常→ProtocolError 不推进退避；拨号失败→NetworkError 推进退避；静默超时→ProtocolError 退避推进 2 次。
- 重连：flaky 首连接即断→重连成功重试，退避不重置。
- RTU-over-TCP：slave id/FC03/地址/数量/CRC16 低字节先发。
- 本地校验：输入区写拒绝、值非 2 字节拒绝、count 0/126 拒绝。
