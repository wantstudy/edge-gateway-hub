# Draft: IoT 数据采集网关项目

## 需求概述（用户原话要点）
- 设备数据采集，兼容 Modbus、HTTP、PLC、OPC 等协议
- 接收数据后处理，发送到 MQTT；支持 MQTT 转发
- 支持 Linux、Windows
- Windows 使用安装包形式（Tauri 开发）
- Linux 使用 Vue 开发（形态待澄清：headless 网关 + Web 管理界面？）
- 收费模块：机器码激活、防破解、免费试用 3 天
- 用户提问：授权体系下，每个数据可否隐式加入机器码做"二次教研"（=二次校验？）

## DeepSeek 给出的建议（用户粘贴，作为讨论基础）
- 四层架构：协议适配层(南向) / 数据处理层 / MQTT分发层(北向) / 系统管理层
- 断网续传（内存队列 + SQLite 磁盘缓存，环形覆盖）
- 远程运维 + OTA + Web 管理界面
- 授权：多硬件源机器码（Windows MachineGuid+WMI UUID；Linux machine-id+product_uuid）、软授权 License + 硬授权加密狗、先部署后激活
- 防破解：Rust 侧实现核心逻辑、服务端二次验证心跳、代码混淆、试用期加密存储+多重标记
- 补充点：数据质量/时间戳统一、等保2.0安全合规、PLC4X 驱动生态、设备模板、性能指标压测、日志可观测性、跨平台打包策略(NSIS/AppImage)、商业分层(试用期后降级基础版)
- P0：Modbus TCP/RTU + MQTT 核心链路、断网续传、授权体系

## 授权体系二次校验方案（对用户问题的分析）
- 术语修正："隐式加入机器码"更准确的叫法是"数据级授权绑定 + 服务端验签"
- 不推荐字面隐写（浮点尾数/时间戳微秒嵌入机器码）——破坏工业数据精度，易被过滤
- 推荐：机器码哈希(mid) + 非对称签名(Ed25519) + 时间戳 + nonce + 服务端授权白名单 + 短期 Token
- 关键边界：二次校验必须依赖"数据经过你控制的服务器/Broker 插件"；客户自建 Broker 则无法强制
- 离线场景需宽限期 + 联网补验

## 尚未澄清的关键问题
- [ ] Windows(Tauri) 与 Linux(Vue) 的具体形态差异
- [ ] MQTT Broker 归属：客户自建 vs 厂商云平台
- [ ] 授权服务端：是否由厂商运营
- [ ] 协议优先级与首发范围
- [ ] 核心语言：Rust 统一 vs 多语言
- [ ] 性能指标目标（设备数/频率/缓存天数）
- [ ] 试用到期策略：降级免费版 vs 停用
- [ ] 团队规模与 Rust 熟练度
- [ ] 断网续传是否 P0 确认
- [ ] 界面功能范围（配置/监控/日志/OTA）

## 已确认的决策（第一轮访谈）
- 交付形态：Windows = Tauri 桌面应用（含 WebView 管理界面）；Linux = 无 GUI 网关服务 + Vue Web 管理界面（浏览器访问）
- 数据去向：默认客户自建 MQTT Broker；厂商云平台为增值服务（企业版）
- 授权：厂商运营云授权服务，本地不可离线激活（激活动作必须联网）
  - ⚠️ 引申问题：激活后断网能用多久？（离线宽限期待确认）
  - ⚠️ 数据不走云平台时，二次校验退化为"客户端强签名"（授权失效→停止签名/停止发送）
- 协议范围：Modbus RTU + Modbus TCP、MQTT 上报+转发、OPC UA、西门子 S7、三菱 MC、HTTP 采集/上报（全选）

## 已确认的决策（第二轮访谈）
- 授权：激活必须联网；激活后离线宽限 7 天，每 24h 心跳一次；超期未心跳 → 本地停止采集/发送
- 试用到期：降级免费基础版（限协议数/设备数，如 Modbus 8 设备），不直接停用
- 性能指标：单网关 50-200 设备、采集频率 ≥100ms、离线缓存 ≥7 天
- 架构：共享 Rust 核心（驱动+处理+MQTT+授权），前端统一 Vue（Tauri 内嵌 WebView 与管理界面同代码）

## 已确认的决策（第三轮访谈 - 用户给出完整产品蓝图）
- 产品定位：工业边缘数据汇聚与统一分发网关（南向异构数据 → 北向统一 MQTT 出口）
- PLC 驱动：自研西门子 S7 (S7comm) + 三菱 MC (3E/4E帧)，rust 原生实现；内部 async_trait 逻辑插件化；后续 Sidecar 进程桥接 PLC4X/第三方（避免 JVM 桥接膨胀）
  - 地址解析器（DB1.DBX0.0/M100/D100 → 字节偏移）、大端/小端字节序、断线自动重连（退避策略）为自研重点
