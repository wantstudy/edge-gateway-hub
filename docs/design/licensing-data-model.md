# iot-daq 云端授权数据模型（对应计划 task 44）

> 配套图：`docs/design/licensing-server-and-admin-console.svg`。字段级 API 契约见 `licensing-api.md`（下一轮）。
> 核心关系：**tenant 1:N device；activation_code 1:1 device（可空绑定）；device 1:N lease；lease 1:N heartbeat / audit_receipt；signing_key 1:N lease（kid 签发）**；`nonce_cache`、`audit_log` 为独立支撑表。

## ER 图（9 表）

```mermaid
erDiagram
    TENANT ||--o{ DEVICE : "拥有"
    TENANT ||--o{ ACTIVATION_CODE : "订购发放"
    ACTIVATION_CODE |o--o| DEVICE : "绑定 1:1（激活时锁定）"
    ACTIVATION_CODE |o--o| ACTIVATION_CODE : "reissued_from 溯源"
    DEVICE ||--o{ LEASE : "1:N 租约随时间更迭"
    SIGNING_KEY ||--o{ LEASE : "kid 签发 Token"
    LEASE ||--o{ HEARTBEAT : "24h 心跳"
    LEASE ||--o{ AUDIT_RECEIPT : "B 档审计回执"
    DEVICE ||--o{ NONCE_CACHE : "防重放"

    TENANT {
        string tenant_id PK "租户ID"
        string name "名称"
        enum verify_mode_default "默认档位 A|B|C（B 默认）"
        string contact "联系人"
        datetime created_at "创建时间"
    }
    DEVICE {
        string device_id PK "设备ID"
        string tenant_id FK "所属租户"
        string machine_code UK "机器码组合指纹（HMAC加盐截断）"
        json anchor_hashes "锚点哈希集[5]（N-of-M 同机判定）"
        enum deploy_mode "部署形态 native|docker"
        string image_digest "镜像 digest（docker 时，溯源）"
        string host_anchor_ref "宿主锚点引用（容器形态只读挂载源）"
        datetime first_activation_at "首次激活时间（试用兜底锚点）"
        enum status "active|gracing|degraded|stopped"
        datetime created_at "创建时间"
    }
    ACTIVATION_CODE {
        string code_id PK "激活码ID"
        string code UK "码值（厂商发放）"
        enum status "issued|bound|revoked|reissued"
        string bound_device_id FK "绑定设备（1:1，未绑定时为空）"
        string tenant_id FK "所属租户"
        string tier "授权档位 tier"
        datetime valid_from "有效期起"
        datetime valid_until "有效期止"
        string source_order_id "来源订单"
        string reissued_from_id FK "溯源：原码ID（重发链）"
        string issued_by "发放操作者"
        datetime revoked_at "废弃时间（立即失效）"
        string revoked_reason "废弃原因（必填）"
        string idempotency_key "幂等键（revoke/reissue 同事务）"
        datetime created_at "创建时间"
    }
    LEASE {
        string lease_id PK "租约ID"
        string device_id FK "设备"
        string code_id FK "激活码"
        string kid FK "签名密钥（signing_key）"
        string token_sig "Lease Token 签名（Ed25519）"
        enum verify_mode "A|B|C（随 Token 下发）"
        string tier "档位"
        datetime issued_at "签发时间"
        datetime valid_until "到期时间"
        datetime last_heartbeat_at "最近心跳"
        enum status "active|gracing|degraded|stopped"
    }
    HEARTBEAT {
        string id PK "记录ID"
        string lease_id FK "租约"
        string device_id FK "设备"
        datetime client_ts "客户端时间（回拨检测）"
        datetime server_ts "服务端时间"
        enum result "ok|revoked|skew|unknown_device"
        string receipt_cursor "携带的最近回执序号区间"
        datetime created_at "落库时间"
    }
    AUDIT_RECEIPT {
        string id PK "回执ID"
        string lease_id FK "租约"
        string device_mid "设备机器码"
        int seq_from "起始序号"
        int seq_to "结束序号"
        int count "条数"
        string payload_digest "payload 摘要哈希（无业务数值）"
        datetime ts "客户端时间"
        string sig "设备签名"
        datetime received_at "接收时间（支持延迟补报）"
        bool gap_flag "跳空/回退/缺失标记（风控告警）"
    }
    NONCE_CACHE {
        string nonce PK "全局随机数（防重放）"
        string device_id FK "设备"
        datetime expires_at "过期时间"
        datetime used_at "使用时间"
    }
    SIGNING_KEY {
        string kid PK "密钥标识"
        enum status "active|retiring|retired（新旧并存轮换）"
        string public_key "公钥（下发客户端公钥集）"
        string hsm_ref "KMS/离线保管引用（私钥不落业务库）"
        datetime enabled_at "启用时间"
        datetime retired_at "退役时间"
    }
    AUDIT_LOG {
        string id PK "审计ID"
        enum actor_type "admin|system|device"
        string actor_id "操作者"
        string action "动作（issue/revoke/reissue/activation/...）"
        string entity_type "实体类型"
        string entity_id "实体ID"
        json detail "详情（原因/幂等键/命中项）"
        datetime ts "时间"
        string ip "来源"
    }
```

