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