- 第三方 MQTT 接入：支持多 Broker 并发连接、动态主题订阅、Payload 解析（JSONPath/正则）→ 统一数据模型
- 数据处理：点位映射、单位换算、死区过滤、Topic 路由映射
- 统一数据格式：用户蓝图明确 Protobuf（TelemetryBatch + DataPoint + AuthBlock）；⚠️与第三轮"双格式可配置"选择存在冲突，待定案
- 断网续传：北向失败 → SQLite 持久化队列，按序补发，环形覆盖上限（10GB/7天）
- 本地全量存储：SQLite（WAL）或 DuckDB，数据不出厂 + 本地报表查询能力
- 授权：多硬件源锚点机器码（系统UUID+磁盘+MAC+主板）；云端下发 Lease Token（Ed25519 签名，本地内置公钥验签）；离线宽限7天超期停止北向转发（保留本地采集）
- 消息级二次校验：AuthBlock（机器码哈希+时间戳+Nonce+签名）；对业务语义做确定性哈希后签名（非直接签序列化字节）；云端校验签名+时间窗口±5分钟+Nonce 防重放
- 本地安全：MQTT 强制支持 TLS/mTLS；Web 管理界面强制账号密码+HTTPS（自签名证书，JWT）；敏感配置+本地数据库 SQLCipher 透明加密，密钥由机器码 HKDF 派生（拷盘即失效）；审计日志（登录/配置修改/授权失败）
- 试用：3天，本地加密试用标记 + 云端首次激活时间，防重装/改时间
- 商业模式：基础版（本地存储+有限协议）/ 专业版（全协议+多路第三方 MQTT+北向统一转发）/ 企业版（私有化+定制驱动+维保）；核心收费点 = 北向 MQTT 转发，到期不续停止转发、保留本地存储
- 管理界面功能（全选）：配置管理、实时监控面板、日志与审计、OTA升级、远程运维
- 通信安全基线：TLS + 界面鉴权 + SQLCipher 密文存储（等保二级基线）

## 已确认的决策（第四轮访谈 - 最终）
- 测试策略：TDD 全链路（项目从零开始，需先搭建测试基础设施：cargo test / vitest / protobuf 生成校验）
- 数据格式定案：Protobuf 为主（TelemetryBatch/DataPoint/AuthBlock），JSON 为调试/兼容模式
- 免费基础版限制：Modbus TCP/RTU 仅 8 设备、采集频率限 ≥1s、无北向 MQTT 转发（本地存储保留）、无 OTA
- 团队：4-8 人中团队，可并行多条线（内核/前端/授权服务/测试）

## 默认决策（待计划中披露，用户可覆盖）
- 本地存储引擎：SQLite（WAL 模式）+ SQLCipher 透明加密（生态成熟）；DuckDB 后续用于本地报表查询
- 主题规划：{projectId}/{gatewayMid}/{namespace}/telemetry 等
- 规则引擎：V1 采用声明式 JSON 规则（点位映射/单位换算/死区过滤/Topic路由），后续再演进

### Metis 审查结果
Metis 咨询两次任务被中止（超时），已跳过。缺口分析由规划者自审 + librarian 研究结果覆盖。

### 研究来源（librarian, 2026-09 生态现状）
- **Tauri 2.x**：稳定版 2.11.5 (2026-07)；Windows NSIS/MSI（MSI 仅 Windows 构建）；Linux AppImage/deb/rpm；交叉编译限制（cargo-xwin 实验性，MSI 不能用）；推荐 GitHub Actions 三平台矩阵原生构建；v1→v2 破坏性变更（package/tauri key 重命名、capabilities 权限系统、updater 插件化）。
- **机器指纹**：`machine-uid 0.6.0`（MachineGuid=/etc/machine-id）、`mid 5.0.1`（多源组合+哈希，Windows WMI + Linux product_uuid）、`tauri-plugin-machine-uid 0.1.3`；MachineGuid 是安装 ID 非硬件 ID（重装失效），建议多源组合 + HMAC。
- **驱动生态**：Neuron LGPL-3.0 插件模型可参考架构（节点/组/标签/北向订阅路由）；PLC4X 0.13.1 Apache-2.0 但 Rust 绑定未生产可用；`tokio-modbus 0.17.0`（Modbus RTU/TCP/ASCII）；`async-opcua 0.19.0`（freeopcua，MPL-2.0，活跃）。
- **MQTT**：`rumqttc 0.25.1`（Apache-2.0，rustls）vs `paho-mqtt 0.14.0`；⚠️ rumqttc 生态分裂（rumqttc-next 0.33.3 活跃）；EMQX/NanoMQ 规则引擎 SQL 语义可参考。
- **SQLite**：`rusqlite 0.40.1`（MIT，单连接+bundled SQLite 3.53.2）；`sea-orm 2.0.0`；写密集：单写连接 + WAL + 批量事务（SQLite 单写者坑）。

## 范围边界（项目级）
- INCLUDE：南向 6 类协议（Modbus RTU/TCP、OPC UA、西门子S7、三菱MC、HTTP、第三方MQTT）、数据处理、断网续传、本地存储、北向 MQTT 转发、Web/Tauri 管理界面、授权体系（机器码/试用/Token/签名）、云授权服务、TLS/加密/审计、OTA
- EXCLUDE（首期不做，后续路线）：PLC4X/第三方驱动 Sidecar、DuckDB 本地报表引擎、等保三级、硬件加密狗、多语言界面国际化、容器化交付