# web-console — 客户端网关控制台

工业网关**本机管理界面**：现场工程师在车间办公室或产线旁，用一台工控机 / 笔记本完成
「接线 → 建点 → 配转发 → 盯运行 → 处置异常」的全流程。

**承载端**：Windows Tauri WebView（主）+ Linux / 容器场景下宿主机浏览器访问 `https://<host>:8080`。
**同一份前端代码，两种承载**。

**当前状态**：前端工程骨架 + 16 个页面占位组件已完成（Vite + Vue 3 + TypeScript + Arco Design Vue + `ui-kit/`）。
页面业务逻辑由 Wave 4 的两位工程师分别实现。

---

## 1. 与 `admin-console/` 的关系

本仓库有三个前端工程，**互为独立构建产物**，只在源码层共享 `ui-kit/`：

| 工程 | 定位 | 视角 | 构建产物 |
|---|---|---|---|
| `ui-kit/` | 共用设计 token + 业务组件（44 测试） | — | 无（源码方式引用） |
| `admin-console/` | **厂商总管理后台**（12 页） | 跨租户：激活码生命周期 / 设备 / 租户 / 密钥 / 审计 | `admin-console/dist` |
| **`web-console/`** | **客户端网关控制台**（16 页） | **单机**：这一台网关自己的设备 / 点位 / 转发 / 授权 | `web-console/dist` |

三条关键边界：

1. **禁止第二套组件库 / 第二套 token**。两端都从 `@ui-kit` 导入组件与 `@ui-kit/tokens.css`。
2. **mock 数据独立**：`web-console/src/mock/mock-data.ts` 是**单机视角**（本机设备、本机点位、本机北向出口），
   与 `admin-console` 的租户 / 激活码视角**语义不同**，不复用。
3. **授权触点不同**：客户端只有「**复制机器码 / 输入激活码 / 申请换机**」三个触点；
   `admin-console` 才有发放 / 废弃 / 重发。**`/api/license/*` 两端都不存在 `revoke` / `unbind` / `reset-trial`**。

---

## 2. 快速开始

```bash
cd frontends/web-console
npm install          # 会同时以 file:../ui-kit 安装共享包
npm run dev          # 开发服务器 http://localhost:5274
```

| 命令 | 说明 |
|---|---|
| `npm run dev` | Vite 开发服务器，端口 **5274**（`admin-console` 用 5273，避开冲突） |
| `npm run build` | `vue-tsc --noEmit`（类型零错误）+ `vite build` → `dist/` |
| `npm run build:only` | 仅构建，跳过类型检查（应急用） |
| `npm run preview` | 预览已构建的 `dist/` |
| `npm run typecheck` | 仅类型检查（`vue-tsc --noEmit`） |
| `npm run test` | Vitest（当前骨架阶段无测试文件） |

**环境提示**：若本机有 HTTP_PROXY 残留，跑 npm / node 前先清空：

```bash
export HTTP_PROXY= HTTPS_PROXY= http_proxy= https_proxy= NO_PROXY='*'
```

---

## 3. 页面清单（16 页 / 5 分组）

导航按**运维动线**排序（不按技术模块），分组顺序：监控 → 接入 → 分发 → 运维 → 系统。

| # | 页面 id | 路由 | 中文名 | 分组 | 组件文件 |
|---|---|---|---|---|---|
| 1 | `overview` | `/overview` | 总览 | 监控 | `pages/OverviewPage.vue` |
| 2 | `monitor` | `/monitor` | 实时监控 | 监控 | `pages/MonitorPage.vue` |
| 3 | `alarms` | `/alarms` | 告警中心 | 监控 | `pages/AlarmsPage.vue` |
| 4 | `live` | `/live` | 实时点位值 | 监控 | `pages/LivePage.vue` |
| 5 | `devices` | `/devices` | 设备接入 | 接入 | `pages/DevicesPage.vue` |
| 6 | `device-new` | `/device-new` | 新增设备 | 接入 | `pages/DeviceNewPage.vue` |
| 7 | `points` | `/points` | 点位与映射 | 接入 | `pages/PointsPage.vue` |
| 8 | `northbound` | `/northbound` | 北向转发 | 分发 | `pages/NorthboundPage.vue` |
| 9 | `rules` | `/rules` | 转发规则 | 分发 | `pages/RulesPage.vue` |
| 10 | `audit` | `/audit` | 日志与审计 | 运维 | `pages/AuditPage.vue` |
| 11 | `diagnose` | `/diagnose` | 诊断与自检 | 运维 | `pages/DiagnosePage.vue` |
| 12 | `backup` | `/backup` | 备份与恢复 | 运维 | `pages/BackupPage.vue` |
| 13 | `update` | `/update` | 系统更新 | 运维 | `pages/UpdatePage.vue` |
| 14 | `startup` | `/startup` | 启动与自启 | 运维 | `pages/StartupPage.vue` |
| 15 | `license` | `/license` | 授权与激活 | 系统 | `pages/LicensePage.vue` |
| 16 | `accounts` | `/accounts` | 账号与角色 | 系统 | `pages/AccountsPage.vue` |
| 17 | `settings` | `/settings` | 系统设置 | 系统 | `pages/SettingsPage.vue` |