## 关键设计决策

1. **一机一码**：`activation_code.bound_device_id` 为可空外键，激活时锁定为 1:1；同码异机激活 → 冲突检测（`machine_code` 比对 + `anchor_hashes` N-of-M，task 41）→ 403 `CODE_BOUND_TO_OTHER_DEVICE`；**数据模型不提供任何客户端自行解绑 / 重置接口**。
2. **容器形态（与 task 41/59 口径一致）**：`device.deploy_mode`（native/docker）+ `image_digest` 供运维溯源；容器指纹来自**宿主锚点**（`host_anchor_ref`），**容器重建 / 升级不产生新 device 记录、不触发换机重发**；宿主重装系统 → 锚点变化 → 走后台「废弃 + 重发」。
3. **废弃语义**：`revoked_at` 即时生效（立即失效），`revoked_reason` 必填，原设备最迟下一次心跳被拒（风险窗口 ≤24h，task 42 决议）。
4. **B 档回执与跳空检测**：`audit_receipt` 仅存白名单字段（无业务数值）；服务端按 `seq_from/seq_to` 连续性检测**跳空 / 回退 / 缺失**，置 `gap_flag` → 风控告警（`audit_log`），**人工核实、不自动封禁**；断网允许延迟补报（`received_at` 可晚于 `ts`）。
5. **私钥隔离**：`signing_key` 表只存公钥与 `hsm_ref`（KMS / 离线保管引用），**私钥不落业务数据库、不与业务 API 同一可写位置**；`status` 支持新旧密钥并存轮换（旧客户端仍可验签）。
6. **防重放**：`nonce_cache` 全局唯一约束 + `expires_at` 过期清理；A 档 `/verify` 与激活共用。
7. **档位配置**：租户级默认（`tenant.verify_mode_default`）+ 设备级覆盖（随 Lease Token 下发 `lease.verify_mode`）；切换仅经后台，客户端无档位入口。
8. **审计闭环**：后台全量操作、网关心跳 / 回执、风控告警均写 `audit_log`（actor / action / detail），支撑 admin-console 审计页与溯源时间线。

## 生命周期与状态枚举汇总

| 表 | 状态枚举 | 迁移触发者 |
|----|---------|-----------|
| `activation_code.status` | issued / bound / revoked / reissued | issued→bound：设备首次激活（服务端）；bound/issued→revoked：仅总管理后台；revoked→reissued：仅总管理后台（生成新码） |
| `device.status` | active / gracing / degraded / stopped | 由租约状态机驱动（task 42），服务端心跳结果回写 |
| `lease.status` | active / gracing / degraded / stopped | 心跳结果 + 时间守卫 |
| `signing_key.status` | active / retiring / retired | 密钥轮换流程（后台系统操作） |
