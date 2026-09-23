# iot-daq 云端授权 API 契约（对应计划 task 44）

> 配套：`licensing-data-model.md`（ER / 状态枚举）、`licensing-server-and-admin-console.svg`（部署与组件）。
> 本契约为 Wave 6（任务 45-48）实现基线；不写实现代码。

## 0. 总则

- **Base URL**：`https://licensing.<vendor>.com/v1`
- **统一响应包裹**：`{ "code": "<BUSINESS_CODE>", "data": {...}, "message": "...", "trace_id": "..." }`；HTTP 状态码 + 业务码双重表达。
- **鉴权边界**：
  - 设备端：`Lease Token`（Ed25519 签名）+ 设备私钥对请求规范字段签名（`device_sig`）。
  - 管理端：RBAC 会话（管理员登录态）；**废弃 / 重发等 /admin/* 写操作仅厂商管理员可用**。
  - 签名私钥仅存于隔离区签名服务（KMS / 离线），业务 API 只持有 `kid` 引用。
- **时间**：一律 UTC 秒；涉及时窗校验 ±5min。
- **幂等原则**：写操作携带 `idempotency_key`；同 key 重放返回首次结果（不产生第二个实体）。
- **档位随 Token 下发**：`verify_mode ∈ {A, B, C}`（租户级默认 + 设备级覆盖），客户端无档位切换接口。

## 1. 设备端端点

### 1.1 `POST /activation`（激活 + 绑定）

| 项 | 内容 |
|----|------|
| 请求 | `{ activation_code, machine_code, anchor_hashes[5], device_pubkey, nonce, ts, req_sig }` |
| 响应 200 | `{ lease_id, lease_token(Ed25519, kid), verify_mode, tier, valid_until, heartbeat_hours=24, server_time, nonce(回显), sig }` |
| 幂等语义 | 同码同机重复激活（凭证丢失重装等）→ 返回现存有效租约（幂等）；同码异机 → 拒绝，不产生新租约 |
| 错误码 | `400 INVALID_CODE`；`403 CODE_REVOKED`（码已废弃）；`403 CODE_BOUND_TO_OTHER_DEVICE`（同码异机，task 41 冲突检测）；`409 NONCE_REPLAY`；`401 TIMESTAMP_SKEW`（±5min 外）；`409 CODE_REISSUED`（旧码已被重发替代，提示导入新码） |
| 重放双重拒绝 | 激活响应被重放到另一台设备：该机 `nonce` 已在 `nonce_cache` → `409 NONCE_REPLAY`；且其 `machine_code` 与绑定不符 → `403 CODE_BOUND_TO_OTHER_DEVICE`——**Nonce + 机器码绑定双重拒绝** |

### 1.2 `POST /heartbeat`（24h；须携带最近审计回执序号区间）

| 项 | 内容 |
|----|------|
| 请求 | `{ lease_id, ts, nonce, receipt_cursor: {seq_from, seq_to}（B 档最近已确认回执区间；A/C 档可空）, device_sig }` |
| 响应 200 | `{ server_time, next_deadline, valid_until, verify_mode, tier, sig }` |
| 幂等语义 | 心跳本身可重复（无害）；同 `nonce` 重放 → `409 NONCE_REPLAY` |
| 错误码 | `403 LEASE_REVOKED`（**被后台废弃：客户端立即进入降级 / 停止北向转发，废弃语义 = 立即失效**）；`404 LEASE_NOT_FOUND`；`401 TIMESTAMP_SKEW`；`409 NONCE_REPLAY` |
| 服务端副作用 | 回写 `lease.last_heartbeat_at` 与 `device.status`；`receipt_cursor` 写入跳空检测游标 |

### 1.3 `POST /verify`（**仅 A 档**：服务端二次校验）

| 项 | 内容 |
|----|------|
| 请求 | `{ device_mid, lease_id, payload_digest, ts, nonce, device_sig }`（业务消息级：验签对象为规范化摘要，非序列化原始字节） |
| 响应 200 | `{ ok: true, server_time, nonce(回显) }` |
| 校验链 | Ed25519 验签（kid 公钥集）→ **±5min 时间窗** → **全局 Nonce 防重放**（`nonce_cache`）→ 字段白名单 → 计量入账 |
| 幂等语义 | 同 nonce 重放 → 409（不重复计量） |
| 错误码 | `401 VERIFY_FAIL`；`401 TIMESTAMP_SKEW`；`409 NONCE_REPLAY`；`403 LEASE_REVOKED`；`422 FIELD_WHITELIST_VIOLATION` |

### 1.4 `POST /audit/receipt`（**B 档审计回执**）

| 项 | 内容 |
|----|------|
| 请求（**字段白名单，逐字段校验**） | `{ device_mid, lease_id, seq_from, seq_to, count, payload_digest, ts, sig }` |
| 白名单强制 | 服务端**拒收任何业务字段**：请求体出现白名单外字段 → `422 FIELD_WHITELIST_VIOLATION`，整单拒收并记审计 |
| 响应 200 | `{ accepted: true, gap: "none" \| "gap" \| "overlap" \| "missing", server_time }` |
| 幂等 / 补报语义 | **相同区间重复上报 → 幂等接受（去重，不重复告警）**；**断网时网关仍持续转发，回执允许延迟补报**（`received_at` 可远晚于 `ts`，按 `ts` 排序评估） |
| 跳空检测 | 服务端按 `expected = last_seq_to + 1` 判定：`seq_from > expected` → **gap（跳空）**；`seq_from ≤ last_seq_to` → **overlap（回退 / 重叠）**；窗口内无回执 → **missing（缺失）**——均置 `gap_flag` 写风控告警（`audit_log`），**人工核实、不自动封禁** |
| 错误码 | `422 FIELD_WHITELIST_VIOLATION`；`403 LEASE_REVOKED`；`404 LEASE_NOT_FOUND`；`409 NONCE_REPLAY` |

### 1.5 档位契约（A/B/C）

| 档位 | 出网通道 | 契约要点 |
|------|---------|---------|
| A | /verify 全链 | 每条业务消息验签 + ±5min + 全局 Nonce + 白名单 + 计量 |
| **B（默认）** | 心跳 + /audit/receipt | **网关侧本地验签 + 租约状态机判定；断网仍持续转发；回执延迟补报；云端跳空 / 回退 / 缺失告警** |
| C | 仅心跳 | 仅租约保活，无回执无计量 |

## 2. 管理端端点（**仅厂商管理员可用**，RBAC 门控）

> 通用错误：非管理员或角色不符 → `403 ADMIN_ONLY`；会话过期 → `401 SESSION_EXPIRED`。所有写操作写 `audit_log`（actor / action / detail / ip）。

### 2.1 `POST /admin/codes/issue`（发放）

- 请求：`{ tenant_id, tier, valid_from, valid_until, count, prebind_machine_code?（可预绑定）, idempotency_key }`
- 响应：`{ codes: [{ code_id, code, status: "issued", prebind? }] }`
- 幂等：同 `idempotency_key` → 返回首次发放结果。
- 错误：`403 ADMIN_ONLY`；`400 TENANT_NOT_FOUND`。

### 2.2 `POST /admin/codes/{id}/revoke`（废弃 · **高危**，仅厂商管理员）

- 请求：`{ reason（必填，枚举+文本）, note（补充说明 ≥10 字）, confirm_tail8（激活码后 8 位）, second_approver?（双人复核开启时必填） }`
- 语义：**立即失效**——`status → revoked`、作废原设备租约（最迟下一次心跳 ≤24h 被拒）；**与 reissue 可同事务**。
- 幂等：同码同原因重复撤销 → 幂等成功（返回已撤销态）。
- 错误：`403 ADMIN_ONLY`（**客户端直接调用废弃接口 → 403，客户端亦无此入口**）；`412 CONFIRM_MISMATCH`（后 8 位不符）；`400 REASON_REQUIRED`；`409 ALREADY_REISSUED`（该码已重发，禁止再撤销原码）。

### 2.3 `POST /admin/codes/{id}/reissue`（重发 · **高危**，仅厂商管理员）

- 请求：`{ prebind: { machine_code } | null（留待首次激活绑定）, inherit_tier: bool, inherit_validity: bool, overrides?, idempotency_key（与 revoke 同事务共用） }`
- 响应：`{ new_code: { code_id, code, reissued_from: <原 code_id>, status: "issued" } }`
- 语义：`reissued_from` 溯源链；**废弃 + 重发同事务且接口幂等**（同 key 重放返回同一新码，不产生第二个新码）。
- 错误：`403 ADMIN_ONLY`；`409 ORIGINAL_NOT_REVOKED`（未废弃不可重发）；`400 PREBIND_CONFLICT`（预绑定机器码已绑定他码）。

### 2.4 `GET /admin/codes`（列表 / 筛选）

- Query：`tenant_id, status(issued|bound|revoked|reissued), tier, order_id, created_range, page`
- 响应：`{ items: [...], total, page }`（码值掩码显示，详情页可揭示）。

### 2.5 `GET /admin/codes/{id}`（详情）

- 响应：`{ code, status, timeline: [发放→绑定→废弃→重发]（含操作者/时间/原因）, reissued_chain, bound_device?, receipt_continuity? }`

### 2.6 `GET /admin/devices`（设备列表）

- Query：`tenant_id, deploy_mode(native|docker), status, machine_code, page`
- 响应：`{ items: [{ device_id, tenant, machine_code(掩码), deploy_mode, image_digest?, last_heartbeat_at, lease_status, receipt_gap_summary }], total }`

### 2.7 `GET /admin/audit/logs`（审计日志）

- Query：`actor_type, action, entity_type, entity_id, time_range, page`
- 响应：`{ items: [{ ts, actor, action, entity, detail, ip }], total }`；支持导出。

## 3. 密钥轮换流程（5 步完整）

1. **生成**：新 `kid` 在 KMS / 离线隔离区生成，`signing_key.status = active`（与旧 kid 并存）；私钥不出隔离区。
2. **分发**：新签发的 Lease Token 使用新 `kid`；客户端内置**公钥集**（非单钥），可通过应用更新通道追加新公钥（更新包本身经代码签名，task 43）。
3. **双密钥并存**：旧 `kid` 置 `retiring`——**旧客户端仍可验签**（存量租约不失效），新签发一律走新 kid。
4. **旧密钥退役**：后台展示存量版本覆盖率（仍在用旧 kid 的活跃设备占比），达标后置 `retired`：不再签发、仅保留验签宽限期，最终拒绝旧 kid 验签。
5. **客户端公钥升级路径**：公钥集随应用升级分发 + 服务端公钥集查询端点兜底——**换密钥无需重发所有客户端**；全程写审计。

## 4. 管理后台自身安全（为 task 57 提供输入）

- **RBAC 四角色**：运营（只读 + 发放）、授权运营（发放 / 废弃 / 重发）、风控（回执异常页 / 审计只读 + 标记异常）、系统（密钥轮换 / 租户档位配置）；页面级 + 操作级双重门控。
- **双人复核（可选开关）**：废弃 / 重发高危操作开启后需第二管理员审批，`second_approver` 入审计。
- **全量操作审计**：所有 `/admin/*` 请求（含读）记 `audit_log`；审计页只读、可导出。
- **会话**：管理员登录态 + 超时；前端不持私钥、不直连授权数据库（仅经管理 API）。
- **回执异常处置边界**：异常页仅「标记异常（备注）→ 转人工核实」，**无「自动判定破解 / 自动封禁」动作**（回执异常 ≠ 破解）。

## 5. 口径对齐

- task 40/42：废弃 = 立即失效（≤24h 窗口）；403 语义与状态机迁移一致。
- task 41：`machine_code` / `anchor_hashes` 与冲突检测双重拒绝。
- task 44 前半：数据模型 9 表（`audit_receipt.gap_flag` 承接 §1.4 跳空检测）。
- task 57：RBAC 角色与权限矩阵输入；task 64：后台交互以本契约为依赖依据。