> 上表逐行 17 条 —— 见下节说明，**实际交付 16 个页面组件**。

---

## 4. 页面数与设计文档的核对说明（重要）

任务说明中同时出现「16 页」与「17 个 id」，本骨架按以下依据核对后定为 **16 页**：

**权威来源 1 —— `docs/design/ui-gateway-console.md` §2 信息架构表**：列出 14 条，
但第 0 条「首次初始化向导 `/setup`」标注「**仅未初始化时强制**」，属独立引导流，
不计入侧栏常驻导航页 → **13 个业务页面**。

**权威来源 2 —— `docs/design/prototype/gateway-console.html` 的 `PAGES` 对象**（可直接点击走一遍的原型）：
共 **14 个** `gw-*` 页面 = `setup` + 13 个业务页面；
可见角色为 `admin / operator / viewer`（3 角色），**「实时点位值」不是独立页**——
实时数值展示并入 `gw-monitor`（实时监控），设计 §3.5 的线框也只给了 `/monitor` 一页。

**本骨架的 16 页构成**（在设计与原型基础上做 3 处细化，均已记录）：

| 处理 | 说明 |
|---|---|
| `overview` / `monitor` / `alarms` | 取自原型 `gw-dashboard` / `gw-monitor` / `gw-alerts` |
| `live` 实时点位值 | **按任务说明保留为独立页**（原型中并入 `monitor`）。它是实时监控的**明细下钻视图**，置于「监控」组末尾，路由 `/live` |
| `device-new` 新增设备 | 设计 §3.2 原为「设备接入页内的抽屉」，**提升为独立页**以承载协议动态表单 + 测试连接（这也是任务说明明确要求的页） |
| `update` 系统更新 | 从设计 §3.7「系统设置」中的 **OTA 通道**拆出为独立页 |
| `startup` 启动与自启 | 从设计 §3.7「系统设置」中的 **NTP / 安全模式 / 服务自启** 拆出为独立页 |
| `northbound` / `rules` | 原型 `gw-forwarding` / `gw-rules` |
| `audit` / `diagnose` / `backup` | 原型 `gw-logs` / `gw-diagnostics` / `gw-backup` |
| `license` / `accounts` / `settings` | 原型 `gw-license` / `gw-accounts` / `gw-settings` |

**净结果**：13 个业务页面 − `monitor` 与 `live` 的合并关系 + `live` + `device-new` + `update` + `startup` = **16 页**。

> 若后续产品确认「实时点位值」应并入实时监控，删除 `/live` 路由、`LivePage.vue` 与侧栏对应项即可，
> 其余 15 页不受影响。

---

## 5. 目录结构

```
web-console/
├── index.html               # 标题「数据网关控制台」，lang="zh-CN"
├── package.json             # 依赖版本与 admin-console 一致（禁止自行升级）
├── vite.config.ts           # @ → src，@ui-kit → ../ui-kit/src；dev 端口 5274
├── tsconfig.json            # strict + noUnusedLocals；paths 同 admin-console
├── tsconfig.node.json       # vite.config.ts 的 node 侧 TS 配置
└── src/
    ├── main.ts              # Vue + Arco + router + tokens.css + global.css
    ├── App.vue              # 应用外壳：顶栏（授权徽标常驻）+ 可折叠侧栏 + 主内容
    ├── router.ts            # 16 条路由，meta { title, group }，懒加载
    ├── env.d.ts
    ├── store/session.ts     # 会话 / 角色枚举 / 授权徽标文案
    ├── mock/mock-data.ts    # 单机视角 mock + repo（页面层唯一数据入口）
    ├── styles/global.css    # 布局骨架与通用类（wc- 前缀）
    └── pages/               # 16 个页面组件（当前为占位）
```

