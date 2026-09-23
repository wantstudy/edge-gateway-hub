# ui-kit — 两端共用前端基础包

网关客户端（`web-console/`）与厂商总管理后台（`admin-console/`）共用的设计 token 与业务组件包。

**当前状态**：目录占位，由 **Wave 4 task 27（Vue 界面骨架 + ui-kit 引导）** 填充，本阶段不引入任何前端依赖。

规划内容（对应 `docs/design/ui-design-system.md`）：

- 设计 token（色彩 / 字体 / 间距 / 状态色，Light 主题，Arco Design Vue 基线）
- 共享业务组件：`StatusTag`、`MachineCodeDisplay`、`CodeLifecycleTimeline`、`QueueGauge`、`QualityBadge`、`EncodingRadio`、`DangerConfirmModal`、`PointTableEditor`、`LogViewer`、`DiagPanel`、`RoleGate`

约束：Light 主题、无 i18n、目标分辨率 ≥1366×768；危险操作组件一律走 `DangerConfirmModal`（原因必填）。