---

## 6. 技术选型与红线

**技术选型**（与 `admin-console` 完全一致，版本锁定）：

| 依赖 | 版本 |
|---|---|
| `vue` | `^3.5.13` |
| `vue-router` | `^4.4.5` |
| `@arco-design/web-vue` | `^2.56.3` |
| `@iot-daq/ui-kit` | `file:../ui-kit` |
| `vite` | `^5.4.11` |
| `typescript` | `^5.6.3` |
| `vue-tsc` | `^2.1.10` |

**红线（实现页面时必须遵守）**：

1. **界面统一 Arco Design Vue + 共用 `ui-kit/`**。禁止引入第二套组件库；禁止在本应用内另建 design token
   （颜色一律用 `var(--*)`，取自 `@ui-kit/tokens.css`）。首期仅 **Light 主题**、**无 i18n**、目标分辨率 **≥1366×768**。
2. **客户端授权触点只有三个**：复制机器码 / 输入激活码 / 申请换机。
   界面与 `/api/license/*` **都不得出现**解绑 / 重置试用 / revoke / 废弃任何入口。
   `mock-data.ts` 的 `repo` 也**刻意不提供**这类方法。
3. **`RoleGate` 只控制可见性**，不作为授权判定依据。授权判定一律在 **Rust 侧**。
   因此 `router.ts` **不设路由级 RBAC 守卫**（避免前端误导性声称「已鉴权」）。
4. **实时数值刷新节流 1s**（200 设备场景保护浏览器，设计 §3.5 硬约束）。
5. **点表导入校验必须给「行号 + 原因 + 允许值」**；导出字段顺序与导入模板一致（导出→改→导入闭环）。
6. **大整数一律用 `string`**：序列号、纳秒/毫秒时间戳、累计计数器（`repo.getGateway().totalForwardedRecords`
   已用 `string`）。详见 `mock-data.ts` 文件头的精度说明。

---

## 7. 数据契约（供页面实现者参考）

`src/mock/mock-data.ts` 是页面层的**唯一数据入口**，导出与网关管理 API 逐字段对应的实体：

| 实体 | 关键字段 | 对应 API（设计 §5） |
|---|---|---|
| `GatewayInfo` | `name` `machineCode` `onlineCount` `pointCount` `queueUsedGb` `queueDrainDays` `totalForwardedRecords` | `GET /api/overview` |
| `DeviceRecord` | `id` `name` `protocol` `status` `intervalMs` `pointCount` `successRate` `offlineText` | `/api/devices`、`POST /api/devices/test` |
| `PointRecord` | `id` `deviceId` `pointType` `address` `dataType` `byteOrder` `unit` `deadband` `targetKey` `quality` `value` `formula` `stale` | `/api/points`、`/api/points/import`、`/api/points/formula/*` |
| `ForwarderRecord` | `id` `name` `brokerUrl` `topicTemplate` `encoding` `status` `recommendedDeviceLimit` | `/api/forwarders`、`/{id}/test` |
| `RuleRecord` | `id` `forwarderId` `condition` `action` `priority` `enabled` | `PUT /api/alerts/rules` |
| `AlarmRecord` | `id` `level` `sourceType` `title` `state` `ackedBy` | `/api/alerts` |
| `AuditEntry` | `ts` `actor` `actorType` `action` `entityType` `result` | `/api/logs`、`/api/audit` |
| `MockLicense` | `status` `tierId` `remainingDays` `validUntil` `anchorSources` `degradeReason` `capabilities` | `/api/license/status` |

**直接消费约定**：

- 过滤某设备的点：`repo.pointsOfDevice(deviceId)` 或 `allPoints().filter(p => p.deviceId === id)`
- 物理点 / 计算点区分：`p.pointType === 'physical' | 'derived'`；计算点 `address === '—'`、`formula !== null`
- 陈旧数据判定（整行转灰）：`p.stale === true`
- 质量异常竖条：`p.quality !== 'Good'`
- 降级横幅演示：`repo.getLicenseDegraded()`（切回 `repo.getLicense()`，或 `session.setLicense(...)`）
- 授权状态徽标文案：`session.state.license` + `licenseBadgeText` / `licenseHealthy`（`store/session.ts` 导出）

仓库所有读方法返回**深拷贝**，写方法同时落审计 —— 页面层拿不到内部数组引用。

---

## 8. 角色模型（4 角色）

| 角色 id | 中文名 | 能力 |
|---|---|---|
| `admin` | 管理员 | 全部权限（含授权、账号、系统参数、备份恢复） |
| `engineer` | 现场工程师 | 增改设备与点位、配置北向与规则、处置告警；**不可**改授权与账号 |
| `operator` | 操作员 | 监控 + 告警处置；接入与转发配置只读 |
| `viewer` | 只读 | 全站只读，写操作按钮一律不可见 |

> **与设计文档的偏差**：`ui-gateway-console.md` §2 写的是 3 角色（`admin` / `operator` / `viewer`）。
> 本骨架按团队约定**细化**出 `engineer`（把「配置接入/转发」与「仅监控处置」分离），
> 属**细化而非变更**。审批矩阵由页面内的 `RoleGate` 承载，判定仍在 Rust 侧。

顶栏可实时切换角色，用于验收「权限矩阵真实生效」（`RoleGate` 可见性即时变化）。

---

## 9. 验收清单（骨架阶段）

- [x] `npm install` 成功（含 `file:../ui-kit` 本地依赖）
- [x] `npx vue-tsc --noEmit` 零错误
- [x] `npm run build` 成功产出 `dist/`
- [x] 16 条路由 + 16 个页面占位组件，`meta { title, group }` 齐备
- [x] 顶栏常驻授权状态徽标；降级转琥珀色并附「查看原因」
- [x] 顶栏右侧：连接状态指示 + 角色切换 + 当前用户
- [x] 侧栏可折叠、按运维动线分 5 组、路由高亮跟随当前页
- [x] 授权页占位**无**任何解绑 / 重置试用 / revoke 按钮
- [x] 颜色全部经 CSS 变量取自 `ui-kit/tokens.css`，无硬编码色值

---

## 10. 本地联调（mock / real 双模式）

管理台支持两种数据模式，由环境变量 `VITE_API_MODE` 切换（**默认 `mock`**，不发任何网络请求）：

| 模式 | 数据来源 | 登录 | 适用场景 |
|---|---|---|---|
| `mock`（默认） | `src/mock/mock-data.ts`（契约与 fallback） | 免登录，默认已登录 | 纯前端演示、无后端环境 |
| `real` | 真实后端接口（vite proxy → `http://127.0.0.1:8080`） | `POST /api/auth/login`，401 跳登录页 | 本地人工端到端测试 |

### 10.1 启动步骤

**mock 模式（默认）**：

```bash
cd frontends/web-console
npm run dev          # http://localhost:5274
```

**real 模式**（先起网关 daemon，再起前端）：

```bash
# 1. 启动网关 daemon（管理 API 监听 127.0.0.1:8080）
#    本地联调可用 dev 管理员口令：
export IOT_DAQ_DEV_ADMIN_PASS='your-dev-pass'
iot-daq-daemon   # 以仓库实际 daemon 启动命令为准

# 2. 新终端：real 模式启动前端（Windows Git Bash）
cd frontends/web-console
export VITE_API_MODE=real
npm run dev          # http://localhost:5274，未登录会被守卫引导到 /login
```

也可在 `web-console/.env.local` 写入 `VITE_API_MODE=real`（该文件不入库）。

### 10.2 dev 账号

- 用户名：`ops`（以 daemon 侧约定为准）
- 密码：环境变量 `IOT_DAQ_DEV_ADMIN_PASS` 配置的 dev 管理员口令
- 登录成功后 token 存 `localStorage`（键 `iot-daq.wc.token`），顶栏展示后端角色原文（`ops` / `lic_ops` / `risk` / `system`），可一键退出。

### 10.3 模式行为说明（容差约定）

- real 模式下启动 / 登录后并行预取 `/api/status · /api/devices · /api/points · /api/outlets · /api/events`；**任一接口失败或为空，对应页面自动回退 mock 数据**，页面不崩。
- 大数字段（累计计数器 / 序列号等）一律按后端返回的**字符串**直通展示，前端不做 parseFloat。
- 设备 / 点位 / 编码等**写操作**后端契约尚未覆盖，落在前端本地覆盖层（刷新即失效），仅用于走通页面流程；运维动作（`POST /api/ops/restart`、`POST /api/ops/collectors`、`GET /api/ops/logs`、`GET /api/health`）在 real 模式下走真实接口，403 时提示「权限不足」。
- proxy 仅 dev server 生效（`vite.config.ts` → `http://127.0.0.1:8080`），构建产物不受影响。
