# 工业边缘数据汇聚与统一分发网关

## TL;DR
> **Quick Summary**: 构建跨平台（Windows Tauri 桌面 + Linux headless 服务/容器）工业边缘网关，采集 Modbus/OPC UA/S7/MC/HTTP/第三方 MQTT 等南向异构数据，经处理后通过北向 MQTT 统一分发。核心 Rust 库跨平台共享，Vue 前端复用。授权体系采用云端 Lease Token（Ed25519）+ 消息级 AuthBlock 二次校验 + 7 天离线宽限。
>
> **Deliverables**:
> - `docs/design/` — **设计图先行交付物**（授权与防破解总体图、一机一码指纹图、激活与激活码生命周期状态机、安装程序防破解图、云端授权服务与总管理后台设计、容器化交付设计）
> - `docs/design/ui-design-system.md`、`ui-gateway-console.md`、`ui-admin-console.md` — **界面设计（已产出）**：设计系统与 token、客户端与管理后台的信息架构/页面清单/线框/交互约定/验收要点
> - `docs/design/prototype/` — **可点击高保真原型**（`gateway-console.html`、`admin-console.html`，单文件零依赖，Light 主题，含危险操作弹窗与角色门控）
> - `docs/design/prototype/gateway-v2a-glacier.html` — **★ 客户端界面 v2 终稿（方案 A「冰川」已经用户选定，实现以此为准）**：16 个页面，含「新增设备」独立向导页、点位映射主从布局、全列表分页；`gateway-v2b-graphite.html` 为备选未采用，仅存档对比
> - `crates/daemon` — 核心守护进程库（驱动 + 处理 + 缓存 + MQTT + 授权 + 管理 API + 进程装配）
> - `crates/licensing-server` — 云授权服务（激活、心跳、激活码生命周期、Token 签发、服务端二次校验）
> - `tauri-shell/` — Windows Tauri 桌面应用（NSIS/MSI 安装包，含安装包签名与防篡改）
> - `headless/` — Linux 网关服务（原生 AppImage/deb/rpm + systemd，同时作为容器镜像的运行体）
> - `deploy/docker/` — **Linux Docker 交付包**（多架构镜像构建、docker-compose、离线镜像 tar 打包、一键安装脚本、宿主机指纹注入与持久卷布局）
> - `web-console/` — 网关侧 Vue 管理界面
> - `admin-console/` — **厂商总管理后台**（激活码发放 / 绑定 / 废弃 / 重发，设备与租户管理）
> - `ui-kit/` — **两端共用前端基础包**（设计 token + 共享业务组件：StatusTag / MachineCodeDisplay / CodeLifecycleTimeline / QueueGauge / QualityBadge / EncodingRadio / DangerConfirmModal / PointTableEditor / LogViewer / DiagPanel / RoleGate）
> - `ci/` — GitHub Actions 双平台矩阵 CI（含多架构镜像构建）
> - 完整测试基础设施与 TDD 用例
>
> **Estimated Effort**: Large (~71 tasks, 11 waves + 终审波)
> **Parallel Execution**: YES — 每波目标 5-8 任务，Wave 0 为串行确认点
> **Critical Path**: **Wave 0 设计（授权 / 防破解 / 一机一码 / 激活码生命周期 / 容器化 / 两端界面）→ 用户确认 → Monorepo → 数据模型 → Driver trait → Modbus/S7 驱动 → 数据处理 → MQTT → 授权闭环（licensing-server + 防破解 + 激活码管理）→ 界面 → 打包（含 Docker 镜像与离线包）→ F1-F4**
> **P0 优先项**: **Wave 0 只出设计图、不写实现代码**；设计图经用户确认后才进入 Wave 1。安装程序防破解、激活、一机一码、换机重发（总管理后台废弃旧码 → 重新发放）为本项目最高优先级。**界面设计（task 63/64）已产出**：`docs/design/ui-*.md` 三份 + `prototype/` 两份可点击原型，经真实浏览器验证 31/31 项通过。**客户端界面 v2（方案 A）已产出并经真实浏览器验证 178 项断言全通过**（`.omo/evidence/v2-A-*.png` 36 张干净态截图，含贴牌预览 / 模拟策略页签 / 北向消息示例）。**方案 B 已按用户要求停止产出**（第五轮反馈：只采用方案 A，不再出 B 的设计与截图；`gateway-v2b-graphite.html` 与 `v2-B-*` 证据已删除）。
>
> ✅ **阶段状态（2026-09-23）：Wave 0 设计已定稿（用户 explicit 确认），项目进入开发阶段。当前执行 Wave 1（task 1-7：Monorepo / Protobuf schema / 机器码指纹 / 配置 / 日志 / 错误类型 / 测试基建）。** 本机环境约束：Rust 工具链未预装，采用 rustup `stable-x86_64-pc-windows-gnu`（自包含 MinGW 链接器，无需 VS Build Tools），`CARGO_HOME=D:\rust\cargo`、`RUSTUP_HOME=D:\rust\rustup`；protoc 不依赖系统安装（构建脚本经 `protoc-bin-vendored` 或 prost derive 规避）。
>
> **Wave 1 实施记录（团队 software-iotdaq）**：
> - ✅ task 1（M1）：workspace 三 crate + 双平台 CI + 占位目录 + .gitignore 密钥黑名单，`cargo build --workspace` 通过。
> - ✅ task 2（M2）：prost-build + `protoc-bin-vendored`（.proto 唯一 schema 源）；TelemetryBatch/DataPoint/AuthBlock + 4 项往返测试。
> - ✅ task 3（M2，**已批准偏差**）：以「AnchorProvider trait + fake 测试 + 平台锚点接线清单」替代 mid 5.0.1 crate 集成——理由：mid 依赖 WMI/COM 违背 Wave 1 纯 Rust 约束；真实注册表/sysfs/DMI 采集归 Wave 6 接线任务。N-of-M 容错 + HMAC-SHA256 聚合逻辑完整可测（12 单测）。
> - ✅ task 4/5/6/7（M3）：配置解析（toml，6 测试）、tracing 日志（7 测试）、thiserror 错误体系（5 测试）、测试基建 + CI 门禁（fmt→build→test→clippy -D warnings）。**Wave 1 全量 36 测试通过，三道门禁绿，工作区 clean**。
> - ✅ 提交：`44e7f2b` chore(wave1): monorepo scaffold（69 文件）；`f411069` feat(wave1): protobuf schema + machine fingerprint + config + logging + error（16 文件）。
> - ⚠️ 工具链坑（已记录进 README 构建节）：① PATH 中 `/d/rust/mingw64/bin` **必须排在** rustup gnu `self-contained` 之前（工程师实测，顺序错则 raw-dylib/dlltool 构建失败）；② mingw-w64 独立工具链已装至 `D:\rust\mingw64`（为 Wave 2b rusqlite bundled 铺路）；③ tempfile 钉版 3.14.0 已随 mingw64 就绪**解除**（3.27.0 回浮）。另：task 7 前端 vitest 未实施（前端尚无工程，归 Wave 5 UI 基建补齐，已批准偏差）。
> - ⏳ QA 独立回归进行中（团队任务 #12，software-qa-engineer 严过关）。
> **Wave 2 / 2b / 3 实施记录（2026-09-23，主理人 + 并行工程师团队）**：
>
> - ✅ task 8/9 南向驱动（opencode 产出，已审查合格）：Driver trait + PointAddressParser + 地址偏移正确
>   （mock server 侧断言 `ReadHoldingRegisters(0,2)` 证明 40001→PDU 地址 0）+ Modbus TCP / RTU-over-TCP。
> - ✅ task 15 数据处理内核 `DataProcessor`（映射 / 单位换算 / 死区 / 时间戳统一，14 单测）。
> - ✅ task 16 组轮询调度器（907 行 / 11 测试）：一组一 task 一 interval + `tokio::time::pause/advance`
>   零真实等待；QA 场景「组 A 10 拍 vs 组 B 1 拍」精确成立；故障组仍被调度且不拖慢健康组。
> - ✅ task 19 MQTT 客户端（1862 行 / 24 测试）：rumqttc + rustls，回环 mock broker 上的真实
>   PUBACK/SUBACK 往返 + 断线重连 `session_present` 断言（含 clean_session=true 对照实验）；
>   encoding 只声明不实现（序列化归 task 62）。
> - ✅ task 20 规则引擎（1426 行 / 16 测试）：SELECT/WHERE/DO + JSONPath 重映射；数值变换与死区
>   **严格委托 task 15 的 DataProcessor**；畸形规则一律 ConfigError(2000)。
> - ✅ task 21 AuthBlock 签名（1002 行 / 20 测试）：签**业务语义哈希**（排序 + 长度前缀），
>   签名域 `iotdaq.authblock.v1|ph|mid|ts|nonce`；授权闸门关闭时**不产出签名**；私钥不落盘。
> - ⚠️ 新坑：rumqttc/tokio-rustls 引入的 rustls **未启用任何 crypto provider**（`get_default()` = None，
>   mqtts 握手会失败）→ 已在 daemon 依赖显式启用 rustls `ring` 特性（不用 aws-lc-rs：cmake/NASM），
>   并在 `to_transport()` 内 `ensure_rustls_provider()` + 回归测试 `tls_crypto_provider_is_installed` 双重守护。
> - ✅ 依赖口径拍板：**vendored C 源可用**（rusqlite 0.40.2 bundled 已实测 mingw 构建 EXIT=0），
>   红线禁的只是需要系统 C 库/工具链的依赖（openssl-sys 非 vendored / paho-mqtt / aws-lc-rs）。
> - 提交：`47c54dc` 数据处理、`bdabb6e` 调度器、`96e0d2a` 规则引擎、`150d148` 签名、`fd3800f` MQTT。
>   daemon 143 测试全绿，clippy `--all-targets -D warnings` 零告警。
>
> **Wave 3 第二轮（2026-09-23 深夜）**：task 17 断网续传队列（queue.db 独立库 + 单写线程 + WAL +
> 三级背压 + high-water mark 续传，14 测试）与 task 62 北向双编码器（protobuf/json 双路径语义一致
> 且验签一致、超 2^53−1 走字符串、MQTT5 属性声明，12 测试 + 性能基线：体积 3.43× / 编码 8.57×）。
> daemon 169 测试全绿。此轮受平台 429 频率限制打断，两个模块的收尾由主理人接续修复（共 13 处，
> 含写线程指令循环缺失、seq 偏移 1、高水位字段硬编码为 0 三个功能性缺陷）。
>
> **Linux 交付形态**: **Docker 容器为主**（可选原生 deb/rpm + systemd）。容器化直接冲击「一机一码 + 试用期」两条防破解主线——**机器码必须锚定宿主机、试用与授权状态必须落宿主机持久卷**，否则 `docker rm && docker run` 即可重置试用、授权也会失效。
> **二次校验与北向编码（本轮定案）**: 二次校验**按 A/B/C 三档并存、默认 B**——B 档下业务数据直连客户 Broker（不经厂商），厂商只收**审计回执**（心跳 + 序号区间 + 条数 + 摘要哈希，**不含业务数值**）。北向编码为**每路出口独立可选**（`protobuf` 默认 / `json`），JSON 路径**超 2^53−1 的整数必须转字符串**。防破解力度定案 **Tier-1**（预算 控制面 7 : 客户端 3）。

---

## Context

### 原始需求
设备数据采集网关，兼容 Modbus、HTTP、PLC、OPC 等协议，接收后处理发 MQTT，支持 MQTT 转发。Windows 用 Tauri 打包安装包，Linux 用 Vue 开发。收费模块：机器码激活、防破解、3 天免费试用。

### 访谈决议汇总
| 维度 | 决议 |
|---|---|
| 交付形态 | Windows = Tauri 桌面（含 WebView 管理界面）；Linux = **Docker 容器（主推形态，可选原生 deb/rpm + systemd）** + Vue Web 管理界面（浏览器访问宿主机端口） |
| Linux 容器交付 | Docker 镜像 + `docker-compose`；多架构 `linux/amd64`、`linux/arm64`（工业现场常见 ARM 网关盒）；**离线镜像 tar 包**（`docker save`/`docker load` + 一键安装脚本），适配无外网现场；镜像 cosign 签名 + digest 固定；数据/日志/配置落宿主机持久卷 |
| 容器内授权锚点 | 机器码**必须取自宿主机**（只读挂载宿主 `/etc/machine-id`、`/sys/class/dmi/id/*`，宿主 MAC 由安装脚本经环境变量/指纹文件注入）；**禁止采容器内 machine-id / 容器 MAC / 容器主机名**——容器重建会导致其变化，授权随即失效 |
| 容器内防破解 | 试用标记、租约、授权状态**必须落宿主机持久卷**（不得只写容器可写层）；`docker rm && docker run` 重建容器不得重置试用、不得改变机器码 |
| 容器内设备接入 | 串口设备经 `--device /dev/ttyUSBx --group-add dialout` 映射（不用 `--privileged` 全开）；南向设备位于宿主局域网，用 `--network host` 保证直连与广播发现 |
| 核心架构 | 共享 Rust 核心（驱动+处理+MQTT+授权），前端统一 Vue |
| 数据去向 | **默认 B 档**：客户自建 MQTT Broker（业务数据不经厂商）；厂商云平台为企业增值服务（A 档） |
| 二次校验分档 | **A/B/C 三档并存，默认 B**。A 档 = 数据经厂商云 → 完整校验（验签 + ±5min + 全局 Nonce + 白名单 + 计量）；**B 档（默认）= 直连客户 Broker → 网关侧本地签名 + 单调序号 + 本地去重，另开控制通道上报审计回执（心跳 + 序号区间 + 条数 + 摘要哈希，不含业务数据）**；C 档 = 纯本地（大客户可选，仅保留租约心跳）。**不变式：「授权判定在网关 Rust 侧」不受档位影响**；B 档下**不得要求在线才能转发**（断网必须继续采集与转发） |
| 授权 | 厂商运营云授权服务，激活必须联网；离线宽限 7 天，每 24h 心跳；超期停止北向转发（保留本地采集） |
| 协议范围 | Modbus RTU/TCP、OPC UA、西门子 S7、三菱 MC、HTTP、第三方 MQTT 接入 |
| 数据格式 | **北向编码为「每路出口独立可选」的一等配置项**：每路 MQTT 连接各自声明 `protobuf`（默认）或 `json`。四个约束：① 超出 2^53−1 的整数**必须编码为字符串**（JSON number 是 IEEE754 double，纳秒时间戳/大 uint64 会静默丢精度）；② **编码与签名解耦**——同一数据两种编码语义一致、验签结果相同（跨端一致性测试项）；③ MQTT 5 用 `Payload Format Indicator` + `Content Type` 显式声明编码（3.1.1 无属性则 topic 后缀或 payload 内标注）；④ JSON 模式需给出性能基线与推荐设备上限（体积约 1.5–3×） |
| 防逆向力度 | **Tier-1 必做**（代码签名、strip/LTO/panic=abort、字符串与常量混淆、段哈希自检、低成本反调试）；Tier-2 暂不做（控制流平坦化、字符串加密、交叉校验、Frida 检测）；Tier-3 明确不做（内核驱动、VMProtect/Themida 级壳、硬件加密狗）。**预算 控制面 : 客户端 = 7 : 3**；量化目标 = **拦掉 90% 非专业破解尝试，专业破解者付出 > 数人日仍无法获得云端有效授权**；定位声明：**提高成本 ≠ 绝对防破解** |
| 公式计算落点 | **点位表一等公民（派生点 / 计算点）**，不是规则引擎的附属能力。点位表 `point_type = physical \| derived`：物理点来自现场，计算点由公式产出。公式**只写在点位表层**（`crates/daemon` 的数据处理链路内），**不进任务 20/37 的转发规则**（规则引擎负责「转发什么、去哪」，公式负责「值怎么算」） |
| 公式引擎形态 | **受限表达式引擎（AST 编译）**，非脚本、非 eval：词法+语法分析 → AST → **编译期白名单校验** → 求值；**编译一次缓存 AST**，禁止每周期重新解析。函数白名单：`abs ceil floor round clamp min max sqrt pow exp log log10`（数学）、`if(cond,a,b)`（条件）、`prev(x)`（上周期值）、`delta(x)` / `rate(x)`（变化量/变化率）、`hold(x,n)`（保持）、`quality(x)`（质量码）。**禁用**：赋值、循环、字符串操作、I/O、反射、动态求值、任意脚本（Lua/JS） |
| 公式求值顺序 | **写死的单一时序**：原始值 → 解码（53）→ **单位换算到工程单位（15）** → **公式求值（70，按依赖拓扑序）** → 死区过滤（15）→ 输出（本地存储 + 北向转发）。**公式必须拿到已换算到工程单位的值**（否则 `A+B` 单位混算）；同周期内**物理点全部处理后**才跑公式（周期屏障） |
| 公式依赖与环 | 公式可引用其他计算点 → 依赖关系构成 **DAG，必须做环检测**；保存期即校验，报错**必须给出环路径**（`R_Cost → R_Energy → R_Cost`）而非只说「存在循环」；拓扑排序决定求值顺序。**导入点表时须做整表级环检测**（逐行校验无法发现跨行成环：A、B 各自合法、合起来成环） |
| 公式失败与质量 | 输入缺失 / 超时 / 非数值 / 除零 / 结果 NaN·±Inf → 输出 `quality = calc_failed`（或**继承输入最差质量**）；失败策略可配：`hold_last`（保留上次有效值 + 质量标记，默认）/ `null`（置空）/ `skip`（不输出）。**公式变更只对新增数据生效，历史数据不重算**（工业惯例，避免「改公式历史值全变」）；公式变更属关键变更，**必须写审计** |
| 性能指标 | 单网关 50-200 设备、采集频率 ≥100ms、离线缓存 ≥7 天 |
| 试用期 | 3 天，降级免费基础版（Modbus 8 设备、≥1s 频率、无北向转发） |
| 安全基线 | MQTT TLS/mTLS，Web 界面强制账号密码+HTTPS，敏感配置 SQLCipher 加密（HKDF 派生密钥），审计日志 |
| 测试策略 | TDD 全链路（cargo test / vitest / protobuf 校验） |
| 团队 | 4-8 人中团队，可并行 |
| 交付优先级 | **P0 = 设计图先行**。授权体系 / 安装程序防破解 / 激活 / 一机一码 / 换机重发 必须先产出设计图并经用户确认，才进入实现 |
| 防破解重点 | 安装程序防破解：安装包签名与完整性校验、反调试/反篡改、核心逻辑在 Rust 侧、试用标记多重冗余存储、系统时钟回拨检测 |
| 一机一码 | 一个激活码只绑定一台设备的机器码（多源锚点指纹）；绑定关系由云端持有，本地仅持签名租约 |
| 换机迁移 | 厂商在**总管理后台**废弃原激活码 → 重新发放新激活码 → 新机器激活并绑定；旧码立即失效，原设备授权随之失效 |
| 厂商侧后台 | 新增 `admin-console/` 总管理后台（激活码 / 设备 / 租户 / 租约管理），与网关侧 `web-console/` 分离 |
| 界面设计 | **Wave 0 先出界面设计再写页面**：设计系统（token + 共享组件 `ui-kit/`）+ 两端信息架构与页面清单 + 可点击原型。**两端统一 Arco Design Vue**（替换原 Element Plus，避免两套设计语言双份维护成本）；首期仅 Light 主题、无 i18n、目标分辨率 ≥1366×768 |
| 客户端页面清单 | 总览 / 实时监控 / 告警中心 / 设备接入 / 点位与映射 / 北向转发 / 转发规则 / **授权与激活** / 日志与审计 / 系统设置 / 账号与角色 / 诊断与自检 / 备份与恢复 + **首次初始化向导** |
| 管理后台页面清单 | 总览 / 激活码管理 + 详情 / 设备管理 + 详情 / 租户与策略 / **回执与异常** / 换机工单 / 签名密钥管理 / 审计日志 / 账号与角色 |
| 角色可见性（RBAC） | 网关侧 `admin`（全部）/ `operator`（接入与转发配置可写，授权与系统只读）/ `viewer`（只读）；厂商侧 `admin` / `ops` / `viewer`。**前端仅控制可见性，判定一律在后端与 Rust 侧** |
| 关键交互硬约束 | 实时数值刷新**节流 1s**（200 设备场景保护浏览器）；空缺态必须给下一步动作；危险操作一律二次确认 + 原因必填；降级必须可解释（写明原因与恢复路径）；授权状态常驻顶栏 |
| 点位表批量操作 | **必须支持 CSV/XLSX 批量导入导出**（工业现场动辄数百点位），校验结果须给出「行号 + 原因 + 允许值」，且导出文件与导入模板字段顺序一致（导出→改→导入闭环） |
| 客户端授权触点 | 客户端只提供「复制机器码 / 输入激活码 / **申请换机**」，**不存在**废弃、解绑、重置试用任何入口；换机必须由厂商后台执行 |

### 研究来源（librarian, 2026-09）
- **Tauri 2.x 2.11.5**：Windows NSIS/MSI（MSI 仅 Windows 构建）、Linux AppImage/deb/rpm；交叉编译限制（cargo-xwin 实验性）；推荐 GitHub Actions 三平台矩阵原生构建；v1→v2 破坏性变更（package/tauri key 重命名、capabilities 权限系统、updater 插件化）。
- **机器指纹**：`machine-uid 0.6.0`（MachineGuid=/etc/machine-id）、`mid 5.0.1`（多源组合+哈希）、`tauri-plugin-machine-uid 0.1.3`；MachineGuid 是安装 ID 非硬件 ID（重装失效），建议多源组合 + HMAC。
- **Docker 容器化交付**：`docker buildx` + `--platform linux/amd64,linux/arm64` 多架构构建（QEMU/binfmt）；Rust 侧建议 **musl 静态链接**规避 glibc 版本绑定，基础镜像用 `debian-slim`/`distroless` 控制体积；`docker save`/`load` 产离线镜像 tar（工业现场常无外网）；镜像完整性用 **cosign 签名 + digest 固定**（Docker Content Trust 已不推荐）；容器内机器码锚点须来自宿主机——容器自身的 machine-id 由 runtime 生成、MAC 为 veth 随机，**重建即变化**。
- **驱动生态**：Neuron LGPL-3.0 插件模型（节点/组/标签/北向订阅路由，可参考架构）；PLC4X 0.13.1 Apache-2.0 但 Rust 绑定未生产可用；`tokio-modbus 0.17.0`（Modbus RTU/TCP/ASCII）；`async-opcua 0.19.0`（freeopcua，MPL-2.0，活跃）；S7/MC 需自研。
- **MQTT**：`rumqttc 0.25.1`（Apache-2.0，rustls）vs `paho-mqtt 0.14.0`；⚠️ rumqttc 生态分裂（rumqttc-next 0.33.3 活跃）；规则引擎参考 EMQX/NanoMQ SQL 语义。
- **SQLite**：`rusqlite 0.40.1`（MIT，单连接+bundled SQLite 3.53.2）；`sea-orm 2.0.0`；写密集：单写连接 + WAL + 批量事务（SQLite 单写者坑）；SQLCipher 是 SQLite 分支需 `libsqlcipher-sys`。

---

## Work Objectives

### 核心目标
构建一个高可靠、可商业化运营的工业边缘数据汇聚与统一分发网关，核心 Rust 库跨平台复用，南向 6 类协议全覆盖，北向 MQTT 统一分发，授权体系闭环（云端激活 + 消息级二次校验）。

**第一优先目标（P0）**：先把「安装程序防破解 + 激活 + 一机一码 + 换机重发」的**设计图**做出来并经用户确认——设计未定案前不写该域的实现代码。

### 交付物
- `docs/design/` **设计图与设计说明**（P0 先行）：`auth-architecture.svg`、`machine-binding.svg`、`activation-state-machine.svg`、`installer-hardening.svg`、`licensing-server-and-admin-console.svg`、`container-delivery.svg` + 配套 `.md` 说明
- **界面设计（已产出）**：`docs/design/ui-design-system.md`（设计系统 / token / 共享组件）、`ui-gateway-console.md`（客户端 IA + 页面清单 + 线框 + 流程）、`ui-admin-console.md`（后台 IA + 页面 + 危险操作约束）、`docs/design/prototype/gateway-console.html` 与 `prototype/admin-console.html`（可点击高保真原型）
- **客户端界面 v2 终稿（已产出，方案 A 已选定）**：`docs/design/prototype/gateway-v2a-glacier.html`（A/B 两版共用同一份引擎与页面内容，仅 CSS 与 `window.__SCHEME__` 不同，保证功能不漂移）。实现以 A 版为准；v2b 仅存档对比
- `ui-kit/` **两端共用前端基础包**（design token + 共享业务组件；`web-console` 与 `admin-console` 均依赖）
- `crates/daemon` 核心库（驱动 trait、Modbus、OPC UA、S7、MC、HTTP、第三方 MQTT、数据处理、调度器、缓存、存储、MQTT 客户端、授权、管理 API、进程装配）
- `crates/licensing-server` 云授权服务（Token 签发、心跳校验、试用期管理、授权白名单、**激活码生命周期 + 服务端二次校验**）
- `tauri-shell/` Windows 桌面安装包（含安装包签名与防篡改校验）
- `headless/` Linux 网关服务（原生 AppImage/deb/rpm + systemd，同时是容器镜像的运行体）
- `deploy/docker/` **Linux Docker 交付包**（多架构 Dockerfile/buildx、docker-compose、离线镜像 tar 打包、一键安装脚本、宿主机指纹注入、持久卷与串口/网络映射模板）
- `web-console/` 网关侧 Vue 管理界面（Arco Design Vue + `ui-kit/`，含首次初始化向导、点位表批量导入导出、授权与激活页）
- `admin-console/` **厂商总管理后台**（激活码发放/绑定/废弃/重发、设备与租户管理、回执与异常、换机工单、密钥轮换）
- `ci/` GitHub Actions CI 流水线（windows-latest / ubuntu-latest 双平台，含多架构镜像构建）
- `docs/` 部署手册（含 Docker 部署）、协议接入指南、API 文档、运维手册
- 完整测试套件

### Definition of Done
- [x] **Wave 0 设计图（6 份）+ 界面设计（客户端与管理后台）全部产出并经用户 explicit 确认**（界面设计稿：`docs/design/ui-*.md` 三份 + `prototype/` 两份可点击原型）—— **2026-09-23 用户定稿确认（「设计图定稿，更新相关文档，进入开发阶段」），开发阶段自本日起算**
- [x] **客户端界面 v2（方案 A）已确认**：`docs/design/prototype/gateway-v2a-glacier.html` 为界面实现基准；A/B 方案选择已定案（A 选定、B 存档）
- [ ] `cargo test --workspace` 全部 PASS
- [ ] `cargo tauri build`（Windows）/ AppImage+deb+rpm（Linux 原生）成功产出安装包
- [ ] **Linux Docker 交付**：多架构镜像产出（`linux/amd64` + `linux/arm64`）；`docker save` 产物可在**无外网**机器 `docker load` 后经一键脚本启动；管理界面可从宿主机浏览器访问；串口与局域网设备接入正常
- [ ] **镜像可信**：cosign 签名校验通过；`docker-compose.yml` 以 digest 固定镜像；篡改镜像后校验失败
- [ ] **容器化不破坏授权**：`docker rm && docker run` 重建容器后，机器码不变、授权状态不变、**试用期不被重置**
- [ ] 安装包签名校验通过；篡改安装包后校验失败（防破解）
- [ ] 单网关模拟 50 设备 × 100ms 采集，端到端数据流转正常；200 设备上限完成一次验收
- [ ] 离线 7 天后续传补发成功，且补发无重复入库
- [ ] 云授权服务签发 Token，客户端验签通过
- [ ] **二次校验按档位生效**：A 档（数据经厂商云）验签 + ±5min 窗口 + 全局 Nonce 防重放拒绝重放消息；**B 档（默认）网关侧签名 + 单调序号 + 本地去重生效，且审计回执（序号区间/条数/摘要哈希）在云端可校验、无业务数据外泄**；B 档断网时**继续采集与转发**（不强制在线）
- [ ] **北向双编码可用**：同一份数据经 `protobuf` 与 `json` 两路出口发出，字段语义一致、**验签结果相同**；超 2^53−1 整数以字符串编码且无精度丢失
- [ ] **防逆向 Tier-1 落地**：二进制已 strip 且关键常量混淆；段哈希自检可发现 patch 并进入受限模式；前端资源有完整性清单校验；**审计确认 WebView/JS 层无授权判定逻辑**
- [ ] **一机一码**：激活码绑定机器码成功；同一激活码在第二台设备激活被拒
- [ ] **换机重发**：总管理后台废弃原激活码后，原设备授权失效；重发新码在新机器激活成功
- [ ] **界面与设计稿一致**：客户端与管理后台的页面、页面清单、危险操作二次确认形态与 `ui-*.md` 一致；两端共用 `ui-kit/`，视觉无漂移；实时监控在 200 设备场景下渲染节流（1s）生效且 CPU 有基线记录
- [ ] **点位表批量导入导出**：CSV/XLSX 导入校验给出「行号 + 原因 + 允许值」；「导出 → 修改 → 导入」闭环可用（字段顺序一致）；**公式列可导入导出，且导入时做整表级环检测**
- [ ] **公式计算可用**：计算点按其依赖拓扑序求值（同一周期内物理点先算完）；`[A]+[B]` 类公式结果与手算一致；引用**已换算到工程单位**的值（不是原始寄存器值）
- [ ] **公式环检测生效**：构造 `R_A = [R_B]+1` / `R_B = [R_A]+1` → **保存被拒**且报错给出**环路径**；导入点表含跨行成环时同样被拒
- [ ] **公式失败语义正确**：除零 / 输入点位超时 / 结果 NaN → 输出质量标记为 `calc_failed`（或继承输入最差质量），且按配置策略（hold_last 默认）处理，**不产生静默错误值**
- [ ] **公式不破坏下游**：同一份计算点数据经 protobuf 与 json 两路出口发出，语义一致、验签结果相同；死区与单位换算顺序正确（先算后滤）
- [ ] **公式变更可追溯**：公式修改写入审计日志（含改前/改后）；**历史数据不因改公式而重算**（可断言改公式前后历史查询结果不变）
- [ ] **客户端无越权入口**：`/api/license/*` 无 revoke/unbind/reset-trial 端点；客户端界面无可点击的解绑 / 重置试用入口（真实浏览器断言：可点元素扫描无命中）
- [ ] **RBAC 可见性生效**：`viewer` 登录后写操作入口不可见（`RoleGate`），直接调用写接口返回 403；首次登录强制修改默认口令

### Must Have
- **设计图先行**：授权/防破解/激活/一机一码/激活码生命周期的设计图与状态机（Wave 0，最高优先级）
- **界面设计先行**：设计系统（token + `ui-kit/` 共享组件）+ 客户端与管理后台的信息架构、页面清单、线框、关键交互（Wave 0，与设计图同批确认）
- **点位表批量导入导出**（CSV/XLSX + 行号级校验 + 导出导入闭环 + 整表级公式环检测）
- **点位表公式计算（计算点 / 派生点）**：受限表达式引擎（AST 编译 + 白名单函数，禁用脚本与 eval）、依赖拓扑序求值、环检测给环路径、失败质量码 `calc_failed` 与 hold_last/null/skip 策略、公式变更审计、历史不重算；**前端编辑器只做即时语法提示，权威校验与试算走网关 API**
- **账号与角色管理页面**（用户增删改、角色权限矩阵、首次登录强制改密、登录失败锁定）
- **授权状态页与换机申请**（客户端唯一授权触点：机器码与锚点来源、试用倒计时、激活、申请换机；客户端无废弃/解绑能力）
- **诊断与备份恢复**（完整性自检、导出诊断包、配置备份/导入、回退上一份有效配置、告警中心）
- Modbus TCP/RTU 采集 + 北向 MQTT 转发链路跑通
- 断网续传（SQLite 持久化队列，环形覆盖 10GB/7 天）
- 授权体系（多源机器码 + 3 天试用 + Ed25519 Lease Token + 24h 心跳）
- **一机一码绑定**（激活码 ↔ 机器码一对一，云端持有绑定关系）
- **激活码生命周期**（发放 / 绑定 / 废弃 / 重发，总管理后台可运营）
- **安装程序防破解**（安装包签名 + 完整性校验 + 反调试/反篡改 + 试用标记冗余存储 + 时钟回拨检测）
- **Linux Docker 交付**（多架构镜像 + 离线 tar 包 + 一键安装脚本 + cosign 镜像签名 + digest 固定）
- **容器内授权锚定**（机器码取自宿主机锚点而非容器；试用标记/租约/授权状态落宿主机持久卷；容器重建不改变机器码、不重置试用）
- **消息级二次校验（分档）**：A 档 = AuthBlock 签名 + 云端验签 + ±5min 时间窗口 + 全局 Nonce 防重放；**B 档（默认）= 网关侧签名 + 单调序号 + 本地去重 + 云端审计回执校验**；C 档 = 仅租约心跳
- **北向双编码（每路出口可选）**：Protobuf（默认）与 JSON 双编码，大整数转字符串保精度，双编码验签语义一致
- **防逆向 Tier-1**：代码签名、二进制硬化（strip/LTO/panic=abort）、字符串与常量混淆、段哈希自检、低成本反调试、前端资源完整性清单
- 共享 Rust 核心 + 统一 Vue 前端
- TDD 全链路

### Must NOT Have
- 首期不做 PLC4X/第三方驱动 Sidecar（JVM 桥接）
- 首期不做 DuckDB 本地报表引擎
- 首期不做硬件加密狗
- 首期不做等保三级
- 首期不做多语言国际化
- ⚠️ 禁止把机器指纹锚点直接采自**容器内部**（容器 `/etc/machine-id`、veth MAC、容器主机名、容器 ID 均随重建变化；必须取宿主机锚点）
- ⚠️ 禁止把试用标记、租约、授权状态只写在**容器可写层**（必须落宿主机持久卷）——否则 `docker rm && docker run` 即重置试用
- ⚠️ 禁止用 `--privileged` 一把梭开权限（串口按需 `--device` + `--group-add dialout`，能力按需授予）
- ⚠️ 禁止交付未签名镜像 / 使用可变 tag（`:latest`）作为生产引用（须 cosign 签名 + digest 固定）
- ⚠️ 禁止把宿主指纹文件、签名私钥、激活码明文打进镜像层（镜像可被 `docker save` 完整导出，等于明文分发）
- ⚠️ 禁止把 **B 档做成「必须在线才能转发」**（断网必须继续采集与转发，审计回执允许延迟补报）
- ⚠️ 禁止把超出 2^53−1 的整数用 JSON number 编码（必须转字符串；设备纳秒时间戳、大 uint64 计数器会静默丢精度）
- ⚠️ 禁止让 JSON 与 Protobuf 两条编码路径产生**不同语义或不同验签结果**
- ⚠️ 禁止在 WebView/JS 层实现任何授权判定或防篡改判定（前端资源等同公开，只能做展示与完整性清单校验）
- ⚠️ 禁止使用内核驱动、VMProtect/Themida 级商业壳、驱动级 hook 做反调试（Tier-3，与 Tauri/杀软冲突且维护成本失控）
- ⚠️ 禁止把机器码明文嵌入数据值做隐写水印（应走显式签名字段）
- ⚠️ 禁止直接签 Protobuf 序列化字节（应签业务语义确定性哈希）
- ⚠️ 禁止在安装包/二进制中硬编码任何激活码、私钥或厂商签名私钥
- ⚠️ 禁止把「废弃激活码」的能力放在客户端——废弃/重发只能由总管理后台经云端授权服务执行
- ⚠️ 禁止在客户端界面提供「解绑 / 解除绑定 / 重置试用 / 更换机器码」任何可点击入口（`/api/license/*` 不得出现 revoke / unbind / reset-trial 端点）
- ⚠️ 禁止 `web-console` 与 `admin-console` 使用不同组件库或各自维护一套设计 token（必须共用 `ui-kit/`，否则视觉必然漂移）
- ⚠️ 禁止点表导入时静默丢弃错误行（必须逐行给出「行号 + 原因 + 允许值」，并提供「仅导入有效行」的显式选择）
- ⚠️ 禁止用 `eval` / 任意脚本（Lua、JS、SQL 片段）实现公式计算——必须是**受控表达式引擎**（白名单函数 + 白名单运算符 + 无赋值无循环无 I/O）；工业现场公式可被客户自行填写，等价于「远程代码执行入口」
- ⚠️ 禁止让公式求值**无界运行**（必须有表达式长度上限、AST 节点数上限、单次求值超时，防 `pow(10,10^9)` 类 DoS）与**无深度限制的嵌套**（防栈溢出）
- ⚠️ 禁止**跳过环检测**保存公式，也禁止环检测只报「存在循环」而不给**环路径**；禁止只做逐行校验（跨行成环必须整表检测）
- ⚠️ 禁止让**前端本地求值**成为权威结果（前端只做即时语法提示；保存前必须过网关侧校验，试算必须走 `POST /api/points/formula/dry-run`，避免两端语义分歧）
- ⚠️ 禁止公式引用**未经单位换算的原始寄存器值**（必须先换算到工程单位，否则 `A+B` 单位混算）
- ⚠️ 禁止在公式变更后**重算历史数据**（首期只对新增数据生效；如需重算属 P2 功能，需显式设计而非默认行为）
- ⚠️ 禁止在公式失败时**静默输出错误数值**（除零 / NaN / 输入超时必须落到 `calc_failed` 或继承最差质量码）
- ⚠️ 禁止把公式计算塞进转发规则引擎（任务 20/37）——公式属点位表层职责，规则引擎只决定「转发什么、去哪」
- ⚠️ 禁止把实时数据无节流直接写入 DOM（200 设备 × 100ms 场景下必须按 1s 节流渲染）
- ⚠️ 禁止用明文或可逆弱混淆存储试用标记、激活码与机器码（须加密 + 多位置冗余）
- ⚠️ 禁止实现本地离线激活（用户已确认激活必须联网）

---

## Verification Strategy

### 测试决策
- **基础设施存在**：NO（项目从零开始，需先搭建）
- **自动化测试**：YES（TDD）
- **框架**：cargo test（Rust）/ vitest（前端）/ protobuf schema 校验
- **TDD 流程**：RED（先写失败测试）→ GREEN（最小实现）→ REFACTOR

### QA 策略
每任务必须含 agent-executed QA Scenarios。
- **设计图（Wave 0）**：产出 SVG + 说明 md，逐张核对「参与方 / 信任边界 / 状态迁移 / 失败路径 / 与 Must NOT 的一致性」，并等用户 explicit 确认；设计图未确认不得开工实现
- **Rust 核心**：`cargo test` + 集成测试（模拟设备数据流转）
- **前端**：`vitest` + `playwright`（管理界面交互、总管理后台激活码操作）
- **协议**：模拟 Modbus TCP/RTU 设备 + 校验采集精度（字节序、地址解析、数据类型解码）
- **授权**：模拟过期 Token / 篡改签名 / 超期心跳，验证拒收；服务端二次校验拒绝时间窗口外与重放 Nonce
- **激活与一机一码**：同一激活码在第二台（不同机器码）激活被拒；废弃后原设备授权失效；重发新码在新机激活通过
- **防破解**：篡改安装包 → 签名/完整性校验失败；回拨系统时间 → 试用期不延长；删除本地标记 → 云端首次激活时间兜底
- **缓存**：断网模拟 → 写入 → 恢复 → 校验补发完整性与**幂等（无重复入库）**

### 证据保存
`.omo/evidence/task-{N}-{scenario-slug}.{ext}`。

---

## Execution Strategy

### 并行执行 Waves

```
Wave 0（★ 最高优先级 — 设计图先行，只出设计不写实现代码）:
├── 40. 授权与防破解总体设计图（架构/信任边界/攻击面/防御矩阵） [deep]
├── 41. 一机一码指纹与绑定设计图（多源锚点/稳定性/容错/密钥托管） [deep]
├── 42. 激活流程与激活码生命周期状态机设计图（激活/宽限/心跳/降级/废弃/重发） [deep]
├── 43. 安装程序防破解设计图（安装包签名/完整性/反调试/试用标记冗余） [deep]
├── 44. 云端授权服务与总管理后台设计图（数据模型/API 契约/后台交互/密钥轮换） [deep]
├── 59. Linux 容器化交付设计图（宿主机指纹锚点/持久卷布局/镜像签名/离线分发/串口与网络接入） [deep]
├── 63. 客户端界面设计（设计系统/客户端信息架构/页面清单/线框/交互/授权触点） [deep]
└── 64. 总管理后台界面设计（后台信息架构/激活码与设备页/危险操作约束/回执异常页） [deep]
→ 产出 6 张设计图 + 2 份界面设计（含可点击原型）→ 用户 explicit 确认 → 才允许进入 Wave 1

Wave 1（基础设施 + 核心骨架）:
├── 1. Monorepo 初始化 + Cargo workspace + CI 骨架 [quick]
├── 2. Protobuf 数据模型 schema（TelemetryBatch/DataPoint/AuthBlock） [quick]
├── 3. 机器码指纹模块（mid 5.0，多源 + HMAC） [quick]
├── 4. 配置管理框架（TOML/YAML 序列化 + 热重载） [quick]
├── 5. 日志与可观测性（tracing + 分级日志 + 结构化） [quick]
├── 6. 核心错误类型 + 统一 Result + 错误码 [quick]
└── 7. 测试基础设施（cargo test + vitest + protobuf 校验工具） [quick]

Wave 2（核心驱动 — 7 任务）:
├── 8. Driver trait + 地址解析器 + 断线重连基类 [deep]
├── 9. Modbus TCP/RTU 驱动（tokio-modbus 0.17） [deep]
├── 10. OPC UA 驱动（async-opcua 0.19） [deep]
├── 11. 西门子 S7 驱动（自研 S7comm） [deep]
├── 12. 三菱 MC 驱动（自研 3E/4E 帧） [deep]
├── 13. HTTP 采集驱动 [quick]
├── 14. 第三方 MQTT 接入驱动 [unspecified-high]

Wave 2b（数据处理 + 缓存 — 4 任务）:
├── 15. 数据处理层（点位映射、单位换算、死区过滤、时间戳统一） [deep]
├── 16. 组轮询调度器（Neuron group_timer 模式） [deep]
├── 17. 断网续传 — SQLite 持久化队列（独立库文件 + rusqlite 单写 + WAL + 环形覆盖） [deep]
└── 18. 本地存储（独立库文件 + SQLite WAL + SQLCipher 透明加密） [deep]

Wave 3（北向 + 授权，9 任务）:
├── 19. MQTT 客户端（rumqttc-next 0.33，rustls，QoS 0/1/2） [deep]
├── 20. MQTT 转发规则引擎（声明式 JSON 规则，动作集 P0 子集） [deep]
├── 21. AuthBlock 签名模块（Ed25519，业务语义哈希） [deep]
├── 22. 云授权客户端（Lease Token 获取 + 24h 心跳） [deep]
├── 23. 试用期管理（本地加密标记 + 云端首次激活时间） [deep]
├── 24. 免费基础版限制逻辑（8 设备/≥1s/无北向转发/无 OTA） [quick]
├── 25. TLS/mTLS 配置 + MQTT 连接安全 [quick]
├── 26. 安全审计模块（登录/配置修改/授权失败日志） [quick]
└── 62. 北向双编码器（每路出口 protobuf/json + 大整数精度 + MQTT5 编码声明） [deep]

Wave 4（管理界面 + 安全 + 平台壳，11 任务）:
├── 27. Vue 界面骨架（Vite + TS + **Arco Design Vue** + `ui-kit/` 共享包引导） [quick]
├── 28. 配置管理页面（设备/点位/协议/规则 + 空态与错误态） [unspecified-high]
├── 29. 实时监控面板（数值 + 连接状态 + 采集频率 + 质量码；1s 节流） [visual-engineering]
├── 30. 日志与审计页面 [unspecified-high]
├── 31. Web 鉴权（JWT + HTTPS 自签名证书 + 路由守卫；首次强制改密） [quick]
├── 32. Tauri shell（Windows 桌面，含 WebView 管理界面） [deep]
├── 33. Linux headless 服务（HTTP server 托管 Vue；容器友好的路径与信号处理） [deep]
├── 34. 敏感配置加密（SQLCipher + HKDF 机器码派生密钥） [deep]
├── 65. 点位表批量导入/导出与校验（CSV/XLSX + 行号级错误 + 导出导入闭环） [deep]
├── 67. 客户端授权状态页与换机申请（机器码/锚点/试用倒计时/激活/申请换机） [unspecified-high]
└── 68. 诊断自检 / 备份恢复 / 告警中心页面 [unspecified-high]

Wave 5（OTA + 高级 + 打包 + 容器化，6 任务）:
├── 35. OTA 升级模块（固件/配置热更新） [deep]
├── 36. 远程运维（远程重启/启停采集/日志拉取） [deep]
├── 37. 规则引擎 V1（补全动作集：规则版本管理 + 组合与循环检测） [deep]
├── 38. 打包与 CI（NSIS/MSI Windows + AppImage/deb/rpm Linux 原生） [quick]
├── 60. Docker 镜像构建与离线分发（多架构/宿主机指纹注入/持久卷/串口与网络映射/cosign 签名） [deep]
└── 39. 联调测试 + 压测（50→200 设备 × 100ms） [deep]

Wave 6（★ 授权/防破解/激活 实现 — 最高优先级实现波，6 任务）:
├── 45. licensing-server 云授权服务主体（激活/心跳/白名单/试用记录/Ed25519 签发 + kid 轮换） [deep]
├── 46. 激活码生命周期与一机一码绑定（发放/绑定/废弃/重发） [deep]
├── 47. 总管理后台 admin-console（激活码/设备/租户管理页面） [unspecified-high]
├── 48. 二次校验三档实现（A 云端验签 + B 网关侧校验与审计回执 + C 纯本地） [deep]
├── 49. 客户端密钥托管（Ed25519 生成 + DPAPI/keyring 加密持久化 + 重装恢复） [deep]
└── 50. 安装包签名与防篡改加固（代码签名/完整性自检/反调试/时钟回拨检测） [deep]

Wave 7（工程补齐 — 硬缺项 + 软缺项，10 任务）:
├── 51. daemon 进程装配与生命周期（bootstrap/优雅停机 flush/看门狗） [deep]
├── 52. 后端管理 API 层 + 实时数据通道（REST + WS/SSE + 状态聚合） [deep]
├── 53. 数据类型解码器 + 质量码规范（IEEE754/字节序组合/位与字符串/quality） [deep]
├── 54. 补发幂等去重 + 背压与内存队列水位 [deep]
├── 55. SQLite schema 迁移框架 + 配置版本迁移与回滚（安全模式启动） [deep]
├── 56. 可信时间（NTP 校时 + 单调时钟 + 时钟回拨检测） [quick]
├── 57. 平台差异补齐 + RBAC（Windows 服务化/开机自启/串口权限 + 角色权限） [unspecified-high]
├── 58. 工程合规与文档（cargo-deny 许可证门禁 + 部署/协议/API/运维手册 + 北向双编码 + 公式语法文档） [unspecified-high]
├── 61. 容器化交付验收（重建不重置试用 / 宿主机指纹一致 / 串口与网络直通 / 镜像签名） [deep]
└── 66. 账号与角色管理页面（用户增删改 + 角色权限矩阵 + 首次强制改密 + 登录锁定） [unspecified-high]

Wave 7b（点位表公式计算 + 界面终验 — 3 任务，★ 执行顺序 **70 → 71 → 69**）:
├── 70. 公式引擎与计算点（受限表达式 AST + 白名单函数 + 依赖 DAG 拓扑序 + 环检测 + 失败质量码 + dry-run API） [deep]
├── 71. 点位表公式编辑器 UI（类型列 / 表达式输入 / 依赖与环提示 / 后端试算 / 失败策略） [unspecified-high]
└── 69. 界面实现与设计稿一致性验收（两端逐项比对 + 门控验证 + 真实浏览器验证；**须在 70/71 完成后**） [unspecified-high]

Wave FINAL（4 并发审查 + 用户确认）:
├── F1. 计划合规审计 — oracle
├── F2. 代码质量审查 — unspecified-high
├── F3. 真实手动 QA — unspecified-high
└── F4. 范围保真检查 — deep
→ 呈现结果 → 等用户 explicit "okay"

Critical Path: 40-44 + 59 + 63/64（设计图与界面设计 + 用户确认）→ 1 → 2 → 8 → 9/10/11/12 → 15 → 19 → 21 → 45/46/48 → 27/28 → 32/33 → 38/60 → 61 → 65/67/69 → F1-F4 → user okay
Parallel Speedup: ~65% faster than sequential
Max Concurrent: 8 (Wave 7；Wave 6 可与 Wave 3/4 并行；**Wave 7b 于任务 53 完成后启动**，可与 Wave 6 尾段并行)
```

### 依赖矩阵

> 格式：`任务: 被阻塞于 — 阻塞`
> **Wave 0（40-44、59、63、64）不依赖任何实现任务，是全项目起点。** 所有实现任务（1-39、45-71）额外被「Wave 0 设计图与界面设计经用户确认」这一里程碑阻塞——设计未确认前不得开工实现。

- **40**: — — 41, 42, 43, 44, 59（同波并行，末尾统一提交用户确认）
- **41**: — — 42, 44, 3, 49
- **42**: — — 22, 23, 24, 45, 46
- **43**: — — 49, 50
- **44**: — — 45, 46, 47, 48
- **1-7**: Wave 0 设计确认 — 8-17, 18, 45-62
- **8**: 3 — 9/10/11/12/13/14, 15, 16
- **9**: 8 — 17, 39
- **10**: 8 — 39
- **11**: 8 — 39
- **12**: 8 — 39
- **13**: 8 — 39
- **14**: 8 — 39
- **15**: 8, 9-14 — 19, 20, 37, 53, 70
- **16**: 8, 15 — 39
- **17**: 8, 9-14 — 39, 54
- **18**: 3, 17 — 34, 39, 55
- **19**: 15, 17 — 20, 39
- **20**: 15, 19 — 37, 39, 52
- **21**: 2, 3 — 22, 39, 49
- **22**: 3, 21, 45 — 23, 39
- **23**: 3, 22, 45 — 39
- **24**: 22, 23 — 39
- **25**: 19 — 39
- **26**: 3 — 39, 52
- **27**: 1 — 28/29/30/31
- **28**: 27, 8/15/20, 52 — 39, 71
- **29**: 27, 52 — 39
- **30**: 27, 26, 52 — 39
- **31**: 27 — 32/33, 47
- **32**: 27, 31, 51 — 38
- **33**: 27, 31, 51 — 38, 60
- **34**: 18, 3, 49 — 39
- **35**: 1, 19 — 39
- **36**: 35, 27, 52 — 39
- **37**: 20, 15 — 39
- **38**: 32/33, 35, 50 — F1-F4
- **39**: 19, 17, 15, 8, 48, 51-58, 62, 70 — F1-F4
- **45**: 40-44, 2, 3 — 22, 23, 46, 47, 48
- **46**: 44, 45 — 47, 39
- **47**: 31, 44, 46 — 39
- **48**: 2, 21, 44, 45 — 39
- **49**: 21, 43 — 22, 34, 50
- **50**: 38, 43, 49 — 39, F1-F4
- **51**: 1, 8 — 32/33, 38, 39（⚑ 装配前置，须在 Wave 4 前完成）
- **52**: 1, 20, 26 — 28/29/30/36, 39
- **53**: 8, 15 — 39, 70
- **54**: 17, 19 — 39
- **55**: 4, 18 — 39
- **56**: 6 — 23, 48, 50
- **57**: 1, 31 — 32/33, 39, 61
- **58**: 4, 5, 38, 60, 62 — 39, F1-F4
- **59**: — — 60, 61（同属 Wave 0，与 40-44 一并提交用户确认）
- **60**: 33, 51 — 61, F1-F4
- **61**: 33, 57, 60 — 39, F1-F4
- **63**: — — 27, 29, 65, 67, 68, 69, 71（同属 Wave 0，与 40-44、59 一并提交用户确认）
- **64**: — — 47, 66, 69（同属 Wave 0）
- **65**: 15, 28, 53, 70 — 39, 69
- **66**: 31, 57 — 69
- **67**: 22, 23, 27, 31, 49 — 39, 69
- **68**: 52, 55, 30 — 39, 69
- **69**: 63, 64, 27-34, 47, 65-68, 70, 71 — 39, F1-F4
- **62**: 2, 15, 21 — 39, 58
- **70**: 2, 4, 15, 53 — 39, 65, 69, 71
- **71**: 27, 28, 52, 63, 70 — 69

### Agent Dispatch Summary
- **Wave 0**: 8 任务 — 全部 `deep`（★ 6 张设计图 + 2 份界面设计，最高优先级，末尾需用户确认）
- **Wave 1**: 7 任务 — 全部 `quick`
- **Wave 2**: 7 任务 — 5 `deep`, 1 `quick`, 1 `unspecified-high`
- **Wave 2b**: 4 任务 — 4 `deep`
- **Wave 3**: 9 任务 — 6 `deep`, 3 `quick`
- **Wave 4**: 11 任务 — 4 `deep`, 5 `unspecified-high`, 1 `quick`, 1 `visual-engineering`（新增 65/67/68 界面与点位批量）
- **Wave 5**: 6 任务 — 5 `deep`, 1 `quick`（含 60 容器化）
- **Wave 6**: 6 任务 — 5 `deep`, 1 `unspecified-high`（★ 授权/防破解/激活实现）
- **Wave 7**: 10 任务 — 6 `deep`, 1 `quick`, 3 `unspecified-high`
- **Wave 7b**: 3 任务 — 1 `deep`（70 公式引擎与计算点）, 2 `unspecified-high`（71 公式编辑器、69 界面终验）；**执行顺序 70 → 71 → 69**
- **FINAL**: 4 任务 — F1 `oracle`, F2 `unspecified-high`, F3 `unspecified-high`, F4 `deep`
- **合计**: 71 任务 + 4 终审 = 75

---

## TODOs

> ⚠️ **执行顺序 ≠ 编号顺序**：任务 **40-44、59、63、64（Wave 0）编号最大，但必须最先执行**。
> Wave 0 只产出设计图、界面设计与可点击原型，**不写任何实现代码**；6 份设计图 + 2 份界面设计经用户 explicit 确认后，才允许从任务 1 开始实现。
> ✅ **界面设计（63/64）产出物已就绪**：`docs/design/ui-design-system.md`、`ui-gateway-console.md`、`ui-admin-console.md` + `prototype/gateway-console.html`、`prototype/admin-console.html`（已通过真实浏览器 31/31 项验证）。本轮待确认即为**冻结设计**。

### Wave 0 — 设计图先行（★ 最高优先级，唯一允许先于设计确认的动作）

- [x] 40. 授权与防破解总体设计图（架构 / 信任边界 / 攻击面 / 防御矩阵）

  **What to do**:
  - 产出 `docs/design/auth-architecture.svg`：授权体系总体架构图，必须画清四类参与方（**设备/网关进程**、**本地授权模块**、**云端授权服务 licensing-server**、**厂商总管理后台 admin-console**）及其信任边界（虚线标注「本地可信区 / 客户可篡改区 / 厂商可控区」）
  - 同图画出数据流与信任流：机器码指纹 → 激活请求 → 激活码绑定 → Lease Token 签发 → 本地验签 → 24h 心跳 → 宽限 → 降级 → 停止北向转发
  - 图中须体现 **Linux 的两种部署形态（原生 systemd 服务 与 Docker 容器）**，并标出容器形态下三个新增信任关注点：**宿主锚点只读挂载**、**持久卷承载授权/试用状态**、**镜像供应链（签名与 digest）**（细节见任务 59）
  - 产出 `docs/design/threat-model.md`：攻击面清单与防御矩阵表（攻击者能力 × 攻击手段 × 防御措施 × 残余风险），至少覆盖：伪造机器码、拷贝授权文件到另一台机器、篡改本地时钟、重放旧 Token/AuthBlock、patch 二进制绕过授权判定、劫持前端 WebView 调用、中间人改激活响应、重装系统重用试用期
  - 明确列出「**防御不住什么**」（残余风险自曝，避免客户预期错位），必须包含：**B 档下破解者可篡改代码不上报回执**（兜底只能靠「心跳缺序号区间即告警降级」，但**无法阻止破解后完全断网静默自用**）；**Tier-2/Tier-3 反破解手段明确不做**；**客户端前端资源不可能保密**（可解包读改）
  - **按二次校验档位分别画信任边界**：A 档（数据经厂商云，校验点最多）、**B 档（默认，数据不经厂商，只剩心跳 + 审计回执两条控制通道）**、C 档（仅心跳）；三档分别标注「厂商能验证什么 / 验证不了什么」

  **Must NOT do**:
  - 本任务只产出设计图与设计说明，**不得写任何实现代码**（不建 crate、不引依赖、不改 Cargo.toml）
  - 不得画出无法落地的防御（如内核驱动、硬件加密狗——首期 Must NOT）

  **Recommended Agent Profile**: Category: `deep` — 授权体系架构设计
  **Parallelization**: Wave 0（与 41-44, 59 并行）；**Blocked By**: 无；**Blocks**: 全部实现任务（1-39、45-62）
  **References**: 第三/四轮访谈决议（授权 + 消息级二次校验 + 安全基线）；librarian 研究（machine-uid / mid）；Ed25519 与 HKDF 既有决议
  **Acceptance Criteria**:
  - [ ] `docs/design/auth-architecture.svg` 存在，含 4 类参与方 + 3 个信任区 + 完整信任流
  - [ ] `docs/design/threat-model.md` 存在，攻击面表 ≥8 行且每行有对应防御措施与残余风险
  - [ ] 与 Must NOT Have 无冲突（无硬加密狗、无隐写水印、非直接签序列化字节）
  - [ ] A/B/C 三档信任边界分别画出，且每档写明「厂商能验证什么 / 验证不了什么」
  - [ ] 「防御不住什么」含 B 档不回执与断网静默自用的残余风险
  **QA Scenarios**:
    - Happy: 打开 SVG → 逐项核对 4 参与方 / 3 信任区 / 信任流闭环 → 断言无缺口
    - Error: 攻击面表中任一行缺「防御措施」或「残余风险」→ 断言设计不合格，退回补全
  **Evidence**: `.omo/evidence/task-40-auth-architecture.svg`, `.omo/evidence/task-40-threat-model.md`
  **Commit**: YES — `docs(design): auth & anti-crack architecture blueprint`

- [x] 41. 一机一码：机器码指纹与绑定设计图

  **What to do**:
  - 产出 `docs/design/machine-binding.svg`：一机一码绑定关系图，明确「**一个激活码 ↔ 一台设备机器码**」的实体关系（activation_code 1:1 device，device 1:N lease）
  - 产出 `docs/design/machine-fingerprint.md`，必须包含：
    - **多源锚点清单**：主板/BIOS UUID、磁盘序列号、网卡 MAC、系统 machine-id/MachineGuid、CPU 特征，逐项标注「取用方式 / 是否易变 / 稳定性权重 / 平台差异（Windows vs Linux）」
    - **稳定性与容错策略**：明确哪些锚点会因合法运维变化（换网卡、加硬盘、虚拟化迁移），设计 **N-of-M 匹配 + 允许 1 项漂移** 的判定规则与阈值取值
    - **指纹算法**：归一化 → 拼接 → HMAC-SHA256(盐为厂商私密常量) → 截断；说明为何不用裸哈希（可逆枚举）
    - **本地存储与保护**：指纹与激活凭证的落盘位置、DPAPI（Windows）/ keyring（Linux）加密、多位置冗余
    - **重装/恢复路径**：系统重装后指纹是否可复现；不可复现时的重新激活流程
    - **容器化形态的锚点来源（Linux Docker）**：锚点必须从**宿主机**采集（只读挂载宿主 machine-id / DMI、安装脚本采集的宿主 MAC 签名指纹文件），并明确 **容器重建/升级不触发「换机」判定**（否则每次 `docker compose up --force-recreate` 都要重发激活码）；口径须与任务 59 完全一致
  - 明确「**一机一码冲突检测**」：同一激活码在不同机器码上激活时，服务端如何判定与拒绝（为任务 46 提供设计依据）

  **Must NOT do**: 不得写实现；不得设计可被简单枚举/暴力复现的裸哈希指纹；不得依赖单一易变锚点；不得把容器内部标识（容器 machine-id / veth MAC / 容器主机名）列为锚点
  **Recommended Agent Profile**: Category: `deep` — 指纹与绑定设计
  **Parallelization**: Wave 0（与 40, 42-44, 59 并行）；**Blocked By**: 无；**Blocks**: 3, 42, 44, 49, 59
  **References**: librarian 研究（`machine-uid 0.6.0`、`mid 5.0.1`：MachineGuid 是安装 ID 非硬件 ID，重装失效）；Windows DPAPI；Linux keyring
  **Acceptance Criteria**:
  - [ ] 锚点清单 ≥5 项，每项含稳定性权重与平台差异
  - [ ] 容错规则明确到「几个锚点命中即视为同机」且给出理由
  - [ ] 含冲突检测判定逻辑（为激活码 1:1 绑定服务）
  - [ ] 容器化锚点来源已说明（宿主锚点 + 重建不触发换机），与任务 59 口径一致
  **QA Scenarios**:
    - Happy: 按设计文档逐项核对锚点清单与容错阈值 → 断言规则自洽、无单一故障点
    - Error: 模拟「换一块网卡」→ 依设计规则推演 → 断言仍判为同机（不误伤合法运维）
    - Error: 模拟「整机更换」→ 推演 → 断言判为异机、要求重新激活
    - Error: 同一宿主上重建 / 升级容器 → 推演 → 断言仍判为同机（不需重新发激活码）
  **Evidence**: `.omo/evidence/task-41-machine-binding.svg`, `.omo/evidence/task-41-machine-fingerprint.md`
  **Commit**: YES — `docs(design): machine fingerprint & one-code-one-machine binding`

- [x] 42. 激活流程与激活码生命周期状态机设计图（含换机废弃/重发）

  **What to do**:
  - 产出 `docs/design/activation-state-machine.svg`：两张状态机图
    - **设备授权状态机**：`未激活 → 激活中 → 已激活(Active) → 宽限期(离线≤7天) → 降级/受限(Degraded) → 停止北向转发`，并标注每个迁移的**触发事件、守卫条件、可观测副作用**（保留本地采集 or 停发）
    - **激活码状态机**：`已发放(Issued) → 已绑定(Bound) → 已废弃(Revoked) → 已重发(Reissued，生成新码)`，标注迁移触发者（谁有权操作）
  - **换机迁移主流程（本任务重点）**：画出时序图 `docs/design/activation-reissue.svg`：客户换机 → 厂商在**总管理后台**查到此前的激活码 → 执行「废弃原激活码」→ 系统立即令原设备租约失效（下次心跳拒绝 + 原设备进入降级/停发）→ 后台「重新发放」新激活码（可指定新机器码或留待首次激活时绑定）→ 新机器激活成功 → 审计留痕
  - 产出 `docs/design/activation-rules.md`：
    - 激活请求/响应契约（字段级：机器码指纹、激活码、设备公钥、tier、有效期、nonce、签名）
    - 宽限期与心跳规则（7 天 / 每 24h；超期行为=停北向转发、保留本地采集）
    - 试用期 3 天与降级免费版的衔接（试用 → 降级，而非停用）
    - **废弃语义**：废弃是「立即失效」还是「到期失效」——必须给出明确决议与理由
    - 换机重发的约束：新码是否复用原 tier/有效期、是否允许一码多机（**默认禁止**）
  - 所有状态迁移必须穷举失败路径（激活失败、心跳失败、时钟异常、服务器不可达）

  **Must NOT do**:
  - 不得写实现代码
  - 不得把「废弃激活码」的入口放到客户端——废弃/重发只能由总管理后台经云端执行（对齐 Must NOT Have）
  - 不得设计离线激活（用户已确认激活必须联网）

  **Recommended Agent Profile**: Category: `deep` — 状态机与业务流程设计
  **Parallelization**: Wave 0（与 40, 41, 43, 44 并行）；**Blocked By**: 无；**Blocks**: 22, 23, 24, 45, 46
  **References**: 第二轮访谈（激活必须联网 / 离线宽限 7 天 / 24h 心跳 / 试用 3 天降级）；第四轮（免费版限制）；本轮新增决议（换机废弃重发）
  **Acceptance Criteria**:
  - [ ] 设备授权状态机：状态 ≥5、迁移 ≥8，且「降级后行为」逐条写明
  - [ ] 激活码状态机：含 `Revoked` 与 `Reissued` 两个状态及迁移触发者
  - [ ] 换机时序图完整：废弃 → 原设备失效 → 重发 → 新机激活 → 审计，五步不缺
  - [ ] 失败路径覆盖 ≥4 条（服务器不可达 / 激活码已废 / 机器码不匹配 / 时钟异常）
  **QA Scenarios**:
    - Happy: 按状态机走一遍「激活 → 心跳正常 → 换机废弃 → 重发 → 新机激活」→ 断言状态迁移与副作用一致
    - Error: 用同一激活码在第二台设备激活 → 断言设计上被拒（1:1 绑定生效）
    - Error: 服务器不可达且已超 7 天 → 断言进入降级并停北向转发、保留本地采集
    - Error: 系统时间回拨 → 断言不延长宽限期/试用期
  **Evidence**: `.omo/evidence/task-42-activation-state-machine.svg`, `.omo/evidence/task-42-activation-reissue.svg`, `.omo/evidence/task-42-activation-rules.md`
  **Commit**: YES — `docs(design): activation flow & activation-code lifecycle state machine`

- [x] 43. 安装程序防破解设计图（安装包签名 / 完整性 / 反篡改 / 试用标记）

  **What to do**:
  - 产出 `docs/design/installer-hardening.svg`：安装与启动阶段的安全链路图（**下载/交付 → 安装包签名校验 → 安装 → 首次启动 → 运行时完整性自检 → 授权判定**），逐环节标注校验点与失败后的行为
  - 产出 `docs/design/installer-hardening.md`，必须包含：
    - **安装包签名**：Windows 代码签名证书（OV/EV）与时间戳、MSI/NSIS 数字签名校验、Linux 侧 AppImage/deb/rpm 的包签名与仓库校验
    - **内容完整性**：安装包内 payload 哈希清单（manifest），安装时校验；与任务 50 的实现边界对齐
    - **运行时自检**：二进制完整性校验（段哈希/签名自检）、关键资源的哈希比对、检测到篡改时进入受限模式（停北向转发 + 审计告警）
    - **反调试 / 反篡改**：明确做到哪一层（拒绝内核驱动与 rootkit 级手段），以及这些手段**只做提高成本、不做绝对防破解**的定位声明
    - **授权判定位置原则**：授权判定必须在 **Rust 侧**，WebView/JS 层不得作为授权判定依据（前端只做展示）；写清前后端判定边界
    - **试用标记多重冗余**：文件 + 注册表/配置目录 + SQLCipher 库 + 云端首次激活时间，四者交叉校验；单点被删如何兜底
    - **时钟回拨检测**：单调时钟 + 记录上次最大时间 + 与服务端时间比对，回拨即判异常
    - **Linux 容器化交付场景**：镜像同样是「分发物」，须纳入同一防篡改体系——镜像 cosign 签名 + digest 固定 + 离线 tar 包哈希清单（详见任务 59）；并写清**镜像可写层不得承载试用标记/租约/授权状态**，必须落宿主持久卷，使 `docker rm && docker run` 无法重置试用
  - **防破解力度已定案（本轮）**：**Tier-1 必做**（代码签名 + 二进制硬化 strip/LTO/panic=abort + 字符串与常量混淆 + 段哈希自检 + **低成本**反调试）；**Tier-2 暂不做**（控制流平坦化、字符串加密、模块交叉校验、Frida 检测）；**Tier-3 明确不做**（内核驱动、VMProtect/Themida 级商业壳、硬件加密狗）。**预算 控制面 : 客户端 = 7 : 3**；量化目标 = **拦掉 90% 非专业破解尝试，专业破解者付出 > 数人日仍无法获得云端有效授权**。文档须写明定位声明：**提高成本 ≠ 绝对防破解**
  - **前端不可保密原则**：Tauri 打包的 WebView 前端资源（JS/CSS）**等同于公开源码**，可解包读改。设计须明确：① 授权与防篡改判定**绝不在 JS 层**；② 前端资源只做**完整性清单哈希校验**（由 Rust 侧启动时校验）；③ 机密（密钥、激活码、签名算法细节）一律不放前端
  - **区分两个威胁**：**防逆向 ≠ 防「拷给第二台机器用」**——后者属机器码绑定 + 一机一码的职责，不得混入二进制加固范畴

  **Must NOT do**:
  - 不得写实现代码
  - 不得设计需要内核驱动 / 硬件加密狗 / 驱动级 hook 的方案（首期 Must NOT Have）
  - 不得把试用标记做单点存储（违反 Must NOT Have：禁止明文或可逆弱混淆存储）

  **Recommended Agent Profile**: Category: `deep` — 安装与防篡改设计
  **Parallelization**: Wave 0（与 40-42, 44 并行）；**Blocked By**: 无；**Blocks**: 49, 50
  **References**: librarian（Tauri 2.x 签名/updater 插件化、NSIS/MSI 构建限制）；第四轮（防破解：Rust 侧核心逻辑、试用期加密存储 + 多重标记、云端二次验证）
  **Acceptance Criteria**:
  - [ ] 安全链路图含 ≥5 个校验点，每点写明「失败后行为」
  - [ ] 试用标记冗余方案 ≥3 处，并给出单点删除的兜底推演
  - [ ] 明确写出防破解的「不做清单」与定位声明（提高成本 ≠ 绝对防止）
  - [ ] 授权判定位置原则明确（Rust 侧判定，前端仅展示）
  - [ ] 容器化场景已覆盖：镜像签名/完整性 + 试用标记不落镜像可写层（与任务 59 口径一致，无冲突）
  - [ ] Tier-1 清单可执行（含具体构建参数：strip / LTO / panic=abort / 段哈希算法），Tier-2 与 Tier-3 不做项已列明并给出理由
  - [ ] 量化目标（90% 非专业尝试）与预算分配（控制面 7 : 客户端 3）已写入
  - [ ] 前端不可保密原则与「防逆向 ≠ 防拷机」两条边界已写明
  **QA Scenarios**:
    - Happy: 按设计逐环节核对校验点 → 断言签名与完整性校验覆盖「下载/交付→安装→启动→运行」全链路
    - Error: 篡改安装包一个字节 → 依设计推演 → 断言安装被拒或启动进入受限模式
    - Error: 删除本地试用标记文件（其余冗余保留）→ 推演 → 断言试用期未被重置
    - Error: 系统时间回拨 30 天 → 推演 → 断言试用期不延长且记录审计
    - Error: `docker rm && docker run`（同卷）→ 推演 → 断言试用标记未被重置
    - Error: 提出用商业壳（VMProtect/Themida）或内核驱动反调试 → 断言归入 Tier-3 被拒，并说明理由（Tauri/杀软冲突、误报、维护成本）
    - Error: 提出把授权判定放到前端 JS 以「隐藏算法」→ 断言被拒（前端等同公开，判定必须留 Rust 侧）
  **Evidence**: `.omo/evidence/task-43-installer-hardening.svg`, `.omo/evidence/task-43-installer-hardening.md`
  **Commit**: YES — `docs(design): installer anti-crack & tamper-resistance blueprint`

- [x] 44. 云端授权服务与总管理后台设计图（数据模型 / API 契约 / 后台交互 / 密钥轮换）

  **What to do**:
  - 产出 `docs/design/licensing-server-and-admin-console.svg`：部署与组件图（admin-console → licensing-server API → 数据库 → 签名私钥保管），含鉴权边界与审计入口；同时画出与网关侧的交互（激活 / 心跳 / Token 校验）
  - 产出 `docs/design/licensing-data-model.md`：数据模型（可用 mermaid ER 图）至少含 `tenant`、`device`（机器码指纹）、`activation_code`（状态：issued/bound/revoked/reissued、绑定设备、tier、有效期、来源订单）、`license/lease`、`heartbeat`、`nonce_cache`（防重放）、`signing_key`（kid、状态、启用时间）、`audit_log`
  - **设备身份与容器形态**：`device` 须能表达「同一宿主机上的容器」——指纹来自**宿主锚点**，容器重建/升级**不产生新 device 记录、不触发换机重发**；`device` 建议记录部署形态（native / docker）与其镜像 digest 供运维溯源（口径与任务 41/59 一致）
  - 产出 `docs/design/licensing-api.md`：API 契约（方法/路径/请求/响应/错误码/幂等语义），至少含：`POST /activation`（激活+绑定）、`POST /heartbeat`（**须携带最近审计回执的序号区间**）、`POST /verify`（**A 档**服务端二次校验：验签 + ±5min 窗口 + Nonce 防重放）、**`POST /audit/receipt`（B 档审计回执：字段白名单 `{device_mid, lease_id, seq_from, seq_to, count, payload_digest, ts, sig}`，服务端按白名单拒收任何业务字段）**、`POST /admin/codes/issue`、`POST /admin/codes/{id}/revoke`、`POST /admin/codes/{id}/reissue`、`GET /admin/codes`、`GET /admin/devices`
  - **二次校验档位（本轮定案）**：数据模型与契约须表达 **A/B/C 三档**（档位为租户级/设备级配置，随 Lease Token 下发）；A 档走 `/verify` 全链校验，**B 档（默认）走网关侧校验 + `/audit/receipt` 回执**，C 档仅心跳。云端需能检测**序号跳空/回退/回执缺失**并出告警。契约须明确：**B 档断网时网关仍持续转发，回执允许延迟补报**
  - 产出 `docs/design/admin-console-wireframe.md`：总管理后台页面与交互线框（激活码列表/筛选/详情、**废弃确认二次弹窗（明示原设备将立即失效）**、重发对话框、设备与租户列表、审计日志页）
  - **密钥管理**：Ed25519 签名密钥的生成、保管（KMS/离线保管）、`kid` 多密钥并存与轮换流程、客户端内置公钥的升级路径（避免换密钥需重发所有客户端）
  - **管理后台自身安全**：管理员账号体系（RBAC，为任务 57 提供输入）、双人复核高危操作（废弃/重发）可选、全量操作审计

  **Must NOT do**:
  - 不得写实现代码
  - 不得设计「客户端可自行解绑」的接口
  - 不得把签名私钥放在与业务 API 同一可写位置（须明确保管方案）

  **Recommended Agent Profile**: Category: `deep` — 服务端与后台设计
  **Parallelization**: Wave 0（与 40-43 并行）；**Blocked By**: 无；**Blocks**: 45, 46, 47, 48
  **References**: 第一轮（厂商运营云授权服务）；第三轮（云端下发 Lease Token、云端校验签名 + ±5min 时间窗口 + Nonce 防重放）；本轮新增（总管理后台废弃/重发）
  **Acceptance Criteria**:
  - [ ] 数据模型 ≥8 张表，`activation_code` 含完整生命周期字段与状态枚举
  - [ ] API 契约 ≥8 个端点，其中废弃/重发端点明确标注「仅厂商管理员可用」
  - [ ] 密钥轮换流程完整（生成 → 分发 → 双密钥并存 → 旧密钥退役 → 客户端公钥升级）
  - [ ] 后台线框含废弃操作的二次确认与影响提示
  - [ ] 设备身份含部署形态（native / docker）字段，且明确容器重建不触发换机重发
  - [ ] 覆盖 A/B/C 三档契约与 `/audit/receipt` 字段白名单（回执不得带业务字段）
  **QA Scenarios**:
    - Happy: 按 API 契约走一遍「发码 → 激活绑定 → 心跳 → 废弃 → 重发 → 新机激活」→ 断言契约字段闭环无缺失
    - Happy: 按 **B 档**走一遍「激活 → 直连客户 Broker 转发 → 回执上报 → 心跳携带区间 → 云端跳空告警」→ 断言契约闭环
    - Error: 要求客户端直接调用废弃接口 → 断言设计上返回 403（仅管理员）
    - Error: 激活响应被重放到另一台设备 → 断言服务端 Nonce + 机器码绑定双重拒绝
    - Error: 同一宿主上容器重建后上报 → 断言识别为同一 device，不触发重发
  **Evidence**: `.omo/evidence/task-44-licensing-server-and-admin-console.svg`, `.omo/evidence/task-44-licensing-data-model.md`, `.omo/evidence/task-44-licensing-api.md`, `.omo/evidence/task-44-admin-console-wireframe.md`
  **Commit**: YES — `docs(design): licensing server & admin console blueprint`

- [x] 59. Linux 容器化交付设计图（宿主机指纹锚点 / 持久卷 / 镜像签名 / 离线分发）

  **What to do**:
  - 产出 `docs/design/container-delivery.svg`：容器化部署拓扑与运行时视图，必须画清 **宿主 Docker 引擎 → 容器内 iot-daq 进程 → 宿主只读锚点挂载（`/etc/machine-id`、`/sys/class/dmi/id/product_uuid`、宿主指纹文件）→ 宿主读写持久卷（`/var/lib/iot-daq`）→ 南向设备（局域网 / 串口）→ 北向 MQTT（宿主网络出口）**；并标注三个区域：宿主可篡改区 / 容器运行区 / **镜像供应链区**；明确标注 **不挂载 `/var/run/docker.sock`**（避免放大逃逸面与控制面）
  - 产出 `docs/design/container-machine-binding.md`：
    - **宿主机指纹锚点方案**：容器内机器码**只来源宿主机**（`--mount type=bind,src=/etc/machine-id,dst=/host/etc/machine-id,readonly`、`/sys/class/dmi/id/product_uuid:ro`、宿主首个物理网卡 MAC 由安装脚本采集并写入只读的 `host-fingerprint.json`）
    - **必须写明禁用容器内锚点的理由**：容器 `/etc/machine-id` 由 runtime 生成、MAC 为 veth 随机、主机名为容器 ID —— `docker rm && docker run` 后全部变化 → 授权立即失效，且 SQLCipher 本地库（HKDF 机器码派生密钥，见任务 34）直接解不开
    - **锚点缺失降级**：宿主无可读锚点（精简宿主/受限内核）时，由安装脚本采集宿主信息并生成**签名指纹文件**（HMAC）只读挂载进容器；容器只读该文件
    - **与一机一码衔接**：激活请求携带的机器码 = 宿主锚点组合哈希；同一宿主上重建/升级容器 → 机器码不变；宿主重装系统 → 机器码变化 → 走「总后台废弃 → 重发」（与任务 42 状态机一致）
  - 产出 `docs/design/container-persistence-layout.md`：
    - **持久卷布局**：`/var/lib/iot-daq/{queue.db, telemetry.db, config/, logs/, license/, trial/, host-fingerprint.json}` 全部落宿主命名卷或宿主绑定挂载；**镜像可写层承载的业务/授权数据清单为「空」**
    - **镜像内不含敏感物**清单：无私钥、无激活码、无宿主指纹、无默认口令、无客户配置
    - **重建语义推演**两条结论：① 同卷重建 → 试用期与租约保持；② 空卷重建 → 视为新装，但**宿主指纹相同 → 云端识别为同一设备**，配合云端首次激活时间（任务 23）使「换容器重置试用」不成立
  - 产出 `docs/design/container-supply-chain.md`：多架构 `buildx`（linux/amd64 + linux/arm64）、基础镜像 digest pin、**cosign 签名与 `cosign verify`**、SBOM、compose 以 digest 固定、**离线 tar 包哈希清单 + 校验脚本**、私有 registry 可选
  - 产出 `docs/design/container-deploy.md`：部署与设备接入设计（`--device /dev/ttyUSBx --group-add dialout` 串口、`--network host` 局域网直连与广播发现、管理界面端口暴露、`--memory/--cpus` 资源限制、`restart: unless-stopped`、日志驱动；离线现场一键安装流程：`docker load` → 校验哈希 → 注入宿主指纹 → `docker compose up -d` → 健康检查）
  - 明确**不做清单**：不做 Kubernetes/Helm（首期）、不做镜像内自更新（OTA 走任务 35 应用层）、不挂 Docker socket、不用 `--privileged`、不做 Windows 容器

  **Must NOT do**:
  - 不得写实现代码（不写 Dockerfile、不建 `deploy/`）
  - 不得设计「机器码来自容器内部」的任何方案
  - 不得设计把试用/租约/授权状态放容器可写层的方案
  - 不得设计需要 `--privileged` 或挂载 Docker socket 的方案
  - 不得把 Docker 交付设计成 Windows 侧的替代品（Windows 走 Tauri 原生安装包）

  **Recommended Agent Profile**: Category: `deep` — 容器化与授权边界设计
  **Parallelization**: Wave 0（与 40-44 并行）；**Blocked By**: 无；**Blocks**: 60, 61

  **References**: 本轮决议（Linux 交付采用 Docker 安装形式）；任务 41（指纹锚点）、42（激活状态机）、43（分发物防篡改）、44（云端数据模型）；librarian（buildx 多架构 / cosign / 容器内 machine-id 不稳定）
  **Acceptance Criteria**:
  - [ ] `container-delivery.svg` 含宿主 / 容器 / 设备 / 云端四层，且标注「只读锚点挂载」与「不挂 Docker socket」
  - [ ] 机器码锚点方案写明 ≥3 个宿主来源 + 容器内锚点禁用理由 + 锚点缺失降级路径
  - [ ] 持久卷布局覆盖授权/试用/队列/配置/日志五类，且镜像可写层承载清单为「空」
  - [ ] 重建语义推演给出两条结论（同卷不重置 / 空卷仍识别为同设备）
  - [ ] 供应链含 cosign 签名 + digest pin + 离线包校验脚本
  - [ ] 明确不做清单（无 k8s、无 privileged、不挂 socket）
  **QA Scenarios**:
    - Happy: 按图推演「docker load → 注入宿主指纹 → compose up → 激活 → 同卷重建容器」→ 断言机器码与租约不变、无需重新激活
    - Error: 提出「用容器内 machine-id 作锚点」→ 断言设计上拒绝，并给出「容器重建 → 机器码变化 → 授权失效 + 本地库解不开」的推演
    - Error: 用空卷 `docker run`（模拟换容器重置试用）→ 推演 → 断言宿主指纹相同 + 云端首次激活时间 → 试用不被重置
    - Error: 要求挂载 `/var/run/docker.sock` → 断言设计上拒绝
    - Error: 串口方案写 `--privileged` → 断言不合格，退回改为 `--device` + 组权限
  **Evidence**: `.omo/evidence/task-59-container-delivery.svg`, `.omo/evidence/task-59-container-machine-binding.md`, `.omo/evidence/task-59-container-persistence-layout.md`, `.omo/evidence/task-59-container-supply-chain.md`, `.omo/evidence/task-59-container-deploy.md`
  **Commit**: YES — `docs(design): container delivery & host-anchored licensing blueprint`

- [x] 63. 客户端界面设计（设计系统 / 信息架构 / 页面清单 / 线框 / 交互 / 授权触点）

  **What to do**:
  - **设计系统与 token**：产出 `docs/design/ui-design-system.md`，定色彩（品牌 + 语义状态色：`ok/warn/danger/info/unknown`，工业设备语境）、尺度（间距 / 圆角 / 字号 / 行高 / 表格密度 / 侧栏与顶栏尺寸）、布局骨架（侧栏 + 顶栏 + 全局横幅槽 + 页头 + 内容区）、共享业务组件清单（落地为 `ui-kit/`）
  - **客户端信息架构与页面清单**：产出 `docs/design/ui-gateway-console.md`，覆盖 13 个页面 + 首次初始化向导；每页写清「目标 / 关键元素 / 关键交互 / 依赖 API / 验收要点」
  - **关键页面线框**：总览、设备接入（新增设备抽屉 + 测试连接）、点位与映射（**含 CSV/XLSX 批量导入 3 步校验**）、北向转发（**每路出口编码单选 + JSON 精度与性能提示 + 编码一致性自检**）、实时监控、**授权与激活（客户端唯一授权触点）**、设置/诊断/备份摘要
  - **关键流程**：首次初始化向导（4 步）、从零到数据上云的 6 步、断网续传、**三种授权降级场景（试用到期 / 心跳超期 / 被后台废弃）的横幅文案与恢复路径**、换机流程（客户端侧视角）、全局横幅优先级
  - **可点击高保真原型**：产出 `docs/design/prototype/gateway-console.html`（单文件零依赖、Light 主题、数据驱动渲染、危险操作弹窗、角色门控演示）
  - **现场约束**：实时数据 1s 节流、数值等宽字体、陈旧行转灰、空态必须给下一步动作、危险操作二次确认、点击区域 ≥32px、目标分辨率 ≥1366×768

  **Must NOT do**:
  - 不得写实现代码（不建 `web-console/`，不写 Vue 组件）
  - 不得在客户端设计任何废弃 / 解绑 / 重置试用的入口（`/api/license/*` 不得出现 revoke / unbind / reset-trial）
  - 不得把授权判定或防篡改判定设计到前端（前端只做展示与可见性）
  - 不得设计深色主题、多语言或移动端适配（首期不做）
  - 不得为两端设计不同的组件库或各自独立的 token 体系

  **Recommended Agent Profile**: Category: `deep` — 界面与交互设计
  **Parallelization**: Wave 0（与 40-44、59、64 并行）；**Blocked By**: 无；**Blocks**: 27, 29, 65, 67, 68, 69

  **References**: 任务 40（信任边界与残余风险）、41（机器码与锚点）、42（激活与换机状态机）、44（API 契约）、59（容器部署形态）；本轮决议「界面设计」「客户端页面清单」「角色可见性」「关键交互硬约束」「点位表批量操作」「客户端授权触点」
  **Acceptance Criteria**:
  - [ ] `ui-design-system.md` 含完整 token（色彩 / 尺度 / 布局 / 阴影）+ 共享组件清单 + 文案规范 + 无障碍要求 + 明确不做清单
  - [ ] `ui-gateway-console.md` 覆盖 13 页 + 向导，每页含「目标 / 元素 / 交互 / 依赖 API / 验收」，含 12 页与后端 API 的对应表
  - [ ] 点位表批量导入线框含**行号 + 原因 + 允许值**三要素与「仅导入有效行」选项
  - [ ] 北向转发页含每路出口编码单选与**就地精度提示**（2^53−1 转字符串）与编码一致性自检入口
  - [ ] 授权页含机器码 + 锚点来源 + 试用倒计时 + 激活 + **申请换机**；**无**废弃/解绑/重置试用入口
  - [ ] 三种降级场景各有横幅文案 + 原因 + 恢复路径；全局横幅优先级已定义
  - [ ] 原型可在浏览器打开并逐页点击；Light 主题；无 JS 报错
  - [ ] 设计稿经用户 explicit 确认
  **QA Scenarios**:
    - Happy: 按原型走「初始化向导 → 新增设备（测试连接）→ 导入点表（含错误行）→ 配北向出口（选 json 见精度提示）→ 实时监控 → 授权页申请换机」→ 断言每步都有明确的下一动作与反馈
    - Error: 在授权页寻找「解绑 / 重置试用」入口 → 断言不存在（可点元素扫描无命中）
    - Error: 以 viewer 角色打开 → 断言写操作入口不可见且越权页自动回落
    - Error: 点表导入含非法数据类型 / 非法字节序 → 断言校验报告给出行号与允许值，且提供「仅导入有效行」
    - Error: 200 设备场景核对设计 → 断言设计中已明确 1s 节流，未设计无节流直推 DOM
    - Error: 检查设计稿 → 断言无深色主题 / 多语言 / 移动端适配（首轮不做项）
  **Evidence**: `docs/design/ui-design-system.md`, `docs/design/ui-gateway-console.md`, `docs/design/prototype/gateway-console.html`, `.omo/evidence/ui-prototype-gateway.png`, `.omo/evidence/ui-prototype-gateway-verify.log`
  **Commit**: YES — `docs(design): gateway console UI design system & clickable prototype`

- [x] 64. 总管理后台界面设计（后台信息架构 / 激活码与设备页 / 危险操作约束 / 回执异常页）

  **What to do**:
  - 产出 `docs/design/ui-admin-console.md`：后台信息架构（运营 / 授权运营 / 风控 / 系统四组，10 个页面）+ 每页「目标 / 元素 / 交互 / 依赖 API / 验收」
  - **激活码管理（核心页）**：列表（5 种状态：已发放 / 已绑定 / 已废弃 / 已重发 / 试用中）+ 筛选 + 导出 + 预绑定机器码发放；**详情页生命周期时间线**（发放 → 绑定 → 废弃 → 重发）
  - **废弃（本系统最危险操作）**：`DangerConfirmModal` 规范 = 影响清单（「原设备将立即停止北向转发，本地采集继续」+「不可撤销」）+ 原因必填 + 补充说明 ≥10 字 + **输入激活码后 8 位二次校验**
  - **重发**：预绑定新机器码 / 留待首次激活绑定二选一；继承 tier 与有效期可覆盖；建立 `reissued_from` 溯源链；废弃 + 重发同事务且**接口幂等**
  - **设备页与回执页**：设备详情含**回执连续性可视化**（24 小时区间覆盖图，跳空 / 缺失标记）与心跳时序；回执异常页必须含「**回执异常 ≠ 破解**」说明文案
  - **密钥管理**：kid 轮换界面对「旧客户端仍可验签（公钥集而非单钥）」与存量版本覆盖率做显式提示
  - **换机工单**：设计为「一键废弃 + 重发」合并执行，目标 **≤3 次点击**完成
  - 可点击高保真原型：`docs/design/prototype/admin-console.html`

  **Must NOT do**:
  - 不得把管理后台与网关侧设计成同一应用或共用同一导航体系（部署与构建必须分离）
  - 不得设计省略二次确认 / 原因必填的废弃入口
  - 不得让前端持有签名私钥或直连授权数据库
  - 不得在回执异常页设计任何「自动判定为破解 / 自动封禁」的动作（异常须人工核实）

  **Recommended Agent Profile**: Category: `deep` — 界面与交互设计
  **Parallelization**: Wave 0（与 40-44、59、63 并行）；**Blocked By**: 无；**Blocks**: 47, 66, 69

  **References**: 任务 44（数据模型与 API 契约、后台交互与密钥轮换）、42（激活码状态机与换机时序）、46（生命周期实现）、48（B 档审计回执与跳空告警）；式样约束见 `ui-design-system.md`
  **Acceptance Criteria**:
  - [ ] `ui-admin-console.md` 覆盖 10 页，含页面与 `/admin/*` API 对应表与幂等要求
  - [ ] 废弃弹窗四要素齐备：影响清单 / 原因必填 / 补充说明 / 对象名二次校验
  - [ ] 重发支持预绑定与留待激活两种模式，并展示溯源关系
  - [ ] 设备详情含回执连续性可视化（跳空 / 缺失区分标记）与心跳时序
  - [ ] 回执异常页含「异常 ≠ 破解」文案与「转人工核实」路径
  - [ ] 密钥轮换页含「旧客户端仍可验签」与存量覆盖率提示
  - [ ] 换机工单流程 ≤3 次点击完成（废弃 + 重发合并）
  - [ ] 原型可在浏览器打开并逐页点击；无 JS 报错；经用户 explicit 确认
  **QA Scenarios**:
    - Happy（原型实测）: 登录 → 激活码列表 → 详情时间线（4 步）→ 点废弃 → 弹窗含影响提示与 3 个必填项 → 确认；再走重发 → 断言新码与溯源可见
    - Error: 不填原因直接废弃 → 断言提交被拦截（原型层面字段必填标记齐备）
    - Error: 在设备页尝试「一键判为破解」→ 断言设计上无此动作，只能「标记异常（备注）」并转人工
    - Error: 检查轮换设计 → 断言已写明旧客户端仍可验签与存量覆盖率，未暗示「轮换即全网失效」
    - Error: 检查网络请求设计 → 断言写操作均走管理 API，前端无本地状态伪造、无私钥
  **Evidence**: `docs/design/ui-admin-console.md`, `docs/design/prototype/admin-console.html`, `.omo/evidence/ui-prototype-admin.png`, `.omo/evidence/ui-prototype-admin-verify.log`
  **Commit**: YES — `docs(design): vendor admin console UI design & clickable prototype`

- [x] ★ Wave 0 出口条件：**6 份设计图 + 2 份界面设计（含可点击原型）**全部产出 → 汇总呈现给用户 → 等用户 explicit 确认 → 才允许开工任务 1（Monorepo 初始化）
  > ✅ **2026-09-23 用户 explicit 确认（「设计图定稿，更新相关文档，进入开发阶段」）→ Wave 0 关闭，进入开发阶段。**
  > 定稿口径：客户端界面 = `gateway-v2a-glacier.html`（唯一实现基准，九轮反馈迭代，178 项断言 / 36 张截图）；管理后台 = `admin-console.html`；设计决策以本计划 task 40-44 / 59 规格章节为准。
  > ⚠️ 6 张设计图的 SVG 源文件此前在会话内交付、未落盘——本日由架构师按规格章节补绘归档至 `docs/design/*.svg`（含 `activation-reissue.svg`），作为定稿基线文件。

- [x] 1. Monorepo 初始化 + Cargo workspace + CI 骨架

  **What to do**:
  - 创建 Cargo workspace root（iot-daq/Cargo.toml），声明 crates/daemon, crates/licensing-server, crates/protocol-proto, tauri-shell, headless, web-console, admin-console, **ui-kit**（`deploy/` 为容器与部署资产目录，非 Cargo 成员，由任务 60 填充）
  - 初始化 git + .gitignore + .editorconfig + CLAUDE.md
  - 创建 GitHub Actions CI 骨架（.github/workflows/build.yml），**双平台矩阵（windows-latest / ubuntu-latest）**——交付仅 Windows 与 Linux，不引入 macos-latest（浪费 CI 配额且增加无关失败面）；tauri-action 自动构建
  - 创建 `docs/design/` 目录，收纳 Wave 0 已确认的 6 份设计图与 3 份界面设计（`ui-design-system.md` / `ui-gateway-console.md` / `ui-admin-console.md`）及 `prototype/` 原型（**注：6 张 SVG 由架构师同步补绘落盘，工程侧只需建目录结构并确保纳入版本管理，不负责绘制**）
  - 创建 `ui-kit/` 目录占位（两端共用前端基础包：设计 token + 共享业务组件，由任务 27 填充）
  - 创建 `admin-console/` 目录占位（厂商总管理后台，Vue 3 + Vite + TypeScript）
  - 创建 crates/protocol-proto（Protobuf 定义目录），初始 schema 文件占位
  - 创建 crates/daemon/Cargo.toml，声明依赖占位

  **Must NOT do**:
  - 不要在此阶段引入业务逻辑或依赖实际 crate（只建骨架）
  - 不要在此阶段做平台打包（那是 Wave 5 的任务）

  **Recommended Agent Profile**:
  > Category: `quick` — 脚手架与配置，无业务逻辑
  > Skills: [`writing-plans`]
  - **Category**: quick
  - **Skills**: []
  - **Skills Evaluated but Omitted**:
    - `systematic-debugging`: 无业务逻辑，无需调试

  **Parallelization**:
  - **Can Run In Parallel**: YES
  - **Parallel Group**: Wave 1 (with 2, 3, 4, 5, 6, 7)
  - **Blocks**: 全部后续任务
  - **Blocked By**: None

  **References**:
  - Tauri CI 模板: https://github.com/tauri-apps/tauri-docs/blob/v2/src/content/docs/distribute/Pipelines/github.mdx
  - Cargo workspace 官方指南: https://doc.rust-lang.org/cargo/reference/workspaces.html

  **Acceptance Criteria**:
  - [ ] `cargo build --workspace` 不报错（空 crate 通过）
  - [ ] GitHub Actions workflow 文件存在且语法有效
  - [ ] workspace 根 Cargo.toml 正确声明所有 member

  **QA Scenarios**:
  Scenario: 工作区构建通过
    Tool: Bash (cargo)
    Preconditions: workspace 初始化完成
    Steps:
      1. 运行 `cargo build --workspace`
      2. 观察输出无 compile error
    Expected Result: 构建成功，"Finished dev [unoptimized + debuginfo]"
    Failure Indicators: 任何 cargo error 或 warning 关于未声明 member
    Evidence: .omo/evidence/task-1-workspace-build.log

  Scenario: CI workflow 语法有效
    Tool: Bash (act 或 GitHub CLI validate)
    Preconditions: .github/workflows/build.yml 存在
    Steps:
      1. 运行 `cat .github/workflows/build.yml` 查看内容
      2. 确认 matrix 包含 windows-latest/ubuntu-latest（不含 macos-latest）
    Expected Result: workflow 包含双平台矩阵和 tauri-action
    Failure Indicators: 缺少任一平台、或出现 macos-latest
    Evidence: .omo/evidence/task-1-ci-syntax.yml

  **Evidence to Capture**: [ ] task-1-workspace-build.log [ ] task-1-ci-syntax.yml
  **Commit**: YES — `feat(ci): init monorepo workspace and CI pipeline`

- [x] 2. Protobuf 数据模型 schema（TelemetryBatch/DataPoint/AuthBlock）

  **What to do**:
  - 在 crates/protocol-proto 中定义 Protobuf schema（proto/telemetry.proto），包含：
    - TelemetryBatch { repeated DataPoint points; int64 ts; string gateway_id; AuthBlock auth; }
    - DataPoint { string device_id; string point_id; bytes value; string unit; int64 ts; int32 quality; }
    - AuthBlock { string mid; string nonce; int64 ts; bytes sig; }
  - 使用 prost-build 或 tonic-build 生成 Rust 代码
  - **JSON 编码的字段映射约定（本轮定案，与任务 62 对齐）**：schema 须为每个字段标注 JSON 编码规则 ——
    - `int64`（`ts`、各类计数器）在 JSON 中**编码为字符串**（超出 2^53−1 会静默丢精度）
    - `bytes`（`value`、`sig`）在 JSON 中 base64 + 类型前缀标识
    - `f64` 的 `NaN` / `±Infinity` 编码为 `null`，有效性借 `quality` 表达
    - 字段名统一 snake_case，**Protobuf 与 JSON 两种编码的语义必须一一对应**
  - 编写 protobuf schema 校验测试（解析/序列化往返）

  **Must NOT do**:
  - 不要直接签 Protobuf 序列化字节（应签业务语义哈希，见任务 21）
  - 不要在此阶段实现业务逻辑
  - **不要让 `int64` 在 JSON 路径下退化为 number**（精度丢失不可接受）
  - 不要为 JSON 另起一套字段命名（必须与 Protobuf 可一一映射）

  **Recommended Agent Profile**:
  > Category: `quick` — schema 定义与生成，无业务逻辑
  > Skills: [`writing-plans`]
  - **Category**: quick
  - **Skills**: []
  - **Skills Evaluated but Omitted**:
    - `systematic-debugging`: schema 校验简单，无需调试

  **Parallelization**:
  - **Can Run In Parallel**: YES
  - **Parallel Group**: Wave 1 (with 1, 3, 4, 5, 6, 7)
  - **Blocks**: 任务 21（AuthBlock 签名依赖此 schema）、任务 62（双编码字段映射）
  - **Blocked By**: None

  **References**:
  - prost-build: https://docs.rs/prost-build/latest/prost_build/
  - Protobuf 官方教程: https://protobuf.dev/programming-guides/proto3/

  **Acceptance Criteria**:
  - [ ] protobuf schema 文件存在且语法有效
  - [ ] `cargo test --package protocol-proto` PASS（序列化往返测试）
  - [ ] 生成的 Rust 代码包含 TelemetryBatch/DataPoint/AuthBlock struct
  - [ ] JSON 字段映射规则已写入 schema 注释（`int64`→string、`bytes`→base64、`NaN`→null、snake_case）

  **QA Scenarios**:
  Scenario: Protobuf 序列化往返
    Tool: Bash (cargo test)
    Preconditions: protocol-proto crate 已生成
    Steps:
      1. 构建 TelemetryBatch 包含 3 个 DataPoint
      2. 序列化为 bytes
      3. 反序列化回 TelemetryBatch
      4. 断言所有 DataPoint 字段一致
    Expected Result: 往返后数据完全一致
    Failure Indicators: 字段丢失或类型不匹配
    Evidence: .omo/evidence/task-2-proto-roundtrip.log

  Scenario: AuthBlock 结构正确
    Tool: Bash (cargo test)
    Preconditions: schema 生成完成
    Steps:
      1. 构建 AuthBlock { mid, nonce, ts, sig }
      2. 断言 mid 为字符串、nonce 为字符串、ts 为 int64、sig 为 bytes
    Expected Result: 结构字段类型正确
    Failure Indicators: 字段类型与 proto 定义不符
    Evidence: .omo/evidence/task-2-authblock-structure.log

  **Evidence to Capture**: [ ] task-2-proto-roundtrip.log [ ] task-2-authblock-structure.log
  **Commit**: YES — `feat(proto): add TelemetryBatch/DataPoint/AuthBlock schema`

- [x] 3. 机器码指纹模块（mid 5.0，多源 + HMAC）

  **What to do**:
  - 在 crates/daemon 中创建 auth/machine_id.rs
  - 集成 `mid 5.0.1` crate：Windows 用 WMI (ComputerSystemProduct UUID + BIOS + BaseBoard)，Linux 用 /etc/machine-id + /sys/class/dmi/id/product_uuid
  - 用固定应用密钥做 HMAC-SHA256 哈希生成稳定机器码指纹
  - 实现 `get_machine_id() -> String` + `get_machine_fingerprint() -> String`
  - 编写测试：验证同一机器多次调用指纹一致

  **Must NOT do**:
  - 不要只用 MachineGuid（重装系统即失效）
  - 不要暴露明文硬件 ID（只输出 HMAC 哈希）

  **Recommended Agent Profile**:
  > Category: `quick` — 库集成与哈希
  > Skills: []
  - **Category**: quick
  - **Skills**: []

  **Parallelization**:
  - **Can Run In Parallel**: YES
  - **Parallel Group**: Wave 1 (with 1, 2, 4, 5, 6, 7)
  - **Blocks**: 任务 22（云授权客户端依赖机器码）
  - **Blocked By**: None

  **References**:
  - mid crate: https://github.com/doroved/mid
  - machine-uid crate: https://crates.io/crates/machine-uid

  **Acceptance Criteria**:
  - [ ] `get_machine_fingerprint()` 返回一致的 SHA-256 哈希
  - [ ] 同一环境下两次调用指纹相同
  - [ ] 单元测试覆盖 Windows 和 Linux 路径

  **QA Scenarios**:
  Scenario: 机器指纹一致性
    Tool: Bash (cargo test)
    Preconditions: machine_id 模块已实现
    Steps:
      1. 调用 `get_machine_fingerprint()` 两次
      2. 断言两次返回值相同
      3. 断言返回值长度为 64 字符（SHA-256 hex）
    Expected Result: 一致且 64 字符
    Failure Indicators: 两次返回不同或长度不对
    Evidence: .omo/evidence/task-3-fingerprint-consistency.log

  Scenario: 硬件源完整性
    Tool: Bash (cargo test)
    Preconditions: 实现已集成 mid
    Steps:
      1. 运行 `get_machine_id()` 
      2. 确认返回的原始 ID 非空
      3. 确认 HMAC 哈希已计算
    Expected Result: 原始 ID 非空 + 哈希已生成
    Failure Indicators: 返回空或哈希计算失败
    Evidence: .omo/evidence/task-3-hardware-source.log

  **Evidence to Capture**: [ ] task-3-fingerprint-consistency.log [ ] task-3-hardware-source.log
  **Commit**: YES — `feat(auth): implement machine-code fingerprint with mid`

- [x] 4. 配置管理框架（TOML/YAML 序列化 + 热重载）

  **What to do**:
  - 创建 crates/daemon/src/config.rs
  - 使用 `serde` + `toml` 定义 GatewayConfig 结构：
    - 设备列表（protocol, address, points, frequency）
    - MQTT 配置（broker, topic, qos, tls）
    - 授权配置（cloud url, heartbeat interval）
    - 缓存配置（sqlite path, max_size, retention_days）
    - 安全配置（tls cert path, web auth）
  - 支持配置文件热重载（inotify/ReadDirectoryChangesW）
  - 编写配置解析单元测试

  **Must NOT do**:
  - 不要在此阶段实现配置界面的 UI 逻辑（那是 Wave 4 的任务）
  - 不要做配置加密（那是任务 34 的 SQLCipher）

  **Recommended Agent Profile**:
  > Category: `quick` — 结构定义与反序列化
  > Skills: []
  - **Category**: quick
  - **Skills**: []

  **Parallelization**:
  - **Can Run In Parallel**: YES
  - **Parallel Group**: Wave 1 (with 1, 2, 3, 5, 6, 7)
  - **Blocks**: 所有需要读取配置的任务（8-34）
  - **Blocked By**: None

  **References**:
  - serde: https://serde.rs/
  - toml: https://crates.io/crates/toml
  - config-rs (可选): https://crates.io/crates/config

  **Acceptance Criteria**:
  - [ ] GatewayConfig 结构可解析示例 TOML 文件
  - [ ] 热重载触发时配置自动更新
  - [ ] `cargo test --package daemon config` PASS

  **QA Scenarios**:
  Scenario: TOML 配置解析
    Tool: Bash (cargo test)
    Preconditions: config.rs 已实现
    Steps:
      1. 创建示例 config.toml（含 1 个 Modbus 设备 + MQTT 配置）
      2. 调用 `GatewayConfig::load("config.toml")`
      3. 断言设备协议为 Modbus，频率为 100ms
    Expected Result: 解析成功，字段值正确
    Failure Indicators: 解析失败或字段值错误
    Evidence: .omo/evidence/task-4-config-parse.log

  Scenario: 热重载生效
    Tool: Bash (cargo test)
    Preconditions: 配置加载完成
    Steps:
      1. 修改 config.toml 的设备频率
      2. 触发热重载
      3. 断言配置已更新
    Expected Result: 新频率生效
    Failure Indicators: 配置未更新
    Evidence: .omo/evidence/task-4-hot-reload.log

  **Evidence to Capture**: [ ] task-4-config-parse.log [ ] task-4-hot-reload.log
  **Commit**: YES — `feat(config): add GatewayConfig with TOML and hot-reload`

- [x] 5. 日志与可观测性（tracing + 分级日志 + 结构化）

  **What to do**:
  - 在 crates/daemon/src/logging.rs 中集成 `tracing` + `tracing-subscriber`
  - 配置分级日志（DEBUG/INFO/WARN/ERROR）
  - 支持日志文件轮转（按大小/时间）
  - 支持远程日志推送配置（可选地址）
  - 编写测试：验证日志级别过滤和输出格式

  **Must NOT do**:
  - 不要在此阶段实现日志前端展示（那是任务 30）

  **Recommended Agent Profile**:
  > Category: `quick` — 库集成
  > Skills: []
  - **Category**: quick
  - **Skills**: []

  **Parallelization**:
  - **Can Run In Parallel**: YES
  - **Parallel Group**: Wave 1 (with 1, 2, 3, 4, 6, 7)
  - **Blocks**: 所有需要日志的任务
  - **Blocked By**: None

  **References**:
  - tracing: https://docs.rs/tracing/latest/tracing/
  - tracing-subscriber: https://docs.rs/tracing-subscriber/latest/tracing_subscriber/

  **Acceptance Criteria**:
  - [ ] `tracing_subscriber` 初始化成功
  - [ ] 日志按级别过滤输出
  - [ ] 文件轮转正常工作

  **QA Scenarios**:
  Scenario: 分级日志输出
    Tool: Bash (cargo test)
    Preconditions: logging 初始化
    Steps:
      1. 设置日志级别 WARN
      2. 分别输出 debug/info/warn/error
      3. 断言只有 warn 和 error 出现在输出
    Expected Result: WARN 和 ERROR 输出，DEBUG/INFO 过滤
    Failure Indicators: 所有级别都输出或 WARN/ERROR 缺失
    Evidence: .omo/evidence/task-5-log-level.log

  **Evidence to Capture**: [ ] task-5-log-level.log
  **Commit**: YES — `feat(logging): add tracing-based structured logging`

- [x] 6. 核心错误类型 + 统一 Result + 错误码

  **What to do**:
  - 创建 crates/daemon/src/error.rs
  - 定义 `DaemonError` 枚举：ProtocolError, ConfigError, AuthError, StorageError, MqttError, NetworkError, SecurityError
  - 实现 `thiserror` derive + 错误码（u16）
  - 定义统一 `DaemonResult<T> = Result<T, DaemonError>`
  - 编写测试：每个变体可构造 + Display 格式化正确

  **Must NOT do**:
  - 不要在此阶段实现错误恢复逻辑（那是各任务内部的逻辑）

  **Recommended Agent Profile**:
  > Category: `quick` — 类型定义
  > Skills: []
  - **Category**: quick
  - **Skills**: []

  **Parallelization**:
  - **Can Run In Parallel**: YES
  - **Parallel Group**: Wave 1 (with 1, 2, 3, 4, 5, 7)
  - **Blocks**: 所有需要 Result 的任务
  - **Blocked By**: None

  **References**:
  - thiserror: https://docs.rs/thiserror/latest/thiserror/

  **Acceptance Criteria**:
  - [ ] `DaemonError` 所有变体可构造
  - [ ] `DaemonResult<T>` 类型别名可用
  - [ ] `cargo test --package daemon error` PASS

  **QA Scenarios**:
  Scenario: 错误类型构造与 Display
    Tool: Bash (cargo test)
    Preconditions: error.rs 已实现
    Steps:
      1. 构造 DaemonError::ProtocolError("modbus timeout")
      2. 断言 Display 输出包含 "ProtocolError" 和 "modbus timeout"
      3. 断言 error_code() 返回非零 u16
    Expected Result: Display 和 code 正确
    Failure Indicators: Display 格式错误或 code 为 0
    Evidence: .omo/evidence/task-6-error-display.log

  **Evidence to Capture**: [ ] task-6-error-display.log
  **Commit**: YES — `feat(error): define DaemonError enum and DaemonResult`

- [x] 7. 测试基础设施（cargo test + vitest + protobuf 校验工具）

  **What to do**:
  - 配置 workspace-level cargo test（workspace default-features 不影响）
  - 在 crates/protocol-proto 中添加 `cargo test` 校验脚本
  - 初始化 web-console/vitest + TypeScript + tsconfig
  - 创建 vitest 配置和示例测试
  - 添加 `cargo test --workspace` 为 CI 门禁

  **Must NOT do**:
  - 不要在此阶段写业务测试（那是各任务自己的 TDD）
  - 不要做 UI 组件测试（那是 Wave 4）

  **Recommended Agent Profile**:
  > Category: `quick` — 测试框架搭建
  > Skills: []
  - **Category**: quick
  - **Skills**: []

  **Parallelization**:
  - **Can Run In Parallel**: YES
  - **Parallel Group**: Wave 1 (with 1, 2, 3, 4, 5, 6)
  - **Blocks**: 所有任务的测试阶段（TDD 的 GREEN 阶段）
  - **Blocked By**: None

  **References**:
  - vitest: https://vitest.dev/
  - cargo test: https://doc.rust-lang.org/cargo/commands/cargo-test.html

  **Acceptance Criteria**:
  - [ ] `cargo test --workspace` PASS（空测试通过）
  - [ ] `npx vitest run` 在 web-console 中 PASS
  - [ ] CI 中 `cargo test` 为必需步骤

  **QA Scenarios**:
  Scenario: 工作区测试通过
    Tool: Bash (cargo test)
    Preconditions: 测试基础设施配置完成
    Steps:
      1. 运行 `cargo test --workspace`
      2. 断言所有 crate 测试通过
    Expected Result: 测试全部 PASS
    Failure Indicators: 任何 compile error 或 test failure
    Evidence: .omo/evidence/task-7-cargo-test.log

  Scenario: vitest 配置有效
    Tool: Bash (npx vitest)
    Preconditions: vitest.config.ts 已创建
    Steps:
      1. 运行 `cd web-console && npx vitest run`
      2. 断言测试通过
    Expected Result: vitest PASS
    Failure Indicators: 配置错误或测试失败
    Evidence: .omo/evidence/task-7-vitest.log

  **Evidence to Capture**: [ ] task-7-cargo-test.log [ ] task-7-vitest.log
  **Commit**: YES — `feat(testing): init cargo test + vitest infrastructure`

- [x] 8. Driver trait + 地址解析器 + 断线重连基类

  **What to do**:
  - 定义 `#[async_trait] pub trait Driver { connect, read(points), write, disconnect }`
  - 实现 `PointAddressParser`（DB1.DBX0.0/M100/D100 → 字节偏移）
  - 实现带指数退避的 `Reconnector` 基类
  - 编写 trait 和地址解析单元测试

  **Must NOT do**: 不要在此实现具体协议
  **Recommended Agent Profile**: Category: `deep` — 核心抽象设计
  **Parallelization**: Wave 2 (blocks 9-14, 15)

  **References**: async-trait crate, tokio-modbus 地址格式
  **Acceptance Criteria**: `[ ] trait compile PASS [ ] 地址解析单元测试 PASS`
  **QA Scenarios**:
    - Happy: 解析 "DB1.DBX0.0" → {db:1, start:0, bit:true}, 断言正确
    - Error: 解析无效地址 "INVALID" → 断言返回 ProtocolError
  **Evidence**: .omo/evidence/task-8-driver-trait.log
  **Commit**: YES — `feat(driver): define Driver trait and PointAddressParser`

- [x] 9. Modbus TCP/RTU 驱动（tokio-modbus 0.17）

  **What to do**:
  - 实现 `ModbusDriver` 结构体，适配 Driver trait
  - 支持 Modbus TCP（端口配置）和 RTU（串口配置）
  - 使用 `tokio-modbus 0.17.0` crate
  - 实现批量读取（03/04 function codes）和单点写入（06）
  - 编写测试：模拟 Modbus TCP 服务器，验证采集精度

  **Must NOT do**: 不要实现 ASCII 模式（除非 P1 需要）
  **Recommended Agent Profile**: Category: `deep` — 协议实现
  **Parallelization**: Wave 2 (with 10, 11, 12, 13, 14)

  **References**: tokio-modbus 0.17.0, Modbus function codes
  **Acceptance Criteria**: `[ ] cargo test --package daemon modbus PASS [ ] 采集精度 ±1 bit`
  **QA Scenarios**:
    - Happy: 连接模拟 Modbus TCP 服务器，读寄存器 40001=1234，断言值正确
    - Error: 连接超时/服务器不可达，断言返回 ProtocolError 且触发重连
  **Evidence**: .omo/evidence/task-9-modbus-tcp.log
  **Commit**: YES — `feat(driver): implement Modbus TCP/RTU driver`

- [ ] 10. OPC UA 驱动（async-opcua 0.19）

  **What to do**:
  - 实现 `OpcuaDriver` 结构体，适配 Driver trait
  - 使用 `async-opcua-server 0.19.0`（freeopcua）
  - 支持 Endpoint 配置、节点浏览、读取/写入
  - 注意 MPL-2.0 许可证（不修改其源码）
  - 编写测试：连接示例 OPC UA 服务器

  **Must NOT do**: 不要修改 async-opcua 源码
  **Recommended Agent Profile**: Category: `deep` — 协议实现
  **Parallelization**: Wave 2 (with 9, 11, 12, 13, 14)

  **References**: async-opcua 0.19.0, OPC UA node browse
  **Acceptance Criteria**: `[ ] cargo test --package daemon opcua PASS [ ] 节点浏览正常`
  **QA Scenarios**:
    - Happy: 浏览 OPC UA 服务器节点树，读取变量值，断言成功
    - Error: 连接无效 Endpoint，断言返回 AuthError/ProtocolError
  **Evidence**: .omo/evidence/task-10-opcua.log
  **Commit**: YES — `feat(driver): implement OPC UA driver`

- [ ] 11. 西门子 S7 驱动（自研 S7comm）

  **What to do**:
  - 实现 `S7Driver` 结构体，适配 Driver trait
  - 自研 S7comm TCP 报文交互（COTP 连接 → Setup Communication → Read/Write Var）
  - 处理 S7-200/300/400/1200/1500 的机架号/槽号差异
  - 实现 DB 块读写（DB1.DBX0.0 → 字节偏移）
  - 实现大端序解析（西门子 Big-Endian）
  - 编写测试：模拟 S7comm 服务器

  **Must NOT do**: 不要使用 PLC4X Rust 绑定（未生产可用）
  **Recommended Agent Profile**: Category: `deep` — 协议实现（高难度）
  **Parallelization**: Wave 2 (with 9, 10, 12, 13, 14)

  **References**: PLC4X mspec 定义, S7comm 报文格式
  **Acceptance Criteria**: `[ ] cargo test --package daemon s7 PASS [ ] DB 块读写正确 [ ] 大端序解析正确`
  **QA Scenarios**:
    - Happy: 读模拟 S7 服务器 DB1.DBW0=0x1234，断言 Big-Endian 解析为 0x1234
    - Error: 连接 S7-300 机架 0 槽 2，断言正确构建 COTP 报文
  **Evidence**: .omo/evidence/task-11-s7comm.log
  **Commit**: YES — `feat(driver): implement Siemens S7 S7comm driver`

- [ ] 12. 三菱 MC 驱动（自研 3E/4E 帧）

  **What to do**:
  - 实现 `MitsubishiMcDriver` 结构体，适配 Driver trait
  - 自研 MC 协议（3E/4E 帧）TCP/UDP 报文
  - 实现批量读取（0401）、随机读取指令
  - 实现小端序解析（三菱 Little-Endian）
  - 编写测试：模拟 MC 服务器

  **Must NOT do**: 不要依赖 Java 库
  **Recommended Agent Profile**: Category: `deep` — 协议实现
  **Parallelization**: Wave 2 (with 9, 10, 11, 13, 14)

  **References**: MELSEC 3E/4E 帧格式, MC 协议命令码
  **Acceptance Criteria**: `[ ] cargo test --package daemon mc PASS [ ] 小端序解析正确`
  **QA Scenarios**:
    - Happy: 读模拟 MC 服务器 D100=0xABCD，断言 Little-Endian 解析正确
    - Error: 无效站号，断言返回 ProtocolError
  **Evidence**: .omo/evidence/task-12-mitsubishi-mc.log
  **Commit**: YES — `feat(driver): implement Mitsubishi MC driver`

- [ ] 13. HTTP 采集驱动

  **What to do**:
  - 实现 `HttpDriver` 结构体，适配 Driver trait
  - 支持 HTTP GET/POST 拉取（REST API、传感器 HTTP 端点）
  - 支持 JSONPath 提取数据点值
  - 使用 `reqwest` crate
  - 编写测试：模拟 HTTP 服务器

  **Must NOT do**: 不要实现 HTTP 推送（那是第三方 MQTT 接入）
  **Recommended Agent Profile**: Category: `quick` — 标准 HTTP 集成
  **Parallelization**: Wave 2 (with 9-12, 14)

  **References**: reqwest, JSONPath
  **Acceptance Criteria**: `[ ] cargo test --package daemon http PASS [ ] JSONPath 提取正确`
  **QA Scenarios**:
    - Happy: GET http://sim/telemetry 返回 {"temp":36.5}，断言 temp=36.5
    - Error: 服务器 500，断言返回 ProtocolError
  **Evidence**: .omo/evidence/task-13-http.log
  **Commit**: YES — `feat(driver): implement HTTP acquisition driver`

- [ ] 14. 第三方 MQTT 接入驱动

  **What to do**:
  - 实现 `ThirdPartyMqttDriver` 结构体，适配 Driver trait
  - 支持多 Broker 并发连接（每个 Broker 独立 tokio task）
  - 动态主题订阅（配置中定义 topics）
  - Payload 解析（JSONPath/正则）→ 映射到统一 DataPoint
  - 使用 `rumqttc-next 0.33`（rustls）
  - 编写测试：模拟 MQTT Broker

  **Must NOT do**: 不要与北向 MQTT 转发混淆（这是南向接入）
  **Recommended Agent Profile**: Category: `unspecified-high` — 多 Broker 并发
  **Parallelization**: Wave 2 (with 9-13)

  **References**: rumqttc-next 0.33, JSONPath
  **Acceptance Criteria**: `[ ] cargo test --package daemon thirdparty_mqtt PASS [ ] 多 Broker 连接正常`
  **QA Scenarios**:
    - Happy: 订阅 broker1/topic1，收到 {"val":1}，断言 DataPoint 正确
    - Error: Broker 断开，断言自动重连
  **Evidence**: .omo/evidence/task-14-thirdparty-mqtt.log
  **Commit**: YES — `feat(driver): implement third-party MQTT ingress driver`

---

### Wave 2b — 数据处理 + 缓存（4 任务）

- [x] 15. 数据处理层（点位映射、单位换算、死区过滤、时间戳统一）

  **边界声明（与 20/37 互斥）**: 本任务只做**逐点、无规则的本地处理内核**——按点位配置直接执行映射/换算/死区/时间戳统一。**规则解析、规则匹配、动作编排属于任务 20/37**，本任务不引入任何规则 DSL。与 53 的边界：本任务对 `quality` 仅做**原值透传**，语义规范化归任务 53（避免 15↔53 循环依赖）。与 70 的边界：**公式求值（计算点）不属于本任务**（见任务 70）；但本任务须保证「**单位换算在公式之前完成**」——任务 70 依赖本任务把值换算到工程单位后再参与运算，故本任务的换算能力是 70 的前置。

  **What to do**:
  - 实现 `DataProcessor` 结构体
  - 点位映射（源点位 → 目标点位）
  - 单位换算（配置中定义系数，如 MPa→bar）
  - 死区过滤（变化小于阈值则不输出）
  - 时间戳统一（保留设备原始时间戳 + 打上采集时间戳）
  - 编写测试：验证映射、换算、死区逻辑

  **Must NOT do**: 不要在此实现规则引擎（规则解析/匹配/动作编排 = 任务 20/37）；不要在此定义数据类型解码与质量码语义（= 任务 53），`quality` 只做原值透传；**不要在此实现公式求值（= 任务 70）**，本任务只需保证换算先于公式完成
  **Recommended Agent Profile**: Category: `deep` — 数据处理核心
   **Parallelization**: Wave 2b (depends on 8, 9-14)

   **References**: DataPoint schema (任务 2)
  **Acceptance Criteria**: `[ ] cargo test --package daemon processing PASS [ ] 死区过滤生效 [ ] 单位换算正确`
  **QA Scenarios**:
    - Happy: 输入 1000 Pa → 映射 + 换算为 1 kPa → 断言输出正确
    - Error: 连续 3 次值 36.49/36.50/36.51 死区阈值 0.1 → 断言只有 1 次输出
  **Evidence**: .omo/evidence/task-15-processing.log
  **Commit**: YES — `feat(processing): implement data processor`

- [x] 16. 组轮询调度器（Neuron group_timer 模式）

  **What to do**:
  - 实现 `GroupScheduler`，按组（group）独立轮询
  - 每组独立频率（配置中定义，如 100ms/1s/10s）
  - 使用 tokio tokio::time interval + 独立 task
  - 组内标签批量读取（减少连接开销）
  - 编写测试：验证不同频率组的独立调度

  **Must NOT do**: 不要实现动态频率调整（那是高级功能）
  **Recommended Agent Profile**: Category: `deep` — 调度器设计
   **Parallelization**: Wave 2b (depends on 8, 15)

   **References**: Neuron group_timer 模式, tokio::time
  **Acceptance Criteria**: `[ ] cargo test --package daemon scheduler PASS [ ] 组独立频率生效`
  **QA Scenarios**:
    - Happy: 组A(100ms)和组B(1s)同时运行，断言 A 采集 10 次期间 B 采集 1 次
    - Error: 某组驱动超时，断言不影响其他组调度
  **Evidence**: .omo/evidence/task-16-scheduler.log
  **Commit**: YES — `feat(scheduler): implement group-based polling scheduler`

- [x] 17. 断网续传 — SQLite 持久化队列（独立库文件 + rusqlite 单写 + WAL + 环形覆盖）

  **存储职责与分库（与 18 的边界）**: 本任务使用**独立的队列库文件** `queue.db`，与任务 18 的遥测库 `telemetry.db` **物理分离、各自持有独立写连接**。SQLite 只允许单写者，共用一个连接会互相阻塞；分库后队列写入不受遥测写入影响。`queue.db` 及其 `-wal`/`-shm` 派生文件一并纳入任务 18 的加密范围。

  **What to do**:
  - 实现 `OfflineQueue` 结构体
  - 使用 `rusqlite 0.40.1`（bundled SQLite 3.53.2，WAL 模式），库文件 `queue.db`（独立于 `telemetry.db`）
  - **背压**：内存队列设水位（默认 10 万条 / 256MB）——达高水位即强制落盘；达硬上限时按策略丢弃最旧数据并写审计（**不得静默丢弃**）
  - 采集数据先写入内存队列，网络正常时异步上传
  - 上传失败 → 转存 SQLite 磁盘队列
  - 环形覆盖策略（超过 10GB/7 天自动淘汰旧数据）
  - 单写连接 + 应用层写队列（`tokio::sync::mpsc`）+ 批量事务
  - **幂等**：每批消息带单调递增 `batch_seq` + `gateway_id` 组成幂等键；补发前读已确认位点（high-water mark）续传，杜绝重复与空洞（服务端侧幂等键校验见任务 54）
  - **顺序保证**：同一设备内按 `batch_seq` 严格有序补发
  - 编写测试：断网 → 写入 → 恢复 → 校验补发完整性、顺序与无重复

  **Must NOT do**: 不要用 sqlx 异步（SQLite 单写者坑）；**不要与任务 18 的 `telemetry.db` 共用库文件或写连接**
  **Recommended Agent Profile**: Category: `deep` — 持久化队列
   **Parallelization**: Wave 2b (depends on 8, 9-14)

   **References**: rusqlite 0.40.1, SQLite 单写者坑 Reddit PSA
   **Acceptance Criteria**: `[ ] cargo test --package daemon offline_queue PASS [ ] 断网写入不丢数据 [ ] 恢复后补发完整且无重复 [ ] 水位上限触发落盘`
   **QA Scenarios**:
    - Happy: 断网时写入 100 条 → 恢复 → 断言 100 条全部按序补发且无重复
    - Error: 超过 7 天/10GB → 断言旧数据被环形覆盖淘汰
    - Error: 内存队列触达硬上限 → 断言丢弃最旧 + 产生审计记录（不静默丢弃）
    - Error: 补发中途进程重启 → 断言按 high-water mark 续传，无重复无空洞
  **Evidence**: .omo/evidence/task-17-offline-queue.log
  **Commit**: YES — `feat(cache): implement offline queue with SQLite WAL`

- [ ] 18. 本地存储（独立库文件 + SQLite WAL + SQLCipher 透明加密）

  **存储职责与分库（与 17 的边界）**: 本任务使用**独立的遥测库文件** `telemetry.db`，与任务 17 的 `queue.db` **物理分离、各自持有独立写连接**，避免 SQLite 单写者约束下互相阻塞；两者密钥同为机器码 HKDF 派生，同属加密范围。

  **What to do**:
  - 实现 `LocalStorage` 结构体
  - 使用 `rusqlite` + `libsqlcipher-sys`（SQLCipher 透明加密），库文件 `telemetry.db`
  - 密钥由机器码通过 HKDF 派生（拷盘即失效）
  - 创建 telemetry 表（device_id, point_id, value, unit, ts, quality）
  - **schema 版本位**：建表时写入 `user_version` 并预留 `schema_migrations` 版本表；迁移框架本体在任务 55 实现，本任务只预留版本位与迁移入口，不做迁移逻辑
  - 实现按时间范围查询 + 按设备查询
  - 编写测试：加密存储 + 换机器码密钥失效

  **Must NOT do**: 不要用 DuckDB（那是 P2）；**不要与任务 17 的 `queue.db` 共用库文件或写连接**
  **Recommended Agent Profile**: Category: `deep` — 加密存储
   **Parallelization**: Wave 2b (depends on 3, 17)

   **References**: libsqlcipher-sys, HKDF crate, SQLCipher
   **Acceptance Criteria**: `[ ] cargo test --package daemon storage PASS [ ] 换机器码密钥解密失败 [ ] telemetry.db 与 queue.db 为独立文件`
   **QA Scenarios**:
    - Happy: 写入数据 → 关闭 → 用正确密钥重开 → 断言数据可读
    - Error: 用错误机器码派生的密钥打开 → 断言解密失败
    - Error: 检查磁盘产物 → 断言存在两个独立库文件且各自有 -wal，无共享连接
  **Evidence**: .omo/evidence/task-18-local-storage.log
  **Commit**: YES — `feat(storage): implement SQLCipher encrypted local storage`

- [x] 19. MQTT 客户端（rumqttc-next 0.33，rustls，QoS 0/1/2）

  **What to do**:
  - 实现 `MqttClient` 结构体
  - 使用 `rumqttc-next 0.33.x`（纯 Rust，rustls）
  - 支持 QoS 0/1/2，TLS/mTLS
  - 连接池（多 Broker 支持）
  - **每路连接可声明 `encoding`（`protobuf` 默认 / `json`）**，编码器由任务 62 提供；本任务只做**配置读取与传递**，不实现序列化细节
  - 实现自动重连 + 会话恢复
  - 编写测试：连接模拟 MQTT Broker，发布/订阅验证

  **Must NOT do**: 不要用 paho-mqtt（引入 C 依赖）；**不要在本任务内实现 JSON 编码逻辑**（归任务 62）
  **Recommended Agent Profile**: Category: `deep` — MQTT 客户端
  **Parallelization**: Wave 3 (with 20-26, 62, depends on 15, 17)

  **References**: rumqttc-next 0.33, rustls, 任务 62（每路出口编码）
  **Acceptance Criteria**: `[ ] cargo test --package daemon mqtt PASS [ ] QoS 1 确认 [ ] 每路连接可独立声明 encoding`
  **QA Scenarios**:
    - Happy: 发布消息 QoS 1 → 断言收到 PUBACK
    - Error: Broker 断开 → 断言自动重连 + 会话恢复
  **Evidence**: .omo/evidence/task-19-mqtt-client.log
  **Commit**: YES — `feat(mqtt): implement MQTT client with rustls`

- [x] 20. MQTT 转发规则引擎（声明式 JSON 规则）

  **边界声明（与 15/37 互斥）**: 本任务交付**规则引擎框架**——规则 JSON schema、解析器、SELECT/WHERE/DO 求值、Topic 路由与 JSONPath 字段重映射，**动作集限于 P0 子集（过滤 + 路由 + 字段重映射）**。`点位映射 / 单位换算 / 死区过滤` **一律复用任务 15 的 `DataProcessor`，不得在本任务重复实现**；完整动作集、规则版本管理、规则间组合与循环依赖检测归任务 37。

  **What to do**:
  - 实现 `RuleEngine` 结构体
  - 解析 JSON 规则：点位映射、单位换算、死区过滤、Topic 路由
  - 支持 SELECT/WHERE/DO 语义（参考 EMQX Rule SQL）
  - 支持 JSONPath 提取和重映射
  - 数值变换类动作委托任务 15 的 `DataProcessor`（不复制其逻辑）
  - 编写测试：验证规则匹配和转换

  **Must NOT do**: 不要直接签序列化字节（那是任务 21）；不要重复实现任务 15 的映射/换算/死区
  **Recommended Agent Profile**: Category: `deep` — 规则引擎
  **Parallelization**: Wave 3 (with 19, 21-26)

  **References**: EMQX Rule SQL 语义, jaq/evalexpr crate
  **Acceptance Criteria**: `[ ] cargo test --package daemon rules PASS [ ] 规则匹配正确`
  **QA Scenarios**:
    - Happy: 规则 "WHERE temp>30 → topic alarms" 输入 36.5 → 断言转发到 alarms
    - Error: 规则语法错误 → 断言返回 ConfigError 且不崩溃
  **Evidence**: .omo/evidence/task-20-rules.log
  **Commit**: YES — `feat(rules): implement declarative JSON rule engine`

- [x] 21. AuthBlock 签名模块（Ed25519，业务语义哈希）

  **What to do**:
  - 实现 `AuthSigner` 结构体
  - 对"业务语义确定性哈希"（TelemetryBatch 字段排序后哈希）签名
  - 使用 `ed25519-dalek` crate；**私钥不在本任务生成也不在本任务落盘**——通过任务 49 的密钥托管 `KeyProvider` trait 取 in-memory 私钥句柄
  - `mid` 统一取**任务 3 指纹模块**的输出（多源 + HMAC-SHA256），**本任务不自造 mid 哈希算法**
  - AuthBlock = { mid: <任务 3 指纹输出>, nonce: uuid, ts: now, sig: base64(ed25519_sign(payload_hash + mid + ts + nonce)) }
  - 签名前先校验本地授权状态（Token 有效且未降级）；授权无效则**不签名**（数据无法通过任务 48 的二次校验）
  - 编写测试：签名 + 验签 + 篡改后验签失败 + 授权失效时拒绝签名

  **Must NOT do**: 不要直接签 Protobuf 序列化字节（应签业务语义哈希）；不要在本任务生成/落盘私钥，也不要自造 mid 算法（统一用任务 3 + 任务 49 的产物）
  **Recommended Agent Profile**: Category: `deep` — 密码学核心
  **Parallelization**: Wave 3 (with 19-20, 22-26)

  **References**: ed25519-dalek, sha2, uuid
  **Acceptance Criteria**: `[ ] cargo test --package daemon auth PASS [ ] 篡改后验签失败`
  **QA Scenarios**:
    - Happy: 签名 → 验签 → 断言 PASS
    - Error: 篡改 payload 后验签 → 断言 FAIL（签名不匹配）
  **Evidence**: .omo/evidence/task-21-auth-sign.log
  **Commit**: YES — `feat(auth): implement Ed25519 AuthBlock signing`

- [ ] 22. 云授权客户端（激活请求 + Lease Token + 24h 心跳）

  **What to do**:
  - 实现 `LicensingClient` 结构体
  - **激活**：首次联网向任务 45 的云授权服务提交「激活码 + 机器码指纹（任务 3）+ 设备公钥（任务 49）」→ 换取 Lease Token；激活失败要区分错误码（码已废弃 / 已绑定他机 / 无此码 / 网络不可达）并给出可读提示
  - **心跳**：每 24h 携带 Token + 指纹上报，校验 Token 未过期、设备未超配额、**激活码状态仍为 bound**（被废弃则立即失效）
  - 超期未心跳 → 停止北向转发（保留本地采集）
  - Token 过期或**心跳被服务端拒绝（码已废弃/已换机）** → 立即停止签名（数据无法通过任务 48 的二次校验）并进入降级态
  - 心跳失败按指数退避重试；连续失败达阈值记审计告警
  - 编写测试：对任务 45 的真实授权服务做集成测试（本地起服务），失败分支用 stub 覆盖

  **Must NOT do**: 不要实现本地离线激活（用户已确认激活必须联网）；不要自建 mock 授权服务后止步于此（必须与任务 45 真实服务联调）
  **Recommended Agent Profile**: Category: `deep` — 云授权集成
  **Parallelization**: Wave 3 (with 19-21, 23-26)

  **References**: reqwest, JWT/HTTP 认证, 任务 45 licensing-server, 任务 44 API 契约
  **Acceptance Criteria**: `[ ] cargo test --package daemon licensing PASS [ ] 心跳超时停止转发 [ ] 激活码被废弃后心跳被拒并降级 [ ] 与任务 45 真实服务联调通过`
  **QA Scenarios**:
    - Happy: 用有效激活码激活 → 断言返回含公钥的有效 Lease Token → 24h 心跳通过
    - Error: 心跳超期 → 断言停止北向转发但本地采集继续
    - Error: 后台废弃该激活码后发起心跳 → 断言服务端拒绝 + 本地进入降级并停发
    - Error: 同一激活码在第二台机器激活 → 断言服务端拒绝并返回「已绑定他机」错误码
  **Evidence**: .omo/evidence/task-22-licensing.log
  **Commit**: YES — `feat(auth): implement cloud licensing client + heartbeat`

- [ ] 23. 试用期管理（本地加密标记 + 云端首次激活时间）

  **What to do**:
  - 实现 `TrialManager` 结构体
  - 首次启动记录云端激活时间
  - 本地加密存储试用标记（SQLCipher + 机器码派生密钥）
  - 3 天到期检查（云端时间 + 本地加密标记交叉校验）
  - 防篡改：防卸载重装、改系统时间
  - 编写测试：验证 3 天后停止北向转发

  **Must NOT do**: 不要用明文存储试用标记
  **Recommended Agent Profile**: Category: `deep` — 试用期逻辑
  **Parallelization**: Wave 3 (with 19-22, 24-26)

  **References**: Trial 逻辑, 加密存储
  **Acceptance Criteria**: `[ ] cargo test --package daemon trial PASS [ ] 3 天后停止转发`
  **QA Scenarios**:
    - Happy: 试用第 2 天 → 断言北向转发正常
    - Error: 试用第 4 天 → 断言停止北向转发，保留本地存储
  **Evidence**: .omo/evidence/task-23-trial.log
  **Commit**: YES — `feat(auth): implement 3-day trial manager`

- [ ] 24. 免费基础版限制逻辑（8 设备 / ≥1s / 无北向转发 / 无 OTA）

  **What to do**:
  - 实现 `TierManager` 结构体
  - 免费版：仅 Modbus TCP/RTU，最多 8 设备，频率 ≥1s，无北向转发（本地存储保留），**无 OTA 升级**
  - 专业版：全协议 + 多路第三方 MQTT + 北向转发 + OTA
  - License 文件中携带 tier 信息（云端签发）
  - 提供**统一的 tier 门禁查询接口**，供任务 35（OTA）、任务 28（设备数上限）等在入口统一调用
  - 编写测试：验证设备数限制、协议限制与 OTA 门禁

  **Must NOT do**: 不要在免费版实现北向转发；不要把 tier 判定散落在各模块（统一走 `TierManager` 门禁接口）
  **Recommended Agent Profile**: Category: `quick` — 限制逻辑
  **Parallelization**: Wave 3 (with 19-23, 25-26)

  **References**: License tier 结构；第四轮访谈（免费基础版：Modbus 8 设备 / ≥1s / 无北向转发 / **无 OTA**）
  **Acceptance Criteria**: `[ ] cargo test --package daemon tier PASS [ ] 9 设备被拒绝 [ ] 免费版 OTA 被拒`
  **QA Scenarios**:
    - Happy: 免费版 8 设备 → 断言正常
    - Error: 免费版第 9 设备 → 断言拒绝连接
    - Error: 免费版触发 OTA → 断言被门禁拒绝并提示升级
  **Evidence**: .omo/evidence/task-24-tier.log
  **Commit**: YES — `feat(auth): implement free tier restrictions`

- [ ] 25. TLS/mTLS 配置 + MQTT 连接安全

  **What to do**:
  - 实现 `TlsConfig` 结构体
  - MQTT 支持 TLS（rustls）+ mTLS（客户端证书）
  - Web 管理界面 HTTPS（自签名证书支持）
  - 证书路径/密码配置
  - 编写测试：验证 TLS 连接和 mTLS 握手

  **Must NOT do**: 不要忽略证书验证错误
  **Recommended Agent Profile**: Category: `quick` — TLS 配置
  **Parallelization**: Wave 3 (with 19, 26)

  **References**: rustls, rcgen（自签名证书）
  **Acceptance Criteria**: `[ ] cargo test --package daemon tls PASS [ ] mTLS 握手成功`
  **QA Scenarios**:
    - Happy: mTLS 连接 → 断言握手成功
    - Error: 错误证书 → 断言连接拒绝
  **Evidence**: .omo/evidence/task-25-tls.log
  **Commit**: YES — `feat(security): implement TLS/mTLS configuration`

- [ ] 26. 安全审计模块（登录/配置修改/授权失败日志）

  **What to do**:
  - 实现 `AuditLogger` 结构体
  - 记录关键事件：登录、配置修改、授权失败、试用期到期
  - 审计日志不可篡改（追加写入 + 签名）
  - 支持远程拉取审计日志
  - 编写测试：验证日志记录和不可篡改

  **Must NOT do**: 不要实现审计日志前端（那是任务 30）
  **Recommended Agent Profile**: Category: `quick` — 审计日志
  **Parallelization**: Wave 3 (with 19-25, 62)

  **References**: tracing/appender, 日志签名
  **Acceptance Criteria**: `[ ] cargo test --package daemon audit PASS [ ] 日志追加不可篡改`
  **QA Scenarios**:
    - Happy: 配置修改 → 断言审计日志记录
    - Error: 尝试删除日志 → 断言失败（追加写入）
  **Evidence**: .omo/evidence/task-26-audit.log
  **Commit**: YES — `feat(security): implement audit logging`

- [x] 62. 北向双编码器（每路出口 protobuf / json + 大整数精度 + MQTT5 编码声明）

  **What to do**:
  > **定案（本轮）**：北向编码是**每路出口独立可选的一等配置项**，不是全局调试开关。默认 `protobuf`。
  - 实现 `Encoder` 抽象（`traits::Encode`）：`ProtobufEncoder`（默认）与 `JsonEncoder`；由任务 19 的 MQTT 客户端按**每路连接配置**选择
  - 配置形态（每路北向连接独立声明）：
    ```
    [[northbound]]
    name = "customer-broker"
    encoding = "protobuf"   # protobuf | json
    ```
    非法取值在**配置加载阶段即报错**，不静默回退
  - **大整数精度（硬要求）**：超出 `2^53 − 1` 的整数**必须序列化为字符串**（`"ts_ns": "1727078400123456789"`）。覆盖：纳秒/微秒时间戳、大 `uint64` 计数器、累计电量等；阈值判定用统一常量；**不得**依赖 JSON number 承载
  - **浮点特殊值**：`NaN` / `±Infinity` 在 JSON 中非法 → 约定编码为 `null` 并附带 quality 标记（与任务 53 质量码对齐）
  - **bytes / blob**：JSON 下 base64 编码并加类型前缀标识，确保类型信息不丢失
  - **编码与签名解耦（硬要求）**：AuthBlock 签的是**业务语义确定性哈希**（任务 21），与编码无关 → 实现后必须证明**同一份数据两种编码语义一致、验签结果相同**
  - **MQTT 5 编码声明**：设置 `Payload Format Indicator`（JSON = UTF-8，Protobuf = 二进制）与 `Content Type`（`application/json` / `application/x-protobuf`）；若连接降级到 MQTT 3.1.1（无属性），则在 payload 外层带 `enc` 字段或由 topic 后缀区分，并在接入指南写明
  - **JSON 结构约定**：字段名与 Protobuf 保持可映射（snake_case），数字/字符串类型规则稳定，避免下游解析二义
  - 编写测试：双编码 round-trip 语义一致；大整数不丢精度；NaN/Inf 与 bytes 编码；双编码验签结果一致；非法 encoding 配置报错
  - 为任务 39 提供 JSON **性能基线**数据点（体积、序列化耗时、200 设备下的 CPU 与延迟）

  **Must NOT do**:
  - **不得**把超出 `2^53 − 1` 的整数用 JSON number 编码
  - **不得**让 JSON 路径与 Protobuf 路径产生不同语义或不同验签结果
  - 不得把 JSON 实现成「仅调试、生产不承诺」的半成品（本轮起 JSON 是**生产可选编码**）
  - 不得在编码层做单位换算 / 死区过滤（那是任务 15 / 37）

  **Recommended Agent Profile**: Category: `deep` — 序列化与跨端一致性
  **Parallelization**: Wave 3（与 19-26 并行）；**Blocked By**: 2（数据模型）、15（处理后数据）、21（签名口径）；**Blocks**: 39、58

  **References**: serde_json（大数精度处理需显式策略）、prost、MQTT 5 规范（Payload Format Indicator / Content Type）；任务 21（签名口径）；任务 53（类型与质量码）；任务 58（接入指南）
  **Acceptance Criteria**: `[ ] 每路连接可独立选 protobuf/json，默认 protobuf [ ] 超 2^53−1 整数以字符串输出且无精度丢失 [ ] NaN/Inf 与 bytes 有明确编码约定 [ ] 双编码语义一致且验签结果相同 [ ] MQTT5 属性正确声明 [ ] JSON 性能基线已产出`
  **QA Scenarios**:
    - Happy: 同一份 TelemetryBatch 分别经 protobuf / json 出口 → 断言字段语义一致、验签均通过
    - Error: 构造 `ts_ns = 9223372036854775807` → 断言 JSON 中以字符串输出且值完整（与 Protobuf 解码结果逐字节相等）
    - Error: 构造大量 `uint64` 计数器 → 断言无精度丢失（逐条比对）
    - Error: 构造 `f64::NAN` / `INFINITY` → 断言按约定编码（null + quality 标记），不输出非法 JSON
    - Error: 配置 `encoding = "yaml"` → 断言配置加载阶段报错，不静默回退
    - Error: 人为让 JSON 路径字段顺序/类型与 Protobuf 不一致 → 断言一致性测试失败并暴露差异
  **Evidence**: `.omo/evidence/task-62-dual-encoding.log`
  **Commit**: YES — `feat(northbound): per-connection protobuf/json dual encoding`

- [ ] 27. Vue 界面骨架（Vite + TypeScript + **Arco Design Vue + `ui-kit/` 共享包**）

  **What to do**:
  - 初始化 `ui-kit/`（两端共用的前端基础包）：从任务 63 的设计 token 落地为 CSS 变量与主题配置；实现共享业务组件（`StatusTag` / `MachineCodeDisplay` / `CodeLifecycleTimeline` / `DeviceStateDot` / `QueueGauge` / `QualityBadge` / `EncodingRadio` / `DangerConfirmModal` / `PointTableEditor` / `LogViewer` / `DiagPanel` / `RoleGate` / `StatCard`）
  - 初始化 web-console（Vite + TypeScript + Vue 3 + **Arco Design Vue**，按任务 63 设计稿；**不使用 Element Plus**）
  - 创建项目结构：pages/components/api/store/router；**按任务 63 的页面清单预置路由与空页面**（总览 / 实时监控 / 告警中心 / 设备接入 / 点位与映射 / 北向转发 / 转发规则 / 授权与激活 / 日志与审计 / 系统设置 / 账号与角色 / 诊断与自检 / 备份与恢复 + 初始化向导）
  - 配置 Vite proxy 到后端（headless HTTP server / Tauri command）；接入 `ui-kit/` 主题变量，确保与原型视觉一致
  - 编写 vitest 配置 + 示例测试（含 `ui-kit` 组件测试）
  - 编写测试：页面加载 + 路由跳转 + 主题 token 生效

  **Must NOT do**: 不要做具体业务页面（那是任务 28-30、65-68）；不要引入 Element Plus 或第二套组件库；不要在 `web-console` 内复制 token（须引用 `ui-kit`）

  **Recommended Agent Profile**: Category: `quick` — 前端脚手架 + 共用包
  **Parallelization**: Wave 4 (with 28-31)；**Blocked By**: 63（设计系统与组件清单）；**Blocks**: 28/29/30/31、47、65、67、68

  **References**: 任务 63 `ui-design-system.md`（token 与组件清单）；任务 63 原型；Vite, Vue 3, Arco Design Vue, vitest
  **Acceptance Criteria**: `[ ] npx vitest run PASS [ ] 页面加载成功 [ ] ui-kit 主题 token 生效（关键组件字号/间距/圆角与设计稿计算值一致） [ ] 14 个页面路由已预置`
  **QA Scenarios**:
    - Happy: `npm run dev` → 浏览器打开 → 断言 Vue 渲染 + 侧边导航 14 项
    - Error: 路由跳转未知路径 → 断言显示 404 页面
    - Error: 检查 web-console 依赖 → 断言无 Element Plus，且 `ui-kit` 被引用
  **Evidence**: .omo/evidence/task-27-vue-skeleton.log
  **Commit**: YES — `feat(ui): init Vue 3 + Vite console with shared ui-kit`

- [ ] 28. 配置管理页面（设备/点位/协议/规则）

  **What to do**:
  - 实现设备管理页：新增/编辑设备（**协议类型选择后动态切换参数表单**：Modbus TCP/RTU、OPC UA、S7、MC、HTTP、第三方 MQTT）+ **「测试连接」动作** + 串口扫描（COM / ttyUSB 及占用状态）
  - 实现点位表配置：数据源点位 → 目标点位映射（**点位类型 `physical` / `derived`**、数据类型、字节序、单位、死区、质量码）+ **「导入点表」入口与导出**（导入校验逻辑见任务 65）
  - 点位表须**区分物理点与计算点**：类型列 + Tag 视觉区分（避免运维把计算点误认为现场点位）；**计算点行的「地址 / 字节序」列为空**，公式列显示表达式摘要（超长截断 + 悬浮全文）；**「新增 / 编辑公式」入口由任务 71 实现**，本任务只提供入口与列表呈现
  - 实现协议参数配置（MQTT broker、串口号等）
  - 实现规则配置界面（JSON 规则编辑器 + 组合与循环检测提示）
  - **空态必须给出下一步动作**（如点位表为空 → 「导入点表」+「手工新增」双入口）；错误态给「发生了什么 + 怎么解决」
  - 调用后端 API（tauri command / HTTP）
  - 编写测试：验证配置提交和回显、协议切换、空态与错误态

  **Must NOT do**: 不要做实时数值展示（那是任务 29）；不要自行发明页面结构（按任务 63 设计稿）；不要让空态只显示「暂无数据」而无动作
  **Recommended Agent Profile**: Category: `unspecified-high` — 表单交互
  **Parallelization**: Wave 4 (with 27, 29-30)

  **References**: Arco Design Vue 表单与表格, API 调用, 任务 63 线框（设备/点位/协议/规则 + 空态与错误态）
  **Acceptance Criteria**: `[ ] vitest PASS [ ] 配置提交成功 [ ] 回显正确`
  **QA Scenarios**:
    - Happy: 添加 Modbus 设备 → 断言列表中出现新设备
    - Error: 必填字段为空 → 断言表单校验拦截
  **Evidence**: .omo/evidence/task-28-config-mgmt.log
  **Commit**: YES — `feat(ui): implement configuration management page`

- [ ] 29. 实时监控面板（数值 + 连接状态 + 采集频率）

  **What to do**:
  - 实现实时数值面板（WebSocket，**渲染节流 1s**——200 设备 × 100ms 场景下禁止逐条直写 DOM）
  - 设备连接状态指示器（在线/离线/采集失败，**必须带文案不能只有圆点**）
  - 采集频率统计图表、数据质量指示（quality code，枚举与任务 53 对齐）
  - 陈旧数据（>1s 未更新）整行转灰；`quality != Good` 行左侧标记
  - WS 断连时顶部显示 `--warn` 细条（含最后更新时间 + 重试），不静默停更
  - 编写测试：数据刷新、状态显示、节流生效、断连提示

  **Must NOT do**: 不要做历史报表（那是 P2 本地报表）；不要无节流直推 DOM；不要自造 quality 枚举文案
  **Recommended Agent Profile**: Category: `visual-engineering` — 数据可视化
  **Parallelization**: Wave 4 (with 27, 28, 30)；**Blocked By**: 27（ui-kit）、63（设计稿）、52（WS 通道）

  **References**: 任务 63 线框与 §4.2 实时数据约定；ECharts/Chart.js；任务 52（WS 推送）、53（quality 枚举）
  **Acceptance Criteria**: `[ ] vitest PASS [ ] 实时数值刷新 <1s [ ] 200 设备场景渲染节流生效（CPU 有基线记录） [ ] WS 断连有可见提示`
  **QA Scenarios**:
    - Happy: 模拟数据 → 断言面板数值实时更新
    - Error: 设备离线 → 断言状态指示器变红且带文案
    - Error: 200 设备 × 100ms 灌数据 → 断言渲染节流生效（对比未节流 CPU/帧率基线）
    - Error: 断开 WS → 断言出现断连提示且数值标记为陈旧
  **Evidence**: .omo/evidence/task-29-monitor.log
  **Commit**: YES — `feat(ui): implement real-time monitoring panel`

- [ ] 30. 日志与审计页面

  **What to do**:
  - 实现日志查看页：分级过滤（DEBUG/INFO/WARN/ERROR）、实时滚动
  - 实现审计日志页：登录/配置修改/授权失败记录
  - 支持远程日志拉取
  - 编写测试：验证日志过滤和远程拉取

  **Must NOT do**: 不要实现日志配置（那是任务 5 的后端）
  **Recommended Agent Profile**: Category: `unspecified-high` — 日志展示
  **Parallelization**: Wave 4 (with 27-29)

  **References**: 日志 API, 表格组件
  **Acceptance Criteria**: `[ ] vitest PASS [ ] 日志过滤生效`
  **QA Scenarios**:
    - Happy: 选择 WARN 级别 → 断言只显示 WARN/ERROR
    - Error: 远程拉取超时 → 断言显示错误提示
  **Evidence**: .omo/evidence/task-30-logs.log
  **Commit**: YES — `feat(ui): implement log and audit page`

- [ ] 31. Web 鉴权（JWT + HTTPS 自签名证书）

  **What to do**:
  - 实现登录页（账号密码）
  - JWT token 管理（存储 + 自动刷新）
  - HTTPS 支持（自签名证书，用户可配置）
  - 权限路由（未登录跳转到登录页）
  - 编写测试：验证登录、鉴权、路由守卫

  **Must NOT do**: 不要在 Tauri 外部暴露敏感操作
  **Recommended Agent Profile**: Category: `quick` — 鉴权
  **Parallelization**: Wave 4 (with 27, 32-33)

  **References**: JWT, rcgen, Vue Router navigation guards
  **Acceptance Criteria**: `[ ] vitest PASS [ ] 未登录访问管理页被拦截`
  **QA Scenarios**:
    - Happy: 登录 → 断言跳转到仪表盘
    - Error: 未登录直接访问 /config → 断言跳转到登录页
  **Evidence**: .omo/evidence/task-31-auth.log
  **Commit**: YES — `feat(ui): implement JWT authentication`

- [ ] 32. Tauri shell（Windows 桌面，含 WebView 管理界面）

  **What to do**:
  - 创建 tauri-shell/（Tauri 2.11.x）
  - 集成 web-console 作为 WebView 前端
  - 入口仅负责**平台初始化**（窗口 / 托盘 / IPC 注册）；**daemon 组件装配与生命周期一律调用任务 51 的 `daemon::bootstrap::assemble()`**，不得在壳内自行装配组件
  - 实现 Tauri commands（桥接任务 52 的后端管理 API，而非直连内部结构体）
  - Windows NSIS + MSI 打包配置
  - 实现托盘图标（后台运行 + 快速配置）
  - **开机自启 + 无登录运行（Windows 服务/计划任务）**：与任务 57 对齐实现方式
  - 编写测试：Tauri app 启动 + 命令调用

  **Must NOT do**: 不要在 Tauri 前端实现核心逻辑（应在 Rust 侧）；不要绕过任务 51 自行装配 daemon；**不要把授权判定放在 WebView/JS 层**（授权判定必须在 Rust 侧，见任务 43 设计）
  **Recommended Agent Profile**: Category: `deep` — 桌面应用
  **Parallelization**: Wave 4 (with 31, 33)；**Blocked By**: 51（装配入口）

  **References**: Tauri 2.11.x, tauri-plugin-sql, tauri-action, 任务 51 bootstrap, 任务 52 管理 API
  **Acceptance Criteria**: `[ ] cargo tauri build PASS [ ] NSIS 安装包生成 [ ] 启动链路经 bootstrap 装配（无壳内自装配）`
  **QA Scenarios**:
    - Happy: 安装 NSIS → 启动 → 断言 WebView 渲染管理界面
    - Error: 未登录 → 断言跳转到登录页
    - Error: 检查启动链路 → 断言 daemon 由 `bootstrap::assemble()` 装配（而非壳内 new 出各组件）
  **Evidence**: .omo/evidence/task-32-tauri-shell.log
  **Commit**: YES — `feat(tauri): implement Windows desktop shell`

- [ ] 33. Linux headless 服务（含轻量 HTTP server 托管 Vue，同时作为容器镜像的运行体）

  **What to do**:
  - 创建 headless/（Linux 无头网关服务）
  - 轻量 HTTP server（axum）托管 Vue 前端 **+ 承载任务 52 的管理 API 与实时通道**
  - 入口与任务 32 一致：**调用任务 51 的 `daemon::bootstrap::assemble()`**，不自建装配逻辑
  - 后台 daemon 模式（systemd service 配置 + 开机自启）
  - 命令行参数（-c config.toml, --daemon）
  - 支持远程浏览器访问管理界面
  - **容器友好要求（按任务 59 设计，为任务 60 提供可运行体）**：
    - 所有可写路径（数据/日志/配置/授权/试用）**可经环境变量或启动参数覆盖**，默认落 `/var/lib/iot-daq`、`/var/log/iot-daq` 等可挂载路径；**不向镜像可写层写任何业务或授权数据**
    - **PID 1 语义**：作为容器主进程时正确接收并处理 `SIGTERM`/`SIGINT` 触发优雅停机（不依赖 systemd），`docker stop` 不得丢数据
    - 监听地址可由环境变量控制（容器内须 `0.0.0.0` 以便宿主端口映射访问）
    - 机器码锚点读取路径可由环境变量指定（容器内指向只读挂载的宿主锚点路径，见任务 59）
    - 健康检查端点（供 `HEALTHCHECK` / compose healthcheck 使用）
  - 编写测试：HTTP 服务启动 + API 调用 + 容器内启动冒烟

  **Must NOT do**: 不要做桌面 GUI（Linux 是无头服务）；不要绕过任务 51 自行装配 daemon；**不要把数据写到镜像内固定路径**（否则容器重建即丢数据、且试用标记可被重置）
  **Recommended Agent Profile**: Category: `deep` — Linux 服务
  **Parallelization**: Wave 4 (with 31, 32)；**Blocked By**: 51（装配入口）

  **References**: axum, systemd, 任务 51 bootstrap, 任务 52 管理 API, 任务 59 容器化设计
  **Acceptance Criteria**: `[ ] cargo build --package headless PASS [ ] HTTP 服务启动 [ ] systemd 单元可开机自启 [ ] SIGTERM 触发优雅停机 [ ] 可写路径可覆盖且不写镜像层`
  **QA Scenarios**:
    - Happy: 启动 headless → curl localhost:8080 → 断言返回 HTML
    - Error: config 不存在 → 断言启动失败并显示错误
    - Error: systemctl restart → 断言优雅停机（队列 flush 完成，无数据丢失）
    - Error: `docker stop`（发 SIGTERM）→ 断言容器内进程优雅停机、队列 flush 完成、容器退出码 0
    - Error: 将数据目录指向只读挂载 → 断言启动时给出明确报错，而非静默写入镜像层
  **Evidence**: .omo/evidence/task-33-headless.log
  **Commit**: YES — `feat(headless): implement Linux daemon with HTTP server`

- [ ] 34. 敏感配置加密（SQLCipher + HKDF 机器码派生密钥）

  **What to do**:
  - 实现 `ConfigEncryptor` 结构体
  - 敏感配置（PLC 密码、MQTT 密码）通过 SQLCipher 加密存储
  - 密钥 = HKDF(machine_code, salt)
  - 拷盘到另一台机器 → 密钥不同 → 解密失败
  - 编写测试：验证加密存储和跨机器失效

  **Must NOT do**: 不要用明文存储任何密码
  **Recommended Agent Profile**: Category: `deep` — 加密核心
  **Parallelization**: Wave 4 (with 3, 18)

  **References**: HKDF, SQLCipher, libsqlcipher-sys
  **Acceptance Criteria**: `[ ] cargo test --package daemon encrypt PASS [ ] 跨机器解密失败`
  **QA Scenarios**:
    - Happy: 加密存储密码 → 用正确密钥解密 → 断言值正确
    - Error: 拷到另一机器 → 用错误密钥解密 → 断言失败
  **Evidence**: .omo/evidence/task-34-encrypt.log
  **Commit**: YES — `feat(security): implement SQLCipher + HKDF encryption`

---

- [ ] 65. 点位表批量导入/导出与校验（CSV/XLSX + 行号级错误 + 导出导入闭环）

  **What to do**:
  - 后端：`POST /api/points/import`（multipart 上传 → 解析 → 校验 → 预览 → 提交）、`GET /api/points/export`、`GET /api/points/template`
  - 支持 CSV 与 XLSX（UTF-8，含 BOM 容错）；**导出文件与导入模板字段顺序必须一致**，保证「导出 → 修改 → 导入」闭环
  - 校验规则（**两阶段**）：
    - **阶段一 · 逐行校验**：数据类型枚举（按驱动能力过滤，如 S7 不支持 float64）、字节序枚举（AB CD / CD AB / BA DC / DC BA）、地址格式与范围、目标点位命名冲突、单位为空（警告）、重复地址（警告）；**计算点行**额外校验：`point_type` 枚举（physical/derived）、公式非空、公式语法、引用点位是否存在（物理点或已定义的计算点）、引用的计算点必须已在同一文件中定义或已存在
    - **阶段二 · 整表校验（关键）**：把全部 `derived` 点位的公式引用关系合成依赖图，做**整表级环检测与拓扑排序**；**逐行校验无法发现跨行成环**（A、B 两行各自合法，合起来即 `R_A=[R_B]+1` / `R_B=[R_A]+1` 成环），必须整表检测，报错给出**环路径**
  - **公式字段的表格转义**：公式含逗号 / 引号 / 括号 / 换行（如 `if([T]>80,1,0)`）→ CSV 必须用引号包裹且内部引号双写转义；XLSX 按单元格文本原样存取；导出与导入必须互为逆运算（含公式含特殊字符的往返用例）
  - 导出列须包含 `point_type`（物理/derived）与 `formula`；导入时**公式列整表校验通过后才允许提交**
  - 校验结果结构：`{row, level(error/warn), field, value, reason, allowed}`；错误行**绝不静默丢弃**；整表级错误（环）额外携带 `cycle_path`
  - 冲突处理：同地址点位「跳过并报告」为默认，「覆盖」需显式选择
  - 前端：按任务 63 线框实现 3 步向导（上传 → 校验 → 确认）+ 错误行定位 + 「仅导入有效行」
  - 编写测试：正常导入、含错误行、含警告行、字段顺序往返一致（导出→导入幂等）、超大点表（≥2000 行）性能

  **Must NOT do**: 不要静默丢弃错误行；不要只报「有 N 行错误」而不给行号与原因；不要改动导出字段顺序（会破坏闭环）；不要在导入时静默覆盖已有点位；**不要只做逐行校验**（公式依赖成环必须整表检测，报错须给环路径）；不要在含环或引用不存在的公式时仍提交导入
  **Recommended Agent Profile**: Category: `deep` — 数据导入导出与校验
  **Parallelization**: Wave 4（与 27-34 并行）；**Blocked By**: 15（点位模型与映射）、28（配置页）、53（类型与字节序枚举）、**70（公式语法与依赖规则，公式列校验口径以 70 为准）**；**Blocks**: 39, 69

  **References**: 任务 63 线框（3 步校验向导线框）；任务 53（数据类型与字节序枚举）；任务 28（点位表页面）；**任务 70（公式语法、白名单函数、环检测口径）**
  **Acceptance Criteria**: `[ ] 导入 CSV 与 XLSX 均成功 [ ] 错误行报告含行号/原因/允许值 [ ] 导出文件可直接作为导入模板（往返一致，含公式含逗号/引号的转义用例） [ ] 2000 行点表导入 < 5s [ ] 冲突处理按选择生效 [ ] 公式列可导入且整表环检测生效（含环文件被拒并给出环路径）`
  **QA Scenarios**:
    - Happy: 导入 128 行合法点表 → 断言全部落库且映射正确
    - Error: 第 47 行 `float64`、第 91 行非法字节序 → 断言报告行号 + 原因 + 允许值
    - Error: 导出 → 不做修改直接导入 → 断言 0 错误 0 警告（幂等）
    - Error: 同地址已存在点位 → 默认断言跳过并报告；选「覆盖」后断言仅该行被覆盖
    - Error: 上传 2000 行含 300 错误行 → 断言全部报告且不丢行
    - Happy: 导入含 12 个计算点的点表（公式含逗号与引号，如 `if([T]>80,1,0)`）→ 断言落库且公式原文无损（转义往返正确）
    - Error: 导入含跨行成环的公式（`R_A=[R_B]+1`、`R_B=[R_A]+1`，两行各自语法合法）→ 断言**整表校验拦下**并给出环路径 `R_A → R_B → R_A`（证明逐行校验不足以发现）
    - Error: 公式引用不存在的点位 → 断言报告行号 + 原因 + 允许值（可引用范围）
    - Error: 导出含计算点的设备 → 不做修改直接导入 → 断言 0 错误 0 警告（含公式列往返幂等）
  **Evidence**: `.omo/evidence/task-65-points-import-export.log` + 错误报告样本
  **Commit**: YES — `feat(points): bulk import/export with row-level validation`

- [ ] 67. 客户端授权状态页与换机申请（机器码 / 锚点 / 试用倒计时 / 激活 / 申请换机）

  **What to do**:
  - 按任务 63 线框实现客户端「授权与激活」页（**客户端唯一授权触点**）
  - **授权设备信息**：机器码（分段显示 + 一键复制）、锚点来源分解（只读，标注权重与签名状态）、设备名、部署形态、程序版本
  - **授权状态**：状态标签（试用中 / 已授权 / 已降级）、tier、校验档位（A/B/C）、租约有效期、上次与下次心跳、离线宽限、到期行为说明
  - **激活**：激活码输入 + 提交（联网校验）；`CODE_ALREADY_BOUND` / `CODE_REVOKED` / `TS_OUT_OF_WINDOW` 等错误码映射为**明确中文文案与下一步建议**
  - **换机申请**：自动带入当前机器码与新检测机器码 + 原因（≥10 字，必填）+ 联系人 → 提交后返回受理编号
  - **降级横幅**（三场景：试用到期 / 心跳超期 / 被后台废弃）：原因 + 恢复路径；顶栏常驻授权状态胶囊
  - 编写测试：机器码复制、激活成功/失败各错误码、换机申请提交、降级横幅在各场景下的文案与优先级

  **Must NOT do**:
  - **不得**提供废弃 / 解绑 / 重置试用 / 更换机器码任何入口（`/api/license/*` 不得出现 revoke / unbind / reset-trial）
  - 不得在前端实现任何授权判定（判定在 Rust 侧与云端，前端只展示结果）
  - 不得让激活码或机器码进入日志明文、URL 参数或前端持久化存储

  **Recommended Agent Profile**: Category: `unspecified-high` — 授权触点前端
  **Parallelization**: Wave 4（与 27-31、65、68 并行）；**Blocked By**: 22（授权客户端）、23（试用期管理）、27（骨架）、31（鉴权）、49（密钥托管）；**Blocks**: 39, 69

  **References**: 任务 63 线框与 §4.4 降级场景；任务 42（状态机）、22/23（授权客户端与试用）、45（服务端错误码）、49
  **Acceptance Criteria**:
  - [ ] 机器码与锚点来源可见、可复制；标注容器场景锚点取自宿主机
  - [ ] 激活成功 / 各类失败错误码均有明确中文文案与下一步建议
  - [ ] 三种降级场景横幅文案与优先级与设计稿一致
  - [ ] 换机申请可提交并返回受理编号
  - [ ] **可点元素扫描确认无任何解绑 / 重置试用入口**（真实浏览器断言）
  - [ ] 前端持久化与日志中无激活码 / 机器码明文
  **QA Scenarios**:
    - Happy: 输入有效激活码 → 断言状态变为已授权、顶栏胶囊更新
    - Error: 输入已被绑定的码 → 断言提示 `CODE_ALREADY_BOUND` 中文说明 + 引导提交换机申请
    - Error: 模拟租约超期 7 天 → 断言横幅出现且写明「北向转发已停用，本地采集继续」+ 恢复路径
    - Error: 模拟后台废弃本机激活码 → 断言横幅显示停用与受理编号引导
    - Error: 真实浏览器扫描可点元素 → 断言无「解绑 / 重置试用 / 更换机器码」命中
    - Error: 静态检查前端存储与日志 → 断言无激活码明文
  **Evidence**: `.omo/evidence/task-67-license-page.log` + 三场景截图
  **Commit**: YES — `feat(ui): client license status page & transfer request`

- [ ] 68. 诊断自检 / 备份恢复 / 告警中心页面

  **What to do**:
  - **诊断与自检页**：二进制段哈希自检结果（通过 / 失败 + 时间）、前端资源完整性清单校验、安装包签名校验、串口权限检查、磁盘与队列水位、时钟偏差与回拨检测、依赖与许可证清单；**导出诊断包**（脱敏，不含激活码 / 私钥）
  - **备份与恢复页**：配置备份（自动策略 + 立即备份）、备份内容说明（**不含密钥与激活码**）、校验和、导出/导入配置包、变更历史、**回退到上一份有效配置**（危险操作：二次确认 + 原因必填）、恢复后数据库完整性检查
  - **告警中心**：活跃告警列表（按严重度排序）+ 告警统计图 + 告警规则配置（阈值、离线判定、队列水位、租约异常）；提供「下一步动作」直达入口
  - 编写测试：诊断数据源正确性、诊断包导出脱敏、备份导出导入、回退流程、告警规则生效与解除

  **Must NOT do**: 不要把诊断包设计成包含激活码 / 私钥 / 完整机器码明文（须脱敏）；不要在回退配置时省略二次确认；不要把告警规则设计成「自动封禁设备/账号」（只告警与提示）

  **Recommended Agent Profile**: Category: `unspecified-high` — 运维页面
  **Parallelization**: Wave 4（与 27-31、65、67 并行）；**Blocked By**: 52（管理 API 与状态聚合）、55（迁移与回滚）、30（日志页范式）；**Blocks**: 39, 69

  **References**: 任务 63 §3.7 三页摘要；任务 50（自检与完整性）、55（配置回滚与安全模式）、56（可信时间）、54（队列水位）、52（告警推送）
  **Acceptance Criteria**: `[ ] 诊断页数据与后端一致且诊断包已脱敏 [ ] 备份导出可再导入（往返一致）[ ] 回退配置需二次确认 + 原因 [ ] 告警规则可配置并生效 [ ] vitest PASS`
  **QA Scenarios**:
    - Happy: 打开诊断页 → 断言自检状态、串口权限、时钟偏差、队列水位均有值且与后端一致
    - Error: 导出诊断包并扫描 → 断言不含激活码 / 私钥 / 完整机器码
    - Error: 导出配置包 → 清空配置 → 导入 → 断言配置恢复且数据库完整性检查通过
    - Error: 点「回退到上一份有效配置」不填原因 → 断言提交被拦截
    - Happy: 配置一条「队列水位 > 70%」告警 → 制造水位 → 断言告警出现并可处置
  **Evidence**: `.omo/evidence/task-68-ops-pages.log`
  **Commit**: YES — `feat(ui): diagnostics, backup/restore & alert center pages`

### Wave 5 — OTA + 高级 + 打包 + 容器化（6 任务）

> 其中 **60（Docker 镜像与离线分发）** 与 38（原生打包）并行；容器化交付的验收独立放在任务 61。

- [ ] 35. OTA 升级模块（固件/配置热更新）

  **What to do**:
  - 实现 `OtaManager` 结构体
  - 从云端下载固件/配置包（校验签名 + 版本）
  - A/B 分区更新（避免更新失败变砖）
  - 回滚机制（校验失败自动回滚）
  - 支持配置热更新（无需重启）
  - 编写测试：模拟 OTA 下载 + 校验 + 应用的完整流程

  **Must NOT do**: 不要实现固件编译（那是发布流程）
  **Recommended Agent Profile**: Category: `deep` — OTA 核心
  **Parallelization**: Wave 5 (with 36, 37)

  **References**: OTA 模式（第三轮访谈全选"OTA 升级"）, 签名校验
  **Acceptance Criteria**: `[ ] cargo test --package daemon ota PASS [ ] 校验失败回滚`
  **QA Scenarios**:
    - Happy: 下载固件 → 校验签名 → 应用 → 断言版本更新
    - Error: 校验失败 → 断言自动回滚到上一版本
  **Evidence**: .omo/evidence/task-35-ota.log
  **Commit**: YES — `feat(ota): implement OTA upgrade manager`

- [ ] 36. 远程运维（远程重启/启停采集/日志拉取）

  **What to do**:
  - 实现 `RemoteOps` 结构体
  - 远程重启网关（安全确认）
  - 远程启停采集任务
  - 远程日志拉取（按时间范围）
  - 操作需要鉴权 + 审计日志
  - 编写测试：验证远程命令执行和审计

  **Must NOT do**: 不要实现远程桌面（那是 SSH）
  **Recommended Agent Profile**: Category: `deep` — 远程运维
  **Parallelization**: Wave 5 (with 35, 37)

  **References**: REST API（第三轮访谈全选"远程运维"）, 审计日志
  **Acceptance Criteria**: `[ ] cargo test --package daemon remoteops PASS [ ] 未授权返回 403``
  **QA Scenarios**:
    - Happy: 远程重启 → 断言网关重启（模拟）
    - Error: 未授权请求 → 断言返回 403
  **Evidence**: .omo/evidence/task-36-remoteops.log
  **Commit**: YES — `feat(ops): implement remote operations`

- [ ] 37. 规则引擎 V1（补全动作集 + 规则版本管理 + 组合与循环检测）

  **边界声明（与 15/20 互斥）**: 任务 20 已交付规则引擎框架与 P0 动作子集；**本任务只做「补全」**——剩余动作类型、规则版本管理、规则间组合与循环依赖检测。`点位映射 / 单位换算 / 死区过滤` 仍**一律复用任务 15 的 `DataProcessor`**，本任务不得重复实现数值变换逻辑（只做动作编排与调用）。

  **What to do**:
  - 在任务 20 的引擎上补全 V1 动作集（不重写框架）
  - 数值变换动作委托任务 15 的 `DataProcessor`（不复制实现）
  - 声明式 JSON 规则格式的版本化：规则 schema 版本号 + 向后兼容策略
  - 支持规则版本管理与灰度切换
  - 规则组合与循环依赖检测（DAG 校验，禁止环）
  - 编写测试：端到端规则链

  **Must NOT do**: 不要实现复杂脚本引擎（那是 P2 插件化）；不要重复实现任务 15 的映射/换算/死区；不要改动任务 20 已定的规则 schema 语义（只做扩展）
  **Recommended Agent Profile**: Category: `deep` — 规则引擎完整版
  **Parallelization**: Wave 5 (with 35, 36)

  **References**: EMQX Rule SQL, 任务 20 框架
  **Acceptance Criteria**: `[ ] cargo test --package daemon rules PASS [ ] 规则链完整`
  **QA Scenarios**:
    - Happy: 规则链"映射+换算+死区" → 断言最终输出正确
    - Error: 规则循环依赖 → 断言返回 ConfigError
  **Evidence**: .omo/evidence/task-37-rules-v1.log
  **Commit**: YES — `feat(rules): complete rule engine V1`

- [ ] 38. 打包与 CI（NSIS/MSI Windows + AppImage/deb/rpm Linux 原生）

  **What to do**:
  - Windows：NSIS 向导式安装包 + MSI 企业分发
  - Linux：AppImage（免安装）+ .deb + .rpm（原生路径）
  - **Linux 容器镜像与离线包不在本任务范围**（由任务 60 负责）；本任务在 CI 中为任务 60 提供 ubuntu-latest runner、buildx 与 cosign 凭据配置
  - GitHub Actions 矩阵 CI（**windows-latest / ubuntu-latest 双平台**，不含 macos-latest）
  - tauri-action 自动构建 + 发布
  - **安装包签名**：接入代码签名证书，产出可校验签名的安装包（签名与完整性方案见任务 43 设计 / 任务 50 实现）
  - ARM Linux 构建（pguyot/arm-runner-action）
  - 编写测试：验证安装包结构、安装流程与签名有效性

  **Must NOT do**: 不要在非 Windows 主机上构建 MSI（WiX 仅限 Windows）；不要在本任务重复实现容器镜像构建（那是任务 60）
  **Recommended Agent Profile**: Category: `quick` — 打包 CI
  **Parallelization**: Wave 5 (with 35-37, 39, 60)

  **References**: Tauri 打包文档, NSIS, WiX, GitHub Actions matrix, 任务 60（容器镜像与离线包）
  **Acceptance Criteria**: `[ ] cargo tauri build Windows PASS [ ] Linux AppImage/deb/rpm PASS [ ] 安装包签名可校验`
  **QA Scenarios**:
    - Happy: Windows NSIS → 安装 → 断言应用启动
    - Error: 在 Linux 上构建 MSI → 断言失败（预期行为）
    - Error: 用 `signtool verify` / `osslsigncode verify` 校验安装包 → 断言签名有效
  **Evidence**: .omo/evidence/task-38-packaging.log
  **Commit**: YES — `feat(ci): implement cross-platform packaging`

- [ ] 39. 联调测试 + 压测（50 → 200 设备 × 100ms）

  **What to do**:
  - 搭建模拟环境：50 个模拟 Modbus/OPC UA 设备（含 S7/MC 模拟器）
  - 全链路压测：采集 → 处理 → 缓存 → MQTT 转发
  - 验证性能指标：50 设备 × 100ms；**另跑一轮 200 设备上限验收**，记录目标硬件规格与 CPU/内存/队列水位
  - 验证断网续传：断网 1 小时 → 恢复 → 补发，校验**条数、顺序、无重复、无空洞**
  - 验证授权：过期 Token → 拒收；激活码被废弃 → 心跳被拒并降级
  - 验证换机流程端到端：后台废弃 → 重发 → 新机激活
  - **容器形态复跑（任务 60 就绪后）**：在 Docker 容器内复跑一轮 50 设备 × 100ms 端到端，确认容器化未引入明显性能退化（对比原生基线，记录 CPU/内存/延迟差异）
  - **北向双编码性能基线（任务 62 就绪后）**：同一负载分别走 `protobuf` 与 `json` 出口，记录**报文体积、序列化耗时、200 设备下的 CPU 与延迟**，产出**JSON 模式下的推荐设备上限**
  - **B 档端到端（任务 48 就绪后）**：直连客户 Broker 转发 → 审计回执上报 → 心跳携带序号区间 → 人为制造跳空 → 断言云端告警；并验证**断网期间持续转发、恢复后回执补报无跳空**
  - **公式计算开销基线（任务 70 就绪后）**：在满负载（200 设备 × 100ms）基础上挂 **N 个计算点**（建议梯度 0 / 50 / 200），记录**周期内公式求值耗时、CPU 增量、是否拖累采集周期**；确认 **AST 已编译缓存**（不得每周期重新解析——这是本特性最易踩的性能坑）；给出**计算点数量推荐上限**与依赖链深度上限
  - 编写性能基准测试

  **Must NOT do**: 不要只做单元测试（需端到端）；**不得用自有 mock 授权服务替代任务 45 的真实服务**；**不得只测 protobuf 路径**（JSON 出口与 B 档审计回执必须同轮覆盖）；**不得在无计算点的场景下宣称性能达标**（公式求值开销必须单独测量并给出上限）
  **Recommended Agent Profile**: Category: `deep` — 联调压测
  **Parallelization**: Wave 5 (with 35-38, 60)

  **References**: tokio-modbus 模拟服务器, rumqttc 测试, 任务 45 真实授权服务, 任务 60（容器形态）, 任务 48（B 档审计回执）, 任务 62（双编码）, **任务 70（计算点与公式求值开销）**
  **Acceptance Criteria**: `[ ] 50设备×100ms 全部采集 [ ] 200 设备完成一轮验收 [ ] 断网续传完整且无重复 [ ] 过期Token拒收 [ ] 换机废弃/重发端到端通过 [ ] 容器形态复跑无显著性能退化 [ ] 双编码性能基线（含 JSON 推荐上限）已产出 [ ] B 档回执与跳空告警端到端通过 [ ] 公式计算开销基线（含计算点上限与 AST 缓存验证）已产出`
  **QA Scenarios**:
    - Happy: 50 设备 × 100ms × 10s → 断言无丢数据
    - Happy: 200 设备 × 100ms × 10s → 断言无丢数据，且记录资源水位
    - Error: 断网 1h → 恢复 → 断言补发条数 = 50 设备 × (1000ms / 100ms) 条/秒 × 3600 秒 = **1,800,000 条**，且按序、无重复、无空洞
      （⚠️ 注意算式：单设备 10 条/秒 → 50 设备 500 条/秒 → 1h = 1.8e6；**不是 36,000**，36,000 是单设备的量）
    - Happy（双编码）: 同一负载分别走 protobuf 与 json 出口 → 断言两边字段语义一致、条数一致，并产出体积/CPU/延迟对比基线
    - Error（B 档）: 断网 1h 恢复 → 断言期间持续转发、回执补报区间连续无跳空；再人为丢弃 3 个区间的回执 → 断言云端告警触发
    - Happy（公式）: 200 设备 × 100ms 基础上挂 200 个计算点 → 断言采集周期未被拖累（对比 0 计算点基线），并断言 AST 仅编译一次（重复周期内无重复解析）
  **Evidence**: .omo/evidence/task-39-stress-test.log
  **Commit**: YES — `feat(test): end-to-end stress test 50 devices`

- [ ] 60. Docker 镜像构建与离线分发（多架构 / 宿主机指纹注入 / 持久卷 / 串口与网络映射 / cosign 签名）

  **What to do**:
  - 创建 `deploy/docker/`，按任务 59 已确认的设计实现：
    - `Dockerfile`：多阶段构建（builder 阶段以 **musl 目标静态编译** `headless`，规避 glibc 版本绑定；运行阶段用 `debian:*-slim` 或 `distroless/cc`）；**非 root 用户运行**；只 COPY 必要产物与前端 dist，**不 COPY 源码、私钥、激活码、客户配置**
    - `.dockerignore`：排除 `target/`、`.git/`、`*.key`、`host-fingerprint.json`、测试夹具
    - `docker-compose.yml`：镜像以 **digest 固定**（禁用可变 tag）；`network_mode: host`（南向局域网直连与广播发现）；`devices: ["/dev/ttyUSB0"]` + `group_add: ["dialout"]`；`volumes` = 宿主只读锚点 + 宿主读写数据卷；`restart: unless-stopped`；`healthcheck`（调任务 33 的健康端点）；`mem_limit/cpus`；日志驱动与轮转
    - `install.sh`：**一键安装**（校验离线包 `SHA256SUMS` → `docker load` → 采集宿主锚点并生成 HMAC 签名的 `host-fingerprint.json` → 渲染 compose 实例 → `docker compose up -d` → 健康检查 → 打印访问地址与激活指引）；**幂等，可重复执行**
    - `pack-offline.sh`：`docker save` 多架构镜像 → `tar.gz` + `SHA256SUMS` + `verify.sh`（供无外网现场）
    - `uninstall.sh`：停容器；**默认保留数据卷**（删除需二次确认）
  - **多架构构建**：`docker buildx build --platform linux/amd64,linux/arm64`；接入 CI（ubuntu-latest + QEMU/binfmt）
  - **宿主机指纹注入**（核心，防「重建即换机/重置试用」）：只读挂载宿主 `/etc/machine-id`、`/sys/class/dmi/id/product_uuid`；安装脚本采集**宿主物理网卡 MAC** 写入签名指纹文件；容器内机器码只由这些宿主锚点派生（禁止读容器内 machine-id / veth MAC / 容器主机名）
  - **持久化**：命名卷映射 `/var/lib/iot-daq`（队列 / 遥测库 / 配置 / 日志 / 授权 / 试用标记）；启动时校验卷可写并拒绝写入镜像层
  - **镜像签名与完整性**：cosign 签名（KMS key 或 keyless）；发布 manifest 记录 digest；`cosign verify` 纳入验收脚本
  - **镜像硬化**：非 root（固定 UID）、丢弃 capabilities、不挂载 `/var/run/docker.sock`、评估 rootfs 只读 + tmpfs（按任务 59 决策）；记录并控制镜像体积
  - 编写测试：镜像构建、非 root 运行、容器内启动 → 管理界面可访问、SIGTERM 优雅停机、卷持久化、串口映射、cosign 校验、离线导入

  **Must NOT do**:
  - 不要用 `--privileged`；不要挂载 `/var/run/docker.sock`
  - 不要把私钥、激活码、宿主指纹、默认口令打进镜像层（镜像可被 `docker save` 完整导出）
  - 不要在 compose 生产引用中使用可变 tag（`:latest`）
  - 不要把数据/试用标记/租约写进容器可写层
  - 不要以裸 `docker run` 长命令作为交付入口（须 compose + 脚本，保证可复现）

  **Recommended Agent Profile**: Category: `deep` — 容器化交付与供应链
  **Parallelization**: Wave 5（与 35-39 并行）；**Blocked By**: 33（运行体）、51（装配与优雅停机）；**Blocks**: 58（部署手册）、61（容器化验收）

  **References**: 任务 59 设计（容器化蓝图）；任务 33（headless 容器友好路径与信号）；任务 51（bootstrap / 优雅停机 / 健康检查）；docker buildx / cosign / compose spec；musl 静态链接
  **Acceptance Criteria**:
  - [ ] `docker buildx` 产出 `linux/amd64` + `linux/arm64` 镜像
  - [ ] `pack-offline.sh` 产出离线 tar + `SHA256SUMS`，且 `verify.sh` 可校验
  - [ ] **无外网**机器 `docker load` 后 `install.sh` 一键启动成功，管理界面可访问
  - [ ] **容器内机器码 == 宿主机采集值**（非容器内 machine-id / MAC）
  - [ ] 数据卷持久化：重建容器后试用标记与授权状态保持
  - [ ] `cosign verify` 通过；compose 以 digest 固定
  - [ ] 容器非 root 运行；未挂 docker.sock；未使用 privileged
  **QA Scenarios**:
    - Happy: `docker compose up -d` → curl 管理界面 → 断言 HTTP 200 且渲染登录页
    - Happy: 串口映射 → 断言容器内可见 `/dev/ttyUSB0` 且 Modbus RTU 采集成功
    - Happy: `docker buildx` 双架构 → 断言两个平台镜像均可启动
    - Error: 断网机器仅用离线 tar → 断言 `docker load` + `install.sh` 成功，全程无外网依赖
    - Error: `docker rm && docker run`（同卷重建）→ 断言机器码不变、试用未被重置、无需重新激活
    - Error: 空卷新建容器（模拟换容器重置试用）→ 断言云端按宿主指纹识别为同一设备，试用不重置
    - Error: 篡改镜像一层后 `cosign verify` → 断言校验失败
    - Error: `docker stop` → 断言 SIGTERM 触发优雅停机、队列 flush 完成、退出码 0
    - Error: 检查容器内进程与 capabilities → 断言主进程非 root 且 capabilities 已丢弃
  **Evidence**: `.omo/evidence/task-60-docker-image.log`, `.omo/evidence/task-60-offline-package.sha256`
  **Commit**: YES — `feat(deploy): multi-arch docker image, offline package & host-anchored licensing`

---

### Wave 6 — 授权 / 防破解 / 激活 实现（★ 最高优先级实现波）

> 全部任务按 Wave 0（40-44）已确认的设计图实现，**不得偏离设计**；如实现中发现设计缺陷，回退改设计并重新确认，不得就地改设计。
> 核心原则：**授权判定必须在 Rust 侧**；**废弃/重发只能由总管理后台经云端执行**；**一码一机**。

- [ ] 45. licensing-server 云授权服务主体（激活 / 心跳 / 白名单 / 试用记录 / Ed25519 签发）

  **What to do**:
  - 创建 `crates/licensing-server`（axum + sqlx/rusqlite，按任务 44 的数据模型建表）
  - 实现任务 44 设计的激活与心跳端点：`POST /activation`、`POST /heartbeat`、`POST /verify`
  - 实现 Ed25519 签发：Lease Token 内容含 `device_id / mid / tier / exp / kid`，签发时带**当前 kid**；客户端用内置公钥验签
  - 实现 `kid` 多密钥并存与轮换（新 Token 用新 kid 签发，旧 kid 继续可验，直到客户端公钥升级完成）
  - 授权白名单与配额校验（设备数、tier、租户）
  - 试用期记录：首次激活时间落库（供任务 23 交叉校验）
  - 按任务 44 契约实现统一错误码（码已废弃 / 已绑定他机 / 无此码 / 超出配额 / 签名失败）
  - 编写测试：签发→验签；过期 Token 被拒；未知 kid 被拒；配额超限被拒

  **Must NOT do**: 不要把签名私钥硬编码进代码或提交进仓库（从环境/KMS 注入）；不要实现客户端可自行解绑的接口；不要把业务 API 与私钥置于同一可写位置

  **Recommended Agent Profile**: Category: `deep` — 服务端 + 密码学
  **Parallelization**: Wave 6（与 46-50 并行）；**Blocked By**: 40-44（设计确认）、2、3；**Blocks**: 22, 23, 46, 47, 48

  **References**: 任务 44 设计（数据模型 + API 契约）；第三轮决议（云端下发 Lease Token、Ed25519 签名、本地内置公钥验签）；ed25519-dalek、axum、sqlx
  **Acceptance Criteria**: `[ ] cargo test --package licensing-server PASS [ ] Token 签发+验签通过 [ ] 过期/未知kid/超配额均被拒 [ ] 私钥未入库`
  **QA Scenarios**:
    - Happy: 激活请求 → 断言返回含 kid 与 exp 的 Token，客户端内置公钥验签通过
    - Error: 用已过期 Token 调 `/verify` → 断言拒绝并返回 `TOKEN_EXPIRED`
    - Error: 伪造 kid → 断言拒绝
    - Error: 提交前搜索仓库 → 断言无私钥/证书私钥文件入库
  **Evidence**: `.omo/evidence/task-45-licensing-server.log`
  **Commit**: YES — `feat(licensing): implement cloud licensing server with Ed25519 issuance`

- [ ] 46. ★ 激活码生命周期与一机一码绑定（发放 / 绑定 / 废弃 / 重发）

  **What to do**:
  - 实现激活码状态机（按任务 42 设计）：`issued → bound → revoked → reissued`
  - **发放**：支持单个/批量生成激活码，携带 tier、有效期、租户、备注/订单号；码本身随机且不可枚举（足够熵 + 校验位）
  - **绑定（一机一码）**：激活时原子性地把码绑定到机器码指纹
    - 码未绑定 → 绑定并签发 Token
    - 码已绑定且指纹匹配（按任务 41 的 N-of-M 容错规则）→ 视为同机，正常续期
    - 码已绑定但指纹不匹配 → **拒绝**，返回 `CODE_ALREADY_BOUND`，写审计（记录尝试的指纹）
  - **废弃**：`revoke` 立即生效——置 `revoked_at`、令关联 lease 失效、后续心跳一律拒绝（原设备下次心跳即降级停发）；**幂等**（重复废弃返回同一结果，不报错）
  - **重发（换机）**：`reissue` 生成新码，可选择
    - (a) 预绑定到指定新机器码，或 (b) 留待新机首次激活时绑定
    - 记录 `reissued_from → new_code` 溯源链；原码保持 `reissued` 终态
    - 新码默认继承原码 tier/剩余有效期（可覆盖）
    - **默认禁止一码多机**（不提供"允许多机"开关，除非用户在后续显式要求）
  - **审计**：废弃/重发必须记录操作者、时间、原因、前后码 ID
  - 完整测试：绑定 → 第二台被拒；废弃 → 原设备心跳被拒；重发 → 新机激活成功；重复废弃幂等

  **Must NOT do**:
  - **不得**在客户端提供任何废弃/重发入口（只能在总管理后台经云端执行）
  - **不得**让同一激活码在第二台设备激活成功（一码一机）
  - **不得**让废弃变成"到期失效"以外的延迟生效——废弃必须立即拒绝心跳

  **Recommended Agent Profile**: Category: `deep` — 业务状态机 + 一致性
  **Parallelization**: Wave 6（与 45, 47-50 并行）；**Blocked By**: 44、45；**Blocks**: 47, 39

  **References**: 任务 42 状态机设计；任务 41 指纹容错规则；本轮新增决议（总管理后台废弃原激活码 → 重新发放）
  **Acceptance Criteria**: `[ ] 四种状态迁移全部实现且幂等 [ ] 第二台设备激活被拒 [ ] 废弃后原设备心跳被拒 [ ] 重发后新机激活成功 [ ] 废弃/重发均有审计记录`
  **QA Scenarios**:
    - Happy: 发码 → 机器 A 激活成功 → 在机器 B 用同一个码激活 → 断言返回 `CODE_ALREADY_BOUND` 且未签发 Token
    - Happy: 后台废弃码 → 机器 A 下一次心跳 → 断言被拒且本地进入降级（停北向转发）
    - Happy: 后台重发 → 新码在机器 B 激活 → 断言成功，且审计链路含 `reissued_from`
    - Error: 连续两次调用 revoke → 断言均返回成功（幂等），无异常/重复扣减
    - Error: 直接以客户端身份调用 revoke 端点 → 断言 403
  **Evidence**: `.omo/evidence/task-46-activation-code-lifecycle.log`
  **Commit**: YES — `feat(licensing): activation code lifecycle with one-code-one-machine binding`

- [ ] 47. ★ 总管理后台 admin-console（激活码 / 设备 / 租户管理）

  **What to do**:
  - 创建 `admin-console/`（Vue 3 + Vite + TypeScript + **Arco Design Vue + 复用 `ui-kit/`**），与网关侧 `web-console/` **代码与部署分离**（`ui-kit/` 是唯一共享层，不得复制 token）
  - **按任务 64 的界面设计稿（`docs/design/ui-admin-console.md`）实现页面**，不自行发明页面结构与交互；危险操作形态严格照设计（影响提示 + 原因必填 + 对象名二次校验）
  - 按任务 44 的线框实现页面：
    - **激活码列表**：筛选（状态 / 租户 / tier / 时间）、分页、导出；列显示绑定设备指纹摘要与生效时间
    - **激活码详情**：完整生命周期时间线（发放 → 绑定 → 废弃 → 重发）、审计记录
    - **废弃操作**：二次确认弹窗，**明确提示「原设备将立即停止北向转发」**，要求填写原因
    - **重发操作**：对话框可选「预绑定新机器码」或「留待首次激活绑定」，展示继承的 tier / 有效期，可覆盖
    - **设备列表 / 租户列表**：租约状态、最近心跳、tier、试用/正式
    - **审计日志页**：按操作者/时间/类型查询
  - 管理员登录 + RBAC（与任务 57 的角色模型对齐）
  - 所有写操作走任务 45/46 的管理 API，**前端不做任何授权判定**
  - 编写测试：vitest 组件测试 + playwright 端到端（废弃 → 重发全流程）

  **Must NOT do**: 不要把管理后台与网关侧管理界面混为同一个应用/同一份构建产物；不要让前端持有签名私钥或直接访问数据库；不要省略废弃操作的二次确认

  **Recommended Agent Profile**: Category: `unspecified-high` — 管理后台前端
  **Parallelization**: Wave 6（与 45, 46, 48-50 并行）；**Blocked By**: 31（前端脚手架与鉴权范式复用）、44、46、64（界面设计稿）、27（ui-kit）；**Blocks**: 39, 69

  **References**: 任务 44 线框与 API 契约；任务 31（JWT 鉴权范式）；Vue 3 + Vite + Arco Design + playwright
  **Acceptance Criteria**: `[ ] vitest PASS [ ] playwright 废弃→重发流程通过 [ ] 废弃操作有二次确认与影响提示 [ ] 构建产物独立于 web-console`
  **QA Scenarios**:
    - Happy（playwright）: 登录 → 查激活码 → 点废弃 → 弹出影响提示 → 确认 → 断言列表状态变为 revoked
    - Happy（playwright）: 点重发 → 填新机器码 → 提交 → 断言生成新码并可见溯源关系
    - Error（playwright）: 不填原因直接废弃 → 断言提交被拦截
    - Error: 检查网络请求 → 断言写操作均打到管理 API，前端无本地状态伪造
  **Evidence**: `.omo/evidence/task-47-admin-console.log`
  **Commit**: YES — `feat(admin): implement vendor admin console for activation codes`

- [ ] 48. 二次校验三档实现（A 云端验签 + B 网关侧校验与审计回执 + C 纯本地）

  **What to do**:
  > ⚠️ **拓扑前提（本轮定案）**：默认拓扑是「客户自建 MQTT Broker，业务数据不经厂商」。因此原「服务端二次校验」在默认拓扑下**没有生效路径**——必须按档位实现。**不变式：授权判定始终在网关 Rust 侧，档位只决定能拿到多少外部证据。**
  - **A 档 · 云直连（企业增值服务）**：在 `crates/licensing-server` 实现 `/verify` 完整校验链（按任务 44 契约）：
    1. 解析 AuthBlock（mid / nonce / ts / sig）
    2. 用设备公钥验签（对**业务语义确定性哈希**，与任务 21 完全一致的口径——两端口径必须逐字段对齐并有交叉测试）
    3. 时间窗口 ±5 分钟（基于任务 56 的可信时间）
    4. Nonce 防重放：`nonce_cache` 表 + TTL（> 窗口期），已出现即拒
    5. mid 与已绑定设备指纹一致性校验
    - Nonce 缓存容量与过期清理策略；拒绝原因码 `SIG_INVALID` / `TS_OUT_OF_WINDOW` / `NONCE_REPLAY` / `MID_MISMATCH`
  - **B 档 · 直连客户 Broker（默认）**：校验责任下沉到网关（Rust 侧），厂商只收**审计回执**
    - 网关侧：AuthBlock 签名（任务 21）+ **单调递增序号 seq** + 本地 nonce/seq 去重窗口 + 连续序号区间账本
    - **审计回执**（新增契约，见任务 44）：只上报 `{device_mid, lease_id, seq_from, seq_to, count, payload_digest, ts, sig}`——**不含任何业务数值**
    - 云端：校验回执签名与设备绑定后落库；检测**序号跳空 / 回退 / 回执缺失**并出异常告警
    - **心跳与回执绑定**：心跳中必须携带最近回执的序号区间；连续 N 次心跳缺区间或区间不连续 → 标记异常 → 可触发降级（停北向）或人工核查
    - **断网语义**：断网时**继续采集与转发**，回执本地持久化、恢复后**补报**（不得因回执失败而停转发）
  - **C 档 · 纯本地（大客户可选）**：仅保留租约心跳，不做二次校验；配置上显式声明并在总管理后台可见
  - **档位配置**：档位为**租户级 / 设备级配置**（随 Lease Token 或云端配置下发）；网关按档位决定回执上报行为；档位变更需重启采集后生效并留审计
  - 编写测试：A 档全链路失败用例；B 档回执生成 / 补报 / 跳空检测；C 档不产生回执；**断网时 B 档仍持续转发**
  - 与任务 21 做**跨端哈希口径一致性测试**（同一 batch 在客户端与服务端算出的哈希一致）

  **Must NOT do**:
  - 不要把非验证（no-verify）开关留在生产代码路径；不要用客户端时间做窗口判断；不要只记 nonce 不做 TTL 清理
  - **不得把 B 档做成「必须在线才能转发」**（断网必须继续采集与转发，回执允许延迟补报）
  - **不得在审计回执中夹带业务数值**（回执只允许序号 / 条数 / 摘要）
  - 不得把授权判定或档位判定放到前端

  **Recommended Agent Profile**: Category: `deep` — 安全校验与审计
  **Parallelization**: Wave 6（与 45-47, 49, 50 并行）；**Blocked By**: 2、21、44；**Blocks**: 39

  **References**: 本轮定案（二次校验三档并存，默认 B）；第三轮决议（云端校验签名 + 时间窗口 ±5 分钟 + Nonce 防重放）；任务 21 签名口径；任务 44 契约（含 `POST /audit/receipt`）；任务 56 可信时间；任务 54（去重与背压）
  **Acceptance Criteria**: `[ ] A 档：重放/超窗/篡改/mid 不匹配均被拒 [ ] B 档：回执生成、补报、跳空告警均通过 [ ] B 档断网仍持续转发 [ ] 回执字段白名单校验无业务数值 [ ] C 档不产生回执 [ ] 跨端哈希口径一致`
  **QA Scenarios**:
    - Happy（A 档）: 合法 AuthBlock → 断言 `/verify` 返回 ok
    - Error（A 档）: 篡改 payload 一个字段 → 断言 `SIG_INVALID`
    - Error（A 档）: ts 改为 6 分钟前 → 断言 `TS_OUT_OF_WINDOW`
    - Error（A 档）: 同一 nonce 连发两次 → 断言第二次 `NONCE_REPLAY`
    - Error: 客户端算出的哈希与服务端不一致（人为改字段顺序）→ 断言跨端口径测试失败并暴露差异
    - Happy（B 档）: 直连客户 Broker 转发 1000 条 → 断言回执 `seq_from/seq_to/count` 与实发一致，且回执体内无业务字段
    - Error（B 档）: 断网 1h 后再联网 → 断言期间持续转发、回执补报且区间连续无跳空
    - Error（B 档）: 人为丢弃 3 个区间不上报 → 断言云端检测出跳空并告警（心跳缺区间 → 异常标记）
    - Error（B 档）: 在回执里塞入一个业务字段 → 断言服务端按字段白名单拒绝
    - Happy（C 档）: 档位设为 C → 断言不产生回执，仅租约心跳
  **Evidence**: `.omo/evidence/task-48-second-stage-verification.log`
  **Commit**: YES — `feat(licensing): tiered second-stage verification with audit receipts`

- [ ] 49. 客户端密钥托管（Ed25519 生成 + DPAPI/keyring 持久化 + 重装恢复）

  **What to do**:
  - 实现 `KeyProvider` trait（供任务 21 消费）与平台实现：
    - Windows：DPAPI（当前用户/机器范围）保护私钥
    - Linux：keyring（Secret Service）或受限权限文件 + 加密
  - 首次启动生成 Ed25519 密钥对；**私钥永不明文落盘、永不外传**；公钥随激活请求上传
  - **重装/恢复路径**（按任务 41 设计）：DPAPI/keyring 不可恢复时如何重新生成密钥并重新激活；旧密钥作废流程
  - 密钥与机器码绑定（HKDF 派生保护密钥，拷盘即失效——与任务 34 一致）
  - 提供 in-memory 私钥句柄，禁止暴露 `to_bytes()` 类接口给业务层
  - 编写测试：密钥生成→持久化→重启复用；拷盘到另一台机器→解密失败；重装→重新激活流程

  **Must NOT do**: 不要把私钥写入配置文件、日志或安装包；不要把私钥上传到云端（只上传公钥）；不要用可逆弱混淆替代 DPAPI/keyring

  **Recommended Agent Profile**: Category: `deep` — 密钥托管与平台安全
  **Parallelization**: Wave 6（与 45-48, 50 并行）；**Blocked By**: 21、43；**Blocks**: 22、34、50

  **References**: 任务 43 设计；Windows DPAPI；Linux keyring/Secret Service；HKDF（与任务 34 同源）；ed25519-dalek
  **Acceptance Criteria**: `[ ] cargo test --package daemon keyprovider PASS [ ] 重启后复用同一密钥 [ ] 拷盘后解密失败 [ ] 私钥不出现在日志/配置/安装包`
  **QA Scenarios**:
    - Happy: 首次启动生成密钥 → 重启进程 → 断言复用同一密钥（公钥不变）
    - Error: 把数据目录拷到另一台机器 → 断言私钥不可解密，走重新激活流程
    - Error: grep 日志与配置文件 → 断言无任何私钥明文
  **Evidence**: `.omo/evidence/task-49-key-provider.log`
  **Commit**: YES — `feat(auth): implement client key provider with DPAPI/keyring`

- [ ] 50. 客户端加固 Tier-1（代码签名 / 二进制硬化 / 完整性自检 / 试用标记冗余 / 时钟回拨）

  **What to do**:
  > **力度已定案（本轮）**：**只做 Tier-1**。Tier-2（控制流平坦化、字符串加密、交叉校验、Frida 检测）**暂不做**；Tier-3（内核驱动、VMProtect/Themida 级壳、硬件加密狗）**明确不做**。**预算 控制面 : 客户端 = 7 : 3**。
  - 按任务 43 设计的 Tier-1 清单实现：
    - **安装包签名**：接入代码签名证书 + 时间戳；CI 产出签名安装包（与任务 38 联动）
    - **二进制硬化（新增，Tier-1 要件）**：release profile 启用 `strip = true`、`lto = "thin"|"fat"`、`panic = "abort"`、关闭 debug info、`opt-level` 权衡；关键字符串与常量做**混淆**（挡住 `strings` 一把梭读取 endpoint / 密钥模式 / 判定文案）
    - **内容完整性**：生成 payload 与**前端资源**哈希清单（manifest），安装时与启动时校验
    - **运行时自检**：启动时校验二进制段哈希与关键资源哈希，失败进入受限模式（停北向转发 + 审计告警）
    - **授权判定在 Rust 侧**：审计并确保 WebView/JS 层无授权判定逻辑（只做展示）；前端资源仅做完整性校验
    - **试用标记多重冗余**：文件 + 系统配置目录 + SQLCipher 库 + 云端首次激活时间，交叉校验并实现单点删除兜底
    - **时钟回拨检测**：单调时钟 + 记录历史最大时间 + 与服务端时间比对（依赖任务 56）
    - **低成本反调试**：`TracerPid` / `IsDebuggerPresent` 一类检测，命中后进入受限模式（**不做对抗性反调试，避免误伤客户环境**）
  - 落实任务 43 的「不做清单」，并在文档中写明定位声明：**提高破解成本，非绝对防破解**；量化目标 = 拦掉 90% 非专业破解尝试
  - 编写测试：篡改安装包被拒；篡改二进制段/前端资源被检出；删除试用标记不重置试用；回拨时间不延长试用

  **Must NOT do**:
  - 不要实现内核驱动/驱动级 hook、不要引入 VMProtect/Themida 级商业壳（Tier-3，首期 Must NOT Have）
  - 不要做对抗性反调试（会误伤客户环境）；不要用明文或可逆弱混淆存储试用标记
  - 不要把授权判定放到前端；不要把机密（密钥/激活码/算法）放前端资源
  - **不要在文档或销售材料中宣称「绝对防破解」**

  **Recommended Agent Profile**: Category: `deep` — 客户端加固
  **Parallelization**: Wave 6（与 45-49 并行）；**Blocked By**: 38、43、49；**Blocks**: 39, F1-F4

  **References**: 任务 43 设计（Tier-1 清单与不做清单）；Tauri 签名与 updater 签名；signtool/osslsigncode；Cargo release profile（strip/lto/panic）
  **Acceptance Criteria**: `[ ] 安装包签名可校验 [ ] release 二进制已 strip 且关键字符串已混淆（strings 抽查无明文 endpoint/密钥模式） [ ] 篡改二进制段或前端资源 → 启动进入受限模式 [ ] 试用标记单点删除不重置 [ ] 时钟回拨不延长试用 [ ] 前端无授权判定逻辑且无机密 [ ] 无 Tier-2/Tier-3 手段引入`
  **QA Scenarios**:
    - Happy: 正常安装 → 启动 → 断言自检通过、授权正常
    - Error: 篡改安装包 payload 一个字节 → 断言安装被拒或启动进入受限模式 + 审计告警
    - Error: 用 `strings` 扫描 release 二进制 → 断言无明文 endpoint、无密钥模式、无判定文案
    - Error: 直接 patch 二进制段 → 断言段哈希自检检出并进入受限模式
    - Error: 删除试用标记文件（其余冗余保留）→ 断言试用期未被重置
    - Error: 回拨系统时间 30 天 → 断言试用期不延长且记录审计
    - Error: 静态检查 web-console / tauri 前端代码与资源 → 断言无授权判定分支、无密钥或激活码
    - Error: 检查依赖与构建配置 → 断言未引入 Tier-2/Tier-3 加固（无壳、无内核驱动）
  **Evidence**: `.omo/evidence/task-50-client-hardening.log`
  **Commit**: YES — `feat(security): tier-1 client hardening & anti-tamper`

### Wave 7 — 工程补齐 + 容器化验收（硬缺项 + 软缺项，10 任务）

> ⚑ 其中 **51（daemon 进程装配）为装配前置**：任务 32/33 的平台壳依赖它，需在 Wave 4 执行前完成（编号靠后仅为保持已有编号稳定）。
> 其余任务按编号顺序执行；**61（容器化交付验收）在 60 完成后执行，须独立于实现方**。

- [ ] 51. ⚑ daemon 进程装配与生命周期（bootstrap / 优雅停机 / 看门狗）

  **What to do**:
  - 实现 `daemon::bootstrap::assemble(config) -> Daemon`：按配置装配全部组件（配置、日志、驱动、处理、调度、缓存、存储、MQTT、授权、管理 API），返回可运行进程句柄；**唯一的装配入口**，供任务 32/33 调用
  - 实现生命周期管理：`start / stop / restart`，**优雅停机**（停止采集 → flush 内存队列与磁盘队列 → 关闭 MQTT → 释放串口/连接）
  - **看门狗与健康检查**：组件心跳、任务 panic 隔离（单个驱动 panic 不拖垮进程）、关键组件失效时的降级与重启策略
  - 组件健康状态聚合（供任务 52 的实时面板消费）
  - 构建/装配失败给出可读诊断（配置错误定位到字段）
  - 编写测试：装配成功启动；kill 单个驱动任务不导致进程退出；停机时队列 flush 完成无丢数据

  **Must NOT do**: 不要在各平台壳（32/33）内重复实现装配逻辑；不要在停机时跳过队列 flush；不要让单个组件的 panic 终止整个 daemon

  **Recommended Agent Profile**: Category: `deep` — 运行时编排
  **Parallelization**: **⚑ 前置任务（须在 Wave 4 之前或并行完成）**；**Blocked By**: 1、8；**Blocks**: 32, 33, 38, 39

  **References**: tokio 任务监督模式；任务 17/18（flush 语义）；任务 52（健康状态消费）
  **Acceptance Criteria**: `[ ] cargo test --package daemon bootstrap PASS [ ] 单驱动 panic 不中止进程 [ ] 优雅停机无数据丢失 [ ] 32/33 均调用同一装配入口`
  **QA Scenarios**:
    - Happy: `assemble(config)` → start → 断言所有组件就绪且健康状态上报正常
    - Error: 注入单个驱动 panic → 断言进程存活、其余驱动继续采集、审计记录故障
    - Error: 停机时队列有未发数据 → 断言 flush 完成后退出，重启无数据丢失
  **Evidence**: `.omo/evidence/task-51-bootstrap.log`
  **Commit**: YES — `feat(daemon): implement process assembly & lifecycle`

- [ ] 52. 后端管理 API 层 + 实时数据通道（REST + WS/SSE + 状态聚合）

  **What to do**:
  - 实现网关侧管理 API（axum，供 `web-console` 与平台壳调用）：
    - 配置 CRUD：设备 / 点位 / 协议 / 规则（含 schema 校验、下发生效、热重载触发）
    - 状态查询：设备连接状态、采集频率、队列深度、授权状态、tier
    - 日志与审计查询：按时间范围 / 级别 / 类型分页
    - 操作类：OTA 触发（任务 35）、远程运维（任务 36）统一走本层
  - 实现**实时数据通道**：WebSocket（或 SSE）推送实时点位数值、设备连接变化、告警；支持订阅过滤（按设备/点位）
  - 状态聚合模块：采集成功率、队列水位、连接数等指标的集中计算（任务 29 面板的数据来源）
  - 全部端点鉴权（复用任务 31 的 JWT 中间件）+ 写操作审计（任务 26）
  - 编写测试：CRUD 端到端；WS 推送到达；未授权返回 401/403

  **Must NOT do**: 不要让前端直连内部结构体/Tauri command 绕过本层；不要在 WS 上推送未过滤的全量数据（需支持订阅过滤，避免 200 设备场景打爆前端）；不要跳过写操作审计

  **Recommended Agent Profile**: Category: `deep` — 后端 API
  **Parallelization**: Wave 7（与 53-58 并行）；**Blocked By**: 1、20、26；**Blocks**: 28, 29, 30, 36, 39

  **References**: axum；任务 31（JWT 中间件）；任务 26（审计）；任务 51（健康状态源）
  **Acceptance Criteria**: `[ ] cargo test --package daemon admin_api PASS [ ] 配置 CRUD 端到端生效 [ ] WS 订阅过滤生效 [ ] 未授权 401/403 [ ] 写操作有审计`
  **QA Scenarios**:
    - Happy: 通过 API 新增设备 → 断言配置生效且热重载触发；查询返回新设备
    - Happy: 订阅 1 个点位 → 断言仅收到该点位的推送
    - Error: 无 Token 调配置写接口 → 断言 401；普通角色调管理员接口 → 断言 403
    - Error: 200 设备全量订阅 → 断言有背压/降采样保护，不导致前端卡死
  **Evidence**: `.omo/evidence/task-52-admin-api.log`
  **Commit**: YES — `feat(api): implement admin API & realtime channel`

- [ ] 53. 数据类型解码器 + 质量码规范（IEEE754 / 字节序组合 / 位与字符串 / quality）

  **What to do**:
  - 实现统一解码器 `ValueDecoder`：
    - 数据类型枚举：bool / int8-64 / uint8-64 / float32 / float64 / string / bitset
    - **跨寄存器数值**：float32/64、int32/64 的寄存器拼接与缩放因子（scale/offset）
    - **字节序组合**：ABC(D) / CBA(D) / BADC / CDAB / DCBA 全组合（Modbus 32 位最易错点），S7 大端、MC 小端
    - 位（bit）与位串（bitset）提取；字符串（定长/结束符）
  - 实现**质量码规范**：定义统一 quality 枚举与语义（good / bad / uncertain / timeout / comm_error / out_of_range / **calc_failed**），给出各驱动状态 → 统一 quality 的映射表（OPC UA StatusCode、Modbus 异常码/超时、S7/MC 错误）
    - **`calc_failed` 由任务 70（公式计算）产出**：计算点因输入缺失/超时/非数值/除零/结果 NaN·±Inf 时置该质量码；本任务定义其**语义、枚举值与「最差质量继承」规则**（多输入取最差），70 只做使用不做定义（避免 53↔70 循环依赖）
  - 与任务 15 对齐：15 只透传 quality 原值，本任务完成后由其调用本规范做规范化
  - 编写测试：每种数据类型 + 每种字节序组合的往返用例（含已知真值向量）；质量码映射用例

  **Must NOT do**: 不要只支持单一字节序（必须全组合）；不要把 quality 语义硬编码在驱动里；不要用字符串拼接做数值解码（避免精度与性能问题）

  **Recommended Agent Profile**: Category: `deep` — 协议解码基础
  **Parallelization**: Wave 7（与 52, 54-58 并行）；**Blocked By**: 8、15；**Blocks**: 39

  **References**: Modbus 字节序惯例（ABCD/CDAB/BADC/DCBA）；IEEE754；OPC UA StatusCode；S7 大端 / MC 小端
  **Acceptance Criteria**: `[ ] cargo test --package daemon decoder PASS [ ] 4 种 32 位字节序组合全部通过 [ ] float32/float64 真值向量通过 [ ] 质量码映射表覆盖各驱动`
  **QA Scenarios**:
    - Happy: 寄存器 [0x41C8_0000] 按 ABCD 解为 float32 → 断言 = 25.0
    - Happy: 同一数据按 CDAB 解码 → 断言得到预期值（与 ABCD 不同，验证组合确实生效）
    - Error: 用错误字节序解 float32 → 断言结果明显异常（作为反例固化到测试中）
    - Error: Modbus 超时 → 断言 quality 映射为 `timeout` 而非 `good`
  **Evidence**: `.omo/evidence/task-53-decoder.log`
  **Commit**: YES — `feat(protocol): implement value decoder & quality code spec`

- [ ] 54. 补发幂等去重 + 背压与内存队列水位

  **What to do**:
  - **幂等去重**：
    - 客户端：以 `gateway_id + batch_seq` 为幂等键；补发前读 high-water mark（任务 17 已预留）
    - 服务端（若接入厂商云/客户 Broker 允许）：提供幂等键校验与重复丢弃（落到 licensing-server 或文档化的 Broker 侧方案）；对客户自建 Broker 场景，输出"去重责任在消费端"的明确说明与参考实现
    - 明确 Ack 语义与位点推进时机（先落 Ack 后推进位点，避免丢数据）
  - **背压**：
    - 内存队列水位（高水位/硬上限）与溢出策略（与任务 17 对齐），丢数据必须审计
    - MQTT 慢消费者保护：发送队列水位 + 超限时的落盘降级（不阻塞采集）
    - 采集侧背压：队列长期高位时对采集频率的降采样或告警策略
  - 输出容量估算文档：200 设备 × 100ms 的每秒消息数、单条大小、7 天容量与 10GB 上限的验证
  - 编写测试：重复补发不产生重复入库；慢消费者下内存不无限增长；水位触顶产生审计

  **Must NOT do**: 不要用"忽略重复"以外的隐式丢弃；不要在队列高位时静默丢数据；不要让慢消费者阻塞采集路径

  **Recommended Agent Profile**: Category: `deep` — 可靠性机制
  **Parallelization**: Wave 7（与 52, 53, 55-58 并行）；**Blocked By**: 17、19；**Blocks**: 39

  **References**: 任务 17（队列与水位预留）；任务 19（MQTT QoS）；第三轮（环形覆盖 10GB/7 天）
  **Acceptance Criteria**: `[ ] cargo test --package daemon idempotency PASS [ ] 重复补发无重复入库 [ ] 慢消费者内存有界 [ ] 水位触顶有审计`
  **QA Scenarios**:
    - Happy: 模拟补发重复批次 → 断言服务端/消费端只入库一次
    - Error: 补发中途 Ack 丢失 → 断言位点未推进，重发后仍不产生重复
    - Error: MQTT 消费者人为变慢 → 断言内存队列不无限增长，超限落盘并审计
    - Error: 200 设备 × 100ms 跑 10 分钟 → 断言队列水位曲线有记录，未触顶
  **Evidence**: `.omo/evidence/task-54-idempotency-backpressure.log`
  **Commit**: YES — `feat(reliability): idempotent resend & backpressure control`

- [ ] 55. SQLite schema 迁移框架 + 配置版本迁移与回滚（安全模式启动）

  **What to do**:
  - **DB 迁移框架**：`telemetry.db` / `queue.db` / 审计库统一版本管理（`user_version` + `schema_migrations` 表，任务 18 已预留版本位）；启动时自动检测并顺序执行迁移；迁移失败可回滚（事务包裹）
  - **配置版本化**：配置文件 schema 版本号 + 迁移器（旧配置 → 新结构）；配置校验失败时**拒绝启动并给出定位到字段的错误**
  - **配置回滚 / 安全模式**：启动前备份上一份有效配置；若新配置导致启动失败，自动回退到上一份有效配置并以安全模式启动（仅保留本地采集、停北向转发），产生审计告警
  - 编写测试：旧版 DB 升级成功；迁移失败回滚；坏配置触发安全模式且服务可起

  **Must NOT do**: 不要做破坏性迁移而不备份；不要在配置错误时直接崩溃退出（必须能进安全模式并可远程诊断）；不要跳过迁移版本校验（防止降级后损坏）

  **Recommended Agent Profile**: Category: `deep` — 迁移与容错
  **Parallelization**: Wave 7（与 52-54, 56-58 并行）；**Blocked By**: 4、18；**Blocks**: 39

  **References**: 任务 18（版本位预留）；SQLite `user_version` / PRAGMA；配置 schema（任务 4）
  **Acceptance Criteria**: `[ ] cargo test --package daemon migration PASS [ ] 旧库升级成功 [ ] 迁移失败可回滚 [ ] 坏配置进安全模式可启动`
  **QA Scenarios**:
    - Happy: 用 v1 库启动新版 → 断言迁移执行且版本号更新，数据完整
    - Error: 迁移脚本中途报错 → 断言事务回滚，库仍为原版本且可启动
    - Error: 写入非法配置（缺必填字段/类型错）→ 断言回退上一份有效配置 + 安全模式启动 + 审计告警
  **Evidence**: `.omo/evidence/task-55-migration.log`
  **Commit**: YES — `feat(storage): schema migration & config rollback`

- [ ] 56. 可信时间（NTP 校时 + 单调时钟 + 时钟回拨检测）

  **What to do**:
  - 启动时与周期性 NTP 校时（可配置服务器/间隔；离线环境支持"上次已知偏差"策略）
  - 维护**单调时钟**基准，避免系统时间跳变影响调度（任务 16）
  - **时钟回拨检测**：记录历史最大时间戳，检测回拨并上报；回拨期间不延长试用期与授权宽限期
  - 对外暴露统一时间服务接口（`now_trusted()` / `now_monotonic()`），供任务 23（试用）、任务 48（±5min 窗口）、任务 50（回拨检测）消费
  - 与服务端时间偏差超阈值时产生告警并记审计
  - 编写测试：时间回拨被检出；单调时钟不受系统时间跳变影响；NTP 不可达时降级可用

  **Must NOT do**: 不要用系统墙钟做超时/调度基准；不要在 NTP 不可达时拒绝启动；不要因校时跳变导致调度重复或漏触发

  **Recommended Agent Profile**: Category: `quick` — 时间服务
  **Parallelization**: Wave 7（与 52-55, 57, 58 并行）；**Blocked By**: 6；**Blocks**: 23, 48, 50

  **References**: NTP 客户端 crate；tokio 定时器；任务 23/48/50 的时间依赖
  **Acceptance Criteria**: `[ ] cargo test --package daemon time PASS [ ] 回拨被检出 [ ] 单调时钟不受墙钟跳变影响 [ ] NTP 不可达可降级`
  **QA Scenarios**:
    - Happy: 正常校时 → 断言 `now_trusted()` 与服务端偏差在阈值内
    - Error: 系统时间回拨 1 小时 → 断言检出并审计，试用期/宽限期不延长
    - Error: 阻断 NTP → 断言服务正常运行并使用上次已知偏差
  **Evidence**: `.omo/evidence/task-56-trusted-time.log`
  **Commit**: YES — `feat(core): trusted time service with rollback detection`

- [ ] 57. 平台差异补齐 + RBAC（Windows 服务化 / 开机自启 / 串口权限 + 角色权限）

  **What to do**:
  - **Windows**：以服务/计划任务方式**无登录运行**、开机自启、服务与桌面版共存策略；串口设备（COM 口）占用与权限处理
  - **Linux**：systemd 单元完善（依赖顺序、重启策略、资源限制）；`/dev/ttyUSB*` 权限（udev 规则）与串口独占
  - **Linux 容器形态**：路径策略须同时满足原生与容器两种形态（可写目录可由环境变量覆盖 → 便于卷挂载）；串口在容器内经 `--device` + `dialout` 组授权，**权限策略与 udev 规则保持同源**；具体实现由任务 60 落地、验收见任务 61
  - 统一的跨平台路径与权限策略（数据目录、日志目录、配置目录按平台惯例，且**可被环境变量覆盖以适配容器卷挂载**）
  - **RBAC**：角色模型（admin / operator / viewer）+ 权限矩阵，落地到任务 52 的管理 API 与任务 47 的管理后台；初始密码生成与强制改密流程
  - 编写测试：权限矩阵用例（viewer 不能改配置、operator 不能改授权相关）；服务化启动冒烟

  **Must NOT do**: 不要用默认口令且不允许修改；不要把所有 API 一律只做"登录即可"（必须按角色授权）；不要在 Windows 上要求用户保持登录会话才能采集

  **Recommended Agent Profile**: Category: `unspecified-high` — 平台与权限
  **Parallelization**: Wave 7（与 52-56, 58 并行）；**Blocked By**: 1、31；**Blocks**: 32, 33, 39

  **References**: Windows 服务/计划任务；systemd unit；udev 规则；任务 31（JWT）；任务 44（管理员体系）
  **Acceptance Criteria**: `[ ] 权限矩阵测试通过 [ ] Windows 无登录可运行 [ ] Linux systemd 自启与串口权限生效 [ ] 初始口令强制修改 [ ] 数据/日志/配置路径可被环境变量覆盖（容器卷挂载前提）`
  **QA Scenarios**:
    - Happy: viewer 登录 → 查询正常 → 断言写接口 403
    - Happy: Windows 注销后服务继续采集（无登录会话）
    - Error: 首次登录用默认口令 → 断言强制跳转改密
    - Error: 非 root 访问 `/dev/ttyUSB0`（udev 未生效）→ 断言给出明确权限提示而非静默失败
    - Error: 将数据目录环境变量指向不可写路径 → 断言启动即报错（不静默退化到镜像层/只读目录）
  **Evidence**: `.omo/evidence/task-57-platform-rbac.log`
  **Commit**: YES — `feat(platform): service mode, serial permissions & RBAC`

- [ ] 58. 工程合规与文档（cargo-deny 许可证门禁 + 四份手册 + 北向双编码文档 + 公式手册索引）

  **What to do**:
  - **许可证合规**：接入 `cargo-deny` 到 CI，配置许可证白名单（MIT / Apache-2.0 / BSD / ISC 等）；对 MPL-2.0（async-opcua）等做显式豁免并注明理由；构建失败即阻断合并
  - **依赖审计**：`cargo audit` 纳入 CI（已知漏洞阻断）
  - **北向双编码文档**（实现见任务 62，本轮起 JSON 是**生产可选编码**）：写明每路出口如何配置 `protobuf` / `json`、大整数以字符串输出的原因与读取方式、MQTT 5 编码声明属性、**JSON 模式下的性能与推荐设备上限**、以及「两种编码验签语义一致」的保证
  - **文档交付**（`docs/`）：
    1. 部署手册（Windows NSIS/MSI + Linux 原生 AppImage/deb/rpm + systemd + 服务化 + 串口权限 + **Docker 部署：离线镜像导入、compose 启动、宿主机指纹注入、持久卷与备份、串口/网络映射配置**）
    2. 协议接入指南（Modbus/OPC UA/S7/MC/HTTP/第三方 MQTT + 数据类型与字节序配置示例 + **北向双编码配置与 JSON 大整数字符串读取示例**）
    3. API 文档（网关侧管理 API + 云端授权 API，含激活/心跳/废弃/重发 + **A/B/C 三档二次校验与 `POST /audit/receipt` 契约**）
    4. 运维手册（授权异常处置、换机重发流程、**容器重建/迁移后的授权核验**、**B 档回执缺失/跳空的处置流程**、安全模式恢复、备份与迁移、常见故障）
  - **索引与交叉引用（公式手册）**：任务 70 产出《公式与计算点手册》（语法、函数清单、依赖与环、失败策略与质量码、历史不重算）；本任务负责把它登记进 `README.md` 与 `docs/` 索引，并在协议接入指南中以**引用方式**指向（**不复制内容**，避免双份维护与口径漂移）
  - 添加 `README.md` 与 `docs/design/` 索引，确保 Wave 0 设计图与实现一致（不一致处标注并回改；**容器化章节须与任务 59 设计、任务 60 实现三方对齐**）

  **Must NOT do**: 不要跳过许可证检查直接合并；不要把文档写成占位空文件（每篇需有可执行步骤与示例）；不要让文档把 **Protobuf 与 JSON 说成优劣二选一**（JSON 是生产可选编码，需如实写明精度与性能代价）

  **Recommended Agent Profile**: Category: `unspecified-high` — 合规与文档
  **Parallelization**: Wave 7（与 52-57, 61 并行）；**Blocked By**: 4、5、38、60；**Blocks**: 39, F1-F4

  **References**: cargo-deny / cargo-audit；第四轮决议（Protobuf 主 + JSON 调试）；任务 10（MPL-2.0 备注）；任务 59/60（容器化设计与实现）
  **Acceptance Criteria**: `[ ] cargo deny check 通过（含显式豁免项） [ ] cargo audit 无高危 [ ] 北向双编码文档齐全（配置方式 / 大整数规则 / MQTT5 声明 / JSON 性能上限） [ ] docs/ 下 4 份手册齐全且含可执行步骤 [ ] 部署手册含 Docker 离线部署全流程 [ ] API 文档含 A/B/C 三档与审计回执契约 [ ] 索引已登记公式与计算点手册（引用而非复制）`
  **QA Scenarios**:
    - Happy: `cargo deny check licenses` → 断言通过并列出豁免项
    - Error: 临时引入一个 GPL 依赖 → 断言 CI 门禁失败
    - Happy: 按文档把某路出口切为 JSON → 断言上报为 JSON、字段与 Protobuf 语义一致，且大整数为字符串；切回 protobuf 正常
    - Error: 检查 docs/ → 断言无空文件、每篇有命令示例
    - Error: 仅按部署手册操作（Docker 章节）在干净无外网机器部署 → 断言可完成部署，无需临场补充步骤
  **Evidence**: `.omo/evidence/task-58-compliance-docs.log`
  **Commit**: YES — `chore(compliance): license gate, docs & JSON debug mode`

- [ ] 61. 容器化交付验收（重建不重置试用 / 宿主机指纹一致 / 串口与网络直通 / 镜像签名）

  **What to do**:
  - **独立验收**（不复用任务 60 实现者的自测结论），对照任务 59 设计逐项核对：
  - **核心反绕过验证（容器不得成为重置试用或规避一机一码的路径）**：
    - `docker rm && docker run`（**同卷**重建）→ 机器码、租约、试用标记均不得变化
    - **空卷**新建容器（模拟"换容器重置试用"）→ 云端须按宿主机指纹识别为**同一设备**，试用按云端首次激活时间判定，不得被重置或延长
    - 把数据卷整体拷到**另一台宿主**运行 → 指纹不同 → 授权失效（与原生「拷盘即失效」同构）
    - `docker exec` 篡改容器内 `/etc/machine-id` → 断言机器码不受影响（锚点来自宿主只读挂载）
  - **供应链完整性**：`cosign verify` 校验镜像；检查 compose 是否以 digest 固定；故意改用可变 tag 或篡改一个 layer → 断言被拒 / 校验失败
  - **离线部署**：在**无外网**机器上仅用离线包完成 `docker load` + `install.sh` + 启动 + 访问管理界面
  - **设备接入**：Modbus RTU（`--device` 串口映射）与 Modbus TCP（`--network host` 局域网直连）在容器内采集成功
  - **数据完整性**：`docker stop` → SIGTERM 优雅停机、队列 flush 完成、退出码 0；重启后补发**无重复**（复用任务 54 幂等能力）
  - **容器安全基线**：主进程非 root、未挂载 `/var/run/docker.sock`、未使用 `--privileged`、capabilities 已丢弃、**镜像层内无敏感物**（`docker history` + 层内扫描私钥/激活码/宿主指纹特征）
  - 输出验收报告至 `.omo/evidence/task-61/`（含每条命令与原始输出）

  **Must NOT do**:
  - 不得由任务 60 的实现者自测即判定通过（须独立执行）
  - 不得只验「容器能起来」——**必须验重建语义与绕过路径**
  - 不得放宽 Must NOT Have 中的容器禁项（privileged / docker.sock / 敏感物入镜像）

  **Recommended Agent Profile**: Category: `deep` — 容器化验收与反绕过验证
  **Parallelization**: Wave 7（与 51-58 并行）；**Blocked By**: 33、57、60；**Blocks**: 39, F1-F4

  **References**: 任务 59 设计（验收对照基准）；任务 60 实现；任务 54（补发幂等）；cosign / docker CLI；任务 43（分发物防篡改口径）
  **Acceptance Criteria**:
  - [ ] **6 份设计图（含容器化）与实现逐项比对无偏离**（有偏离则已回改设计并重新确认）
  - [ ] 镜像签名校验通过；可变 tag 引用被发现并判不合格
  - [ ] 无外网机器离线导入部署成功
  - [ ] 容器内机器码 == 宿主锚点派生值，且容器内改不动
  - [ ] 同卷重建：试用与租约保持；空卷重建：云端识别同一设备且试用不重置
  - [ ] 跨宿主拷贝数据卷 → 授权失效
  - [ ] 串口与局域网 TCP 采集均成功
  - [ ] 安全基线：非 root / 无 socket / 无 privileged / 镜像内无敏感物
  - [ ] `docker stop` 优雅停机 + 重启补发无重复
  **QA Scenarios**:
    - Happy: 离线 tar → `install.sh` → 激活 → 采集 → 断言端到端通过
    - Error: `docker rm && docker run`（同卷）→ 断言机器码 / 租约 / 试用均不变
    - Error: 空卷重建 → 断言云端识别为同一设备、试用不重置
    - Error: 数据卷拷至另一宿主 → 断言授权失效
    - Error: `docker exec` 篡改容器内 machine-id → 断言机器码不受影响
    - Error: 篡改镜像并改用 `:latest` → 断言 `cosign verify` 失败 / 引用被拒
    - Error: `docker stop` → 断言优雅停机、退出码 0、重启后无重复补发
    - Error: 镜像层内扫描 → 断言无私钥 / 激活码 / 宿主指纹残留
  **Evidence**: `.omo/evidence/task-61/report.md` + 各命令原始输出
  **Commit**: YES — `test(deploy): container delivery acceptance & anti-bypass verification`

- [ ] 66. 账号与角色管理页面（用户增删改 + 角色权限矩阵 + 首次强制改密 + 登录锁定）

  **What to do**:
  - 按任务 63 线框实现「账号与角色」页：用户列表（用户名 / 姓名 / 角色 / 口令状态 / 最近登录 / 来源 IP）、新增用户（初始口令一次性展示 + 强制改密）、启用/停用、重置口令
  - **角色权限矩阵**表格就地展示：`admin` / `operator` / `viewer` × 功能项，明确标注「废弃 / 解绑激活码 = 客户端无此能力」
  - **首次登录强制改密**：默认口令未改前禁止进入任何业务页面（路由守卫 + 后端双重校验）
  - **登录失败锁定**：连续 5 次口令错误锁定 10 分钟，并记审计
  - 前端 `RoleGate` 仅控制可见性；写接口在任务 57 的 RBAC 层强制执行（越权返回 403）
  - 编写测试：新增用户、角色切换后菜单可见性、viewer 越权被 403、强制改密阻断、锁定生效与自动解锁

  **Must NOT do**: 不要把权限判定只做在前端（后端必须独立校验）；不要在界面上提供任何「关闭强制改密」的开关；不要以明文回显或持久化用户口令

  **Recommended Agent Profile**: Category: `unspecified-high` — 账号与权限前端
  **Parallelization**: Wave 7（与 51-58、61 并行）；**Blocked By**: 31（鉴权范式）、57（RBAC 角色模型）；**Blocks**: 69

  **References**: 任务 63 线框；任务 57（RBAC 与首次改密、登录锁定）；任务 31（JWT 与路由守卫）
  **Acceptance Criteria**:
  - [ ] 可新增 / 启停用户；初始口令仅一次展示且强制改密
  - [ ] 角色权限矩阵与后端 57 的角色模型逐项对齐
  - [ ] viewer 登录后写操作入口不可见，且直接调用写接口返回 403
  - [ ] 默认口令未改前无法进入业务页面（前端守卫 + 后端拒绝双证）
  - [ ] 连续 5 次错误口令锁定 10 分钟并记审计
  **QA Scenarios**:
    - Happy: 新增 operator 用户 → 用初始口令登录 → 断言强制跳转改密页
    - Error: viewer 直接 `PUT /api/settings` → 断言 403 且审计留痕
    - Error: 绕过前端守卫直接访问业务路由 → 断言后端拒绝并落审计
    - Error: 连续 5 次口令错误 → 断言第 6 次被锁定并提示剩余时间；10 分钟后自动解锁
  **Evidence**: `.omo/evidence/task-66-accounts-rbac.log`
  **Commit**: YES — `feat(ui): accounts & role management with forced password change`

---

### Wave 7b — 点位表公式计算 + 界面终验（3 任务）

> ★ **执行顺序 70 → 71 → 69**（不等于编号顺序）：公式引擎（70）→ 公式编辑器（71）→ **界面终验（69）必须在全部 UI 任务完成后**。
> 公式计算依赖任务 15（单位换算）与 53（类型与质量码），故本波在 Wave 7 之后启动；69 的界面终验需同时覆盖 65-68 与 71 的界面。

- [ ] 69. 界面实现与设计稿一致性验收（两端逐项比对 + 门控验证 + 真实浏览器）

  **What to do**:
  - **独立于实现方**（本任务不得由实现 27-34、47、65-68 的同一执行者自评通过）
  - 逐页比对 `docs/design/ui-gateway-console.md` / `ui-admin-console.md` 与实际界面：页面清单是否齐全、元素位置与文案、空态与错误态、危险操作确认形态
  - **必须验证的门控**：客户端可点元素扫描**无**解绑 / 重置试用 / 更换机器码；`/api/license/*` 无 revoke / unbind / reset-trial 端点；前端无授权判定分支（静态扫描 + 运行时断网试验）
  - **必须验证的交互**：实时监控 1s 节流生效（记录 CPU 基线）；点表导入错误行号可定位；北向编码切换后精度提示与一致性自检可用；**公式编辑器可用且校验不可绕过**（非法公式与成环公式均被拦下并显示环路径；试算结果来自后端 dry-run 而非前端伪造）；两端共用 `ui-kit/`，视觉无漂移（比对关键组件的字号/间距/圆角计算值）
  - **RBAC 实测**：三角色分别登录，断言菜单可见性与接口 403 行为与设计一致
  - 用真实浏览器（headless Chromium + CDP）执行并截图存证；对照原型逐页走查
  - 输出：偏离清单（页面/项/期望/实际/严重度）+ 结论 `VERDICT: APPROVE / REJECT`

  **Must NOT do**: 不要只跑构建与单测就判定通过（必须真实浏览器交互）；不要用「元素存在」代替「行为生效」的断言；不要由实现方自评；**不要以静态文本扫描代替行为断言**（扫页面全文会命中说明文案本身，须扫可点元素或断言行为）

  **Recommended Agent Profile**: Category: `unspecified-high` — 独立验收
  **Parallelization**: **Wave 7b 末段（界面终验，必须在 27-34、47、65-68、71、72 全部完成后执行）**；**Blocked By**: 63、64（设计稿基准）、**72（客户端界面 v2 终稿：向导 / 主从布局 / 分页契约）**、27-34、47、65-68、**71（公式编辑器）**、**70（公式校验口径）**；**Blocks**: 39, F1-F4

  **References**: `docs/design/prototype/gateway-v2a-glacier.html`（客户端界面实现基准，方案 A 已选定）、`docs/design/ui-design-system.md`、`ui-gateway-console.md`、`ui-admin-console.md`、`docs/design/prototype/*.html`；任务 63/64/72 的验收要点；headless Chromium + CDP 验证方法
  **Acceptance Criteria**:
  - [ ] 两端页面清单与设计稿 100% 对齐（缺页 / 多页均记为偏离）
  - [ ] 危险操作确认形态齐备（影响提示 + 原因必填 + 二次校验）
  - [ ] 客户端无解绑 / 重置试用入口（可点元素扫描 + 端点扫描双重确认）
  - [ ] 实时节流生效且有 CPU 基线记录
  - [ ] 三角色 RBAC 可见性与 403 行为符合设计
  - [ ] 偏离清单已产出，无「未修复即关闭」的 P0/P1 偏离
  **QA Scenarios**:
    - Happy: 逐页走查客户端 14 页 + 后台 10 页 → 断言页面齐全、关键元素与文案一致
    - Error: 扫描客户端可点元素 → 断言无解绑 / 重置试用命中（若有则 REJECT）
    - Error: 抓取前端网络请求 → 断言写操作均打到后端 API，前端无本地状态伪造
    - Error: 200 设备场景记录渲染 CPU 与帧率 → 断言节流生效（对照未节流基线）
    - Error: viewer 登录后直接调用写接口 → 断言 403（前端隐藏不算通过）
  **Evidence**: `.omo/evidence/task-69/ui-consistency-report.md` + 各页截图 + 浏览器验证日志
  **Commit**: YES — `test(ui): design-to-implementation consistency acceptance`

- [ ] 70. 公式引擎与计算点（受限表达式 AST + 白名单函数 + 依赖 DAG 拓扑序 + 环检测 + 失败质量码 + dry-run API）

  **定位与边界（与 15/20/37/53 互斥）**: 本任务在**点位表模型层**实现「计算点 / 派生点」，是数据处理链路的一环。**不等同于任务 20/37 的转发规则引擎**——规则引擎决定「转发什么、去哪」，本任务决定「值怎么算」。**不重复任务 15 的映射/换算/死区**（复用 15，且求值必须跑在 15 的单位换算**之后**）；**不定义质量码语义**（`calc_failed` 的枚举与「最差质量继承」规则由任务 53 定义，本任务只使用）。

  **What to do**:
  - **数据模型**：点位表支持 `point_type = physical | derived`；计算点字段：`name`、`expr`（表达式原文）、`output_type`（float32/float64/int64/bool）、`unit`、`deadband`、`on_failure`（`hold_last` / `null` / `skip`，默认 `hold_last`）、`eval_mode`（`on_change` 依赖变化触发 / `periodic` 固定周期）
  - **表达式引擎（核心，禁用脚本与 eval）**：
    - 实现**受限表达式引擎**（自研递归下降解析器或选型 `evalexpr` 一类库）：词法 → 语法 → **AST** → 编译期校验 → 求值
    - **点位引用语法**：`[point_id]`（方括号包裹，避免与运算符/函数括号产生歧义）；可引用物理点与其他计算点
    - **函数白名单**：数学 `abs ceil floor round clamp min max sqrt pow exp log log10`；条件 `if(cond,a,b)`；状态 `prev(x)`（上周期值）、`delta(x)`、`rate(x)`（单位时间变化率）、`hold(x,n)`、`quality(x)`（取质量码）
    - **禁用集（解析期即拒绝）**：赋值、循环、字符串操作、I/O、反射、动态求值、任意脚本（Lua/JS/SQL 片段）
    - **数值语义**：内部统一 f64 运算；输出按 `output_type` 显式转换；整数输出做范围检查（越界 → `calc_failed`）
    - **编译缓存**：表达式**编译一次并缓存 AST**，禁止每周期重新解析（须有测试证明解析只发生一次）
  - **依赖图与求值顺序**：
    - 解析每个计算点的引用 → 构建**依赖 DAG** → **拓扑排序**决定求值顺序
    - **环检测**：保存期即校验，报错**必须给出环路径**（如 `R_Cost → R_Energy → R_Cost`），不得只报「存在循环」
    - **周期屏障**：同一采集周期内**物理点（含单位换算）全部处理完成后**才执行公式求值；跨周期引用（`prev`/`delta`/`rate`）只读上周期快照，不读本周期未完成值
    - **跨设备引用**：允许引用其他设备点位；被引用设备离线或该点本周期未更新时按 `on_failure` 策略处理，**不得阻塞其他计算点求值**
  - **失败与质量**：
    - 输入缺失 / 超时 / 非数值 / 除零 / 结果 NaN·±Inf / 输出越界 → `quality = calc_failed`，并按 `on_failure` 处理（`hold_last` 保留上次有效值但**质量码保持 calc_failed**，不得伪装成 Good）
    - 多输入时**继承最差质量码**（口径依任务 53）
    - 失败计数与最近失败原因可查询（供监控页与诊断页展示）
  - **资源保护（防 DoS）**：表达式长度上限（建议 512 字符）、AST 节点数上限、单次求值超时（建议 5ms）、依赖链深度上限；超限即拒绝保存或标记 `calc_failed`
  - **API**：
    - `POST /api/points/formula/validate`（语法 + 白名单 + 引用存在性 + 环检测，返回结构化错误与 `cycle_path`）
    - `POST /api/points/formula/dry-run`（**权威试算**：给定输入值 → 返回计算结果、各依赖中间值、质量码）
    - 计算点 CRUD 复用 `/api/points`（`point_type` 区分）
  - **审计与版本**：公式新增/修改/删除写入审计日志（含**改前与改后全文**）；公式纳入配置版本与回滚（任务 55）
  - **产出《公式与计算点手册》**（`docs/`）：语法与运算符、**函数白名单与示例**、点位引用写法、依赖与环（含环路径示例）、失败策略与质量码语义、资源上限、**「历史不重算」说明**、常见错误与排查。**该手册是本项目公式能力的唯一语法来源**，任务 58 只做索引与引用、任务 71 只做 UI 呈现，均不得自行改写语法
  - **历史不重算**：明确实现为「公式变更只对新增数据生效」，并有测试断言改公式前后**历史查询结果不变**
  - 编写测试：语法错误、白名单拒绝、环检测（含多级环）、拓扑序正确性、除零/NaN 失败语义、三策略（hold_last/null/skip）、跨设备离线降级、AST 缓存计数、求值超时保护、历史不重算

  **Must NOT do**: 不要用 `eval` / 脚本引擎（Lua/JS/Python 嵌入）实现公式；不要把公式求值放进转发规则引擎（20/37），也不要在 15 里重复实现；不要跳过环检测保存，也不要只报「存在循环」而不给环路径；不要在每周期重新解析表达式；不要在公式失败时静默输出错误数值或伪造成 Good；不要实现公式变更后的历史重算（属 P2）；不要在此定义质量码语义（依任务 53）

  **Recommended Agent Profile**: Category: `deep` — 表达式引擎与依赖图
  **Parallelization**: Wave 7b（71 依赖本任务，二者串行）；**Blocked By**: 2（DataPoint schema）、4（配置管理）、15（点位模型与单位换算）、53（类型枚举与质量码规范）；**Blocks**: 71、65（公式列校验口径）、39（性能基线）、69

  **References**: 受限表达式引擎设计（AST + 白名单）；工业网关「虚拟点/计算点」常见模式（Neuron 运算插件、ThingsBoard calculated field）；任务 15 换算时序；任务 53 质量码；任务 55 配置版本
  **Acceptance Criteria**: `[ ] cargo test --package daemon formula PASS [ ] 语法/白名单/未知引用在保存期被拒且可定位 [ ] 环检测报错含环路径（含 A→B→C→A 多级环） [ ] 拓扑序求值正确 [ ] 除零/NaN/超时 → calc_failed 且 hold_last 不伪装 Good [ ] AST 缓存生效（解析调用 1 次） [ ] 长度/深度/超时保护生效 [ ] 公式变更写审计且历史数据不重算 [ ] 《公式与计算点手册》产出且手册内每个函数示例均可实际求值`
  **QA Scenarios**:
    - Happy: `[P_Inj] * [T_Barrel1] / 1000` → dry-run 与手算一致；落库后实时监控可见该点且带单位
    - Happy（拓扑序）: `R_C = [R_A] + [R_B]`，而 `R_A`、`R_B` 本身为计算点 → 断言同周期内 `R_A`/`R_B` 先于 `R_C` 求值
    - Happy（单位时序）: 物理点 Pa 经换算为 kPa 后参与公式 → 断言公式拿到的是**换算后**的值而非原始寄存器值
    - Error: `R_A=[R_B]+1` 且 `R_B=[R_A]+1` → 断言保存被拒且错误含环路径
    - Error: 表达式含赋值 / 循环 / 未白名单函数（如 `import`、`exec`）→ 断言解析期即拒绝并列出允许函数
    - Error: `[A]/[B]` 且 `[B]=0` → 断言 `calc_failed`；`hold_last` 下值保留但质量标记，`null` 下置空
    - Error: 被引用设备离线 → 断言该计算点按策略降级，**其他计算点仍正常求值**
    - Error: `pow(10, 1000000000)` → 断言触发上限保护且不拖垮采集周期
    - Error: 修改公式后查询历史数据 → 断言历史结果不变（未重算）
  **Evidence**: `.omo/evidence/task-70-formula-engine.log`（含环路径报错样本、dry-run 对照表、AST 缓存计数、超时保护记录）
  **Commit**: YES — `feat(points): restricted-expression formula engine for derived points`

- [ ] 71. 点位表公式编辑器 UI（类型列 / 表达式输入 / 依赖与环提示 / 后端试算 / 失败策略）

  **What to do**:
  - 按任务 63 设计稿与原型 `docs/design/prototype/gateway-console.html` 点位页实现：
    - 点位表新增**类型列**（物理点 / 计算点，Tag 视觉区分）；计算点行的**地址与字节序列置空**，公式列显示表达式摘要（超长截断 + 悬浮全文）
    - **公式编辑器**（抽屉或弹窗）：点位名称、输出类型、单位、**表达式输入框**（等宽字体、多行）、引用点位选择器（插入 `[point_id]`）、**函数面板**（白名单函数一键插入 + 用途说明）、失败策略（hold_last/null/skip）、死区
    - **实时提示区**：语法校验结果、解析出的依赖列表（可展示当前值）、**环检测结果（含环路径）**、错误定位到具体字符位
    - **后端试算区**：调用 `POST /api/points/formula/dry-run` 展示「输入值 → 结果 + 质量码」；**权威结果以后端为准**，前端即时提示仅作体验补充
    - **保存前必须过 `POST /api/points/formula/validate`**，不通过则保存按钮不可用
    - 列表中计算点与物理点**视觉可区分**（tag / 左侧色条），避免现场误认为物理点位
    - **公式变更影响提示**：「仅对新增数据生效，历史数据不重算」须在保存前可见
  - 编写测试：类型列渲染、编辑器校验（语法/白名单/环）、试算调用、保存拦截、依赖列表解析

  **Must NOT do**: 不要把前端本地求值结果当作权威（必须以 `dry-run` 返回为准，前端仅即时提示）；不要自造公式语法或函数名（以任务 70 规范为唯一来源）；不要让编辑器成为可绕过校验的入口；不要提供物理点「静默批量套用同一公式」的隐式操作（公式须逐点显式确认）

  **Recommended Agent Profile**: Category: `unspecified-high` — 表单与编辑器交互
  **Parallelization**: Wave 7b（在 70 之后）；**Blocked By**: 27（ui-kit）、28（点位表页面）、52（API 层）、63（设计稿）、70（引擎与 validate/dry-run API）；**Blocks**: 69

  **References**: `docs/design/ui-gateway-console.md` §3.3 与公式编辑器线框；`docs/design/prototype/gateway-console.html` 点位页；任务 70 的 validate / dry-run API
  **Acceptance Criteria**: `[ ] vitest PASS [ ] 计算点与物理点在列表中可区分（类型 Tag） [ ] 非法公式（语法/白名单/未知点位）行内拦截 [ ] 成环公式被拦下且显示环路径 [ ] 试算调用后端 dry-run 并展示结果与质量码 [ ] 保存不可绕过校验 [ ] 「历史不重算」提示可见`
  **QA Scenarios**:
    - Happy: 新建计算点 `[P_Inj] * [T_Barrel1] / 1000` → 断言依赖解析出 2 个点位、无环、试算返回数值、保存成功且列表出现「计算点」Tag
    - Error: 输入 `[A] +` → 断言行内语法错误且保存按钮禁用
    - Error: 输入非白名单函数（如 `eval("x")`）→ 断言被拒并提示允许的函数清单
    - Error: 构造 `R_A=[R_B]+1` / `R_B=[R_A]+1` 后保存 → 断言提示环路径 `R_A → R_B → R_A` 且保存被拒
    - Error: 断开 API → 断言试算显示「试算不可用」而非伪造结果
  **Evidence**: `.omo/evidence/task-71-formula-editor.log` + 编辑器截图
  **Commit**: YES — `feat(ui): formula editor for derived points`

- [ ] 72. 客户端界面 v2 终稿落地（方案 A）：设备接入向导 / 点位映射主从布局 / 列表分页契约

  **定位**: 本任务是**客户端界面 v2 的落地口径**，把已确认的可点击原型（`docs/design/prototype/gateway-v2a-glacier.html`，方案 A 已选定）转成真实实现，并**冻结三条交互契约**（单页步进向导、点位映射主从布局、列表分页单一计数口径）。与 27/28/65-68、71 的分工：27/28 提供 `ui-kit/` 与点位表页骨架，本任务提供**交互契约与页面结构终稿**；与 71 的分工：71 是公式编辑器（点位表的子功能），本任务是设备接入链路与点位页整体结构，**71 的编辑器须嵌入本任务的右面板**。

  **What to do**:
  - **实现基准**：`docs/design/prototype/gateway-v2a-glacier.html`（方案 A「冰川」）。B 版（`gateway-v2b-graphite.html`）未采用，仅作对比，**不得混用其视觉 token**
  - **① 新增设备 = 独立页面 + 单页四步步进**（不是一屏摊开，也不是四个页面）：
    - 四步：① 选择设备类型 → ② 连接参数 → ③ 点表映射 → ④ 测试并保存
    - **同一时刻只渲染当前步骤的区块**（DOM 中不得出现其它步骤的卡片）
    - 底部固定操作条：`取消` / `上一步` / `下一步`；**最后一步才出现保存按钮**（`仅保存连接参数` 与 `保存并开始采集`）
    - **步进约束**：只能「前进一步」或「回退到已到达的步骤」——**跳到未到达的步骤必须无效**（否则等于绕过必填项，校验形同虚设）
    - **步进不得丢已填值**：连接参数字段按 `字段 key` 暂存（命名空间建议 `proto:<协议名>::<key>`），并保证「前进→后退」「切协议→切回」都不丢
    - **末步保存门槛**：未勾选「我已逐点核对地址与字节序」时保存按钮 `disabled`，**且操作条必须写明原因**（灰着不解释不算合格）
    - 向导条支持点回退（已走过的步骤可点），未到达的步骤置锁
  - **② 连接参数字段随设备类型整块替换**（7 种协议各一套，**不使用一张万能表单**）：
    - `Modbus TCP`：设备 IP / 端口 / 从站号(Unit ID) / 超时 / 重试次数 / 并发请求数 / 功能码 / 最大合并间隔 / 地址偏移基数 / 默认字节序
    - `Modbus RTU`：串口设备 / 波特率 / 校验位 / 数据位 / 停止位 / 从站号 / 功能码 / 超时 / 重试次数 / 默认字节序
    - `Siemens S7`：设备 IP / 端口 / 机架号(Rack) / 槽号(Slot) / PDU 长度 / 超时 / DB 块访问方式（优化块）/ 默认字节序
    - `OPC UA`：Endpoint URL / 安全策略 / 认证方式 / 用户名 / 密码 / 默认命名空间索引 / 数据变更订阅 / 采样间隔 / 发布间隔
    - `Mitsubishi MC`：设备 IP / 端口 / 报文格式(3E/4E) / 网络号 / PC 号 / 站号 / 默认软元件 / 超时
    - `HTTP`：请求地址 / 请求方法 / 取数方式(轮询·Webhook) / 轮询间隔 / 认证方式 / Token / 取值表达式 / 响应编码 / 超时
    - `第三方 MQTT`：Broker 地址 / Client ID / 认证方式 / 用户名 / 密码 / 订阅 Topic / QoS / 载荷格式 / Clean Session / 保持连接间隔
    - 每个字段需带 `label / 类型 / 单位 / 是否必填 / hint（易错点）`；**点表模板的地址风格随协议变化**（S7 `DB1.0`、Modbus `40001`、OPC UA `ns=2;s=…`、MC `D100`、HTTP JSONPath、MQTT Topic）
    - 每种协议旁展示「最常见的三个坑」（文案依原型），字段清单与 hint 一并落进 `docs/design/ui-gateway-console.md`
  - **③ 点位映射 = 主从布局（左设备列表 / 右点表与操作）**：
    - **左列**：设备列表，支持**按分组筛选**（chip 由设备数据派生，含「全部」）与**关键词搜索**（名称 / 协议 / 地址 / 分组）
      - 搜索必须**只刷新列表**，不得整页重渲染——否则输入框焦点与光标每打一个字就丢
      - 无匹配时必须给空态与「清除筛选」出口
      - 列表项展示：设备名 / 协议 Tag / 地址 / 连接状态 / 点位数 / 「未配点表」标记
      - 页脚同处给出「总数 + 筛选后数量」（**不得在别处再写一遍台数**）
    - **右列**：选中设备的**点表信息与操作**——设备头（名称 / 协议 / 状态 / 分组 / 地址 / 采集频率 / 最后采集）+ 操作按钮（编辑设备 / 测试连接 / 导入点表）+ **映射链路**（设备 → 地址区间 → 点位 → 北向 target key）+ 点表信息（地址风格 / 点表来源 / 点位数量（含公式点数）/ 地址区间 / target 前缀）+ 点表（含 71 的公式编辑器入口）
    - **未配点表的设备 → 右侧空态引导**（导入 CSV/XLSX、使用该协议模板、手动新增），**绝不假装有数据**
    - 从设备列表页点「点位数」跳转时，左列必须自动选中该设备
  - **④ 全部主列表页统一分页**：设备 / 点位 / 告警 / 出口 / 规则 / 审计 / 账号 / 更新历史 / 备份
    - 控件：每页条数（5/10/20/50）、页码（多于 7 页折叠为 `1 … 4 5 6 … 20`）、上一页 / 下一页、跳至 N 页
    - **原则上由前端统一组件实现，不得逐页各写一套**；当前页「上一页」禁用、末页「下一页」禁用
    - **★ 计数单一口径**：分页条上的「共 N 条」一律取真实行数，**表格页脚不得再写一遍「共 N 条」**（两处数据源必然漂移）
    - 断言必须落在**同层比较**：当前页行数 vs 当前页相关状态，不得拿「全局计数」比「当前页行数」
  - **⑤ 单一数据源与不泄露内部标识**：
    - 设备清单为唯一来源，驱动设备表 / 左列表 / 映射链路 / KPI / 分组 chip；**点位数一律派生**（有点表取 `ptsOf(id).length`，无点表才是声明值），设备表点位数与真实点表长度必须一致（须有断言）
    - 「**计划点位数**」与「**已配表点位数**」是两个口径，KPI 必须同时给出（否则出现「点位总数 226 但只配了 24 个点」这类看起来像 bug 的数字）
    - 北向 target key 前缀用**对外标识**（如 `line1_m1`），**不得把内部设备 id（`d01`）暴露给客户**
  - **⑥ 渲染器纪律（本轮已踩过的坑，必须保持）**：未实现的区块类型必须「响亮失败」（渲染显眼哨兵而非空白）；渲染器返回「已渲染 HTML 字符串」的地方不得把结果再送回渲染函数；同一字段只允许一个来源
  - **⑦ 第四轮用户反馈（2026-09-23，已落在原型并验证）**：
    - **设备接入页精简**：只保留 KPI 统计 + 设备列表（删除映射图 / 映射检查 / 接入顺序建议 / 设备类型字段说明卡；映射维护只在「点位与映射」）
    - **设备列表行新增「实时数据」按钮** → 独立「实时数据」页：顶部设备 chips 可切换；KPI（点位数 / 采集频率 / 连接状态 / 曲线窗口）+ **每点位一张曲线卡**（数量 = 点表长度，标注当前值；未配点表给空态引导，不伪造曲线）
    - **北向转发新增出口支持「转发范围」**：`全部设备`（含未来新增）/ `指定设备`（12 台 chips 勾选，可切换可取消）；编码收敛为表单内 select，JSON 大整数约束压缩为一条 hint
    - **质量码对外展示一律中文**：良好 / 不确定 / 坏 / 模拟（规则 DSL 表达式 `quality != Bad` 豁免；须有全站英文质量码扫描断言）
    - **提示信息精简**：点位页删除与顶栏重复的模拟横幅、删除「模拟的边界与风险」卡；监控页删除与 desc 重复的节流 hint；向导各步 note 压缩；设备表默认每页 10 条
  - 把上述 ①②③④ 的**字段清单、交互契约、计数口径**写回 `docs/design/ui-gateway-console.md`（该文档为界面实现的唯一来源）

  **Must NOT do**: 不要一屏摊开四步向导，也不要拆成四个页面；不要允许跳到未到达的步骤（绕过必填）；不要让「下一步」落库（保存只能发生在最后一步）；不要在向导里用一张万能表单硬塞 7 种协议的字段；不要在页脚重复写「共 N 条」；不要把内部设备 id 暴露到北向字段名；不要让未配点表的设备显示假数据；不要给 A/B 两版各起一套 token 或组件（**共用同一份页面内容与引擎，只允许 CSS 与主题变量不同**）；不要在验证脚本里截图（验证会改脏配置，截图必须由独立的不改配置脚本产出）

  **Recommended Agent Profile**: Category: `unspecified-high` — 界面实现（前端交互 + 组件契约）
  **Parallelization**: Wave 7b（与 69/71 串行：本任务产出结构，71 在此结构内嵌编辑器，69 做最终验收）；**Blocked By**: 27（ui-kit）、28（点位表页骨架）、52（管理 API）、63（界面设计稿）、70/71（公式点与编辑器口径）；**Blocks**: 69
  **References**: `docs/design/prototype/gateway-v2a-glacier.html`（唯一实现基准）、`docs/design/ui-gateway-console.md`、`docs/design/ui-design-system.md`；本任务列出的 7 套协议字段清单与量化口径
  **Acceptance Criteria**:
    - [ ] 真实浏览器断言全通过（基线 **178 项**，仅方案 A 一版；方案 B 已停产出）
    - [ ] 第九轮反馈已落地：① 点表列表每行操作统一「公式 / 复制 / 编辑 / 删除」（非公式点也能点「公式」打开编辑弹窗，公式点主位「编辑公式」取消、由行内「公式」统一承担）；② 系统设置「模拟全局策略」从基础提取为独立页签「模拟策略」（第 2 位，含策略卡 + 模拟点速览卡，速览为派生计数）；③ 管理后台「租户与策略」新增「离线运行策略」卡（允许离线运行 radio + 允许离线时长 / 超期动作 select，随 Lease Token 下发、网关本地只读）；④ 北向转发右列「断网续传」上方新增「消息示例」卡：ts / device_name / device_mid / custom（设备自定义参数）/ data（带单位）/ data_raw（不带单位），大整数走字符串、quality 中文质量码
    - [ ] 第八轮反馈已落地：① 新增设备「设备 ID（默认时间戳字符串）/ 备注 / 自定义参数（key-value 可增删）」；② 北向转发支持 MQTT Broker / HTTP(S)+JSON（POST JSON，接口地址/认证 Header/批量大小）双类型出口；③ 点位表行操作「复制 / 编辑 / 删除」；④ 模拟总览页删除、全局模拟策略入「系统设置 › 基础」（第九轮再提取为「模拟策略」页签）；⑤ 贴牌（OEM）：系统设置 › 基础新增贴牌卡（启用开关 / 品牌名 / Logo 文字 / 副标题 / 登录页标语 / 主题色），侧栏品牌与 --accent 实时联动，关闭即还原；⑥ 页级按钮位置收敛：总览（删页级按钮+顶部横幅）/ 实时监控 / 实时数据 / 北向转发（删右上角按钮）、告警（规则/批量确认移列表标题行右侧、删底部提示块）、设备接入改名「设备列表」（新增设备移列表标题行右侧）、新增设备（删返回按钮+底部提示块）、点位与映射（设备状态后方=实时数据/编辑设备/新增点位/下载模板/导入点表/导出点表，列表下方按钮全删）、转发规则与账号列表（新增按钮移列表标题行右侧）
    - [ ] 第七轮反馈已落地：① 新增设备末步含「设备 ID（默认当前时间戳字符串，可更改，即北向 device_mid）/ 备注 / 自定义参数（key-value 行可增删）」；② 北向转发出口类型支持「MQTT Broker / HTTP(S) 接口（POST JSON，含接口地址/认证 Header/批量大小/断网续传）」，出口列表含 HTTP 示例行；③ 点位表行操作含「复制 / 编辑 / 删除」（删除走确认弹窗；复制生成名称副本；编辑为草稿弹窗）④ 模拟总览页删除，全局模拟策略迁入「系统设置 › 基础」，顶栏「模拟中 N 点」直达点位与映射页
    - [ ] 第六轮反馈已落地：① 点位与映射右面板的设备相关按钮（实时数据 / 编辑设备）紧跟设备状态之后，操作行只留点表相关操作（新增点位 / 下载模板 / 导入点表）；② 点表公式点可编辑——操作列「编辑公式」弹窗（表达式编辑 + 点位 chip 插入 + 引用检查[点表内给当前值 / 公式点拒绝 / 缺失标外部输入] + 草稿隔离），保存后公式展示在数据类型副行
    - [ ] 第五轮反馈已落地：① 模拟总览为独立页面（数据接入菜单）；② 点位与映射页无顶部提示/统计，左列表仅搜索框 + 固定高度 + 分页，右面板页签「点表 / 点表信息 / 映射链路 / 点位模拟」+「新增点位 / 下载模板」按钮；③ 全站不渲染页面标题 / 描述，操作按钮收进工具行，各页顶部提示横幅已删；④ 实时数据页用设备查询框（datalist）切换，不铺开设备 chips
    - [ ] 设备接入页只有统计与列表（无映射图 / 步骤指引）；设备行带「实时数据」按钮
    - [ ] 实时数据页：图表数 = 点表长度、可切设备、标注当前值；未配点表给空态
    - [ ] 北向新增出口：转发范围「全部设备 / 指定设备」可切换、指定模式 chips 勾选可增可减
    - [ ] 全站质量码为中文（规则 DSL 除外）
    - [ ] 「新增设备」为独立页 + 单页步进；同一时刻只渲染当前步骤；末步才出现保存且未确认时禁用 + 说明原因
    - [ ] 未到达的步骤跳转无效；前进→后退不丢已填值
    - [ ] 切协议字段整块替换（4 种以上协议逐条断言），点表模板地址风格随协议变化
    - [ ] 点位映射为空分左右两栏；左列按分组/关键词筛选生效且**输入焦点不被打断**；无匹配有空态出口
    - [ ] 未配点表的设备 → 右侧空态引导（非伪造数据）
    - [ ] 全部主列表页有分页；翻页为真实切片；页码高亮同步；每页条数生效；第 1 页上一页禁用
    - [ ] 分页条计数与表格行数同源（页脚无第二处「共 N 条」）
    - [ ] 无 JS 报错、无横向溢出、无 `undefined` / `[object Object]` / 未渲染区块
    - [ ] `ui-gateway-console.md` 已同步 7 套协议字段清单与三条交互契约
  **QA Scenarios**:
    - Happy: 新增设备 → 选 S7 → 第 2 步出现「机架号/槽号/DB 块访问方式」→ 第 3 步模板为 `DB1.0` → 第 4 步勾选核对后保存可用
    - Happy: 点位映射页按分组「注塑车间」筛选 → 左列 4 台；搜索「空压」→ 1 台（输入框仍保持焦点）
    - Happy: 点设备列表的「16 个」→ 跳到点位映射页且左列选中 1#注塑机、右列地址为 `DB1.0`
    - Error: 第 1 步直接点第 4 步 → 断言仍停在第 1 步
    - Error: 第 4 步未勾选核对就点保存 → 断言按钮禁用且操作条给出原因
    - Error: 选中未配点表的设备 → 断言右侧为空态引导，且**不含任何伪造点位行**
    - Error: 审计页翻到第 2 页 → 断言行内容与第 1 页不同（真实切片）、页码高亮为 2
    - Error: 打开搜索框输入后连打 3 个字符 → 断言 `document.activeElement` 始终是输入框
  **Evidence**: `.omo/evidence/v2-A-*.png`（24 张干净态截图：向导 4 步 + 4 份字段特写 + 映射 4 状态 + 分页 + 更新/自启/授权/模拟）+ 断言日志（203/203）
  **Commit**: YES — `feat(ui): gateway console v2 (device wizard, point mapping master-detail, list pagination)`

---

## Final Verification Wave

- [ ] F1. **计划合规审计** — `oracle`
  读取计划全文。校验 Must Have 实现（读文件/curl/运行命令）；校验 Must NOT Have（搜索禁止模式：私钥入库、客户端废弃入口、前端授权判定、隐写水印、本地离线激活）；校验证据文件存在；**校验 Wave 0 五份设计图存在于 `docs/design/` 且实现与设计一致（不一致处需有回改记录）**；比对交付物 vs 计划（含 `admin-console/`、`docs/`、`crates/licensing-server` 真实存在）。
  输出：`Must Have [N/N] | Must NOT Have [N/N] | Design Docs [N/N] | Tasks [N/N] | VERDICT: APPROVE/REJECT`

- [ ] F2. **代码质量审查** — `unspecified-high`
  `cargo test --workspace` + `cargo clippy` + `cargo fmt`；前端 `vitest` + `npm run build`（`web-console` 与 `admin-console` 各自构建）。检查 `as any`/`@ts-ignore`、空 catch、console.log、AI slop（过度注释/泛型命名）。
  输出：`Build [PASS/FAIL] | Lint [PASS/FAIL] | Tests [N/N] | VERDICT`

- [ ] F3. **真实手动 QA** — `unspecified-high`（UI 用 playwright）
  从零状态执行所有任务的 QA Scenarios；跨任务集成测试（多协议同时采集 + 断网续传 + 授权校验）；**授权专项端到端：一码一机（第二台被拒）→ 后台废弃 → 原设备降级停发 → 重发 → 新机激活成功 → 审计留痕；安装包篡改被拒；系统时间回拨不延长试用**；边界测试。保存到 `.omo/evidence/final-qa/`。
  输出：`Scenarios [N/N pass] | Integration [N/N] | Auth Flows [N/N] | Edge Cases [N tested] | VERDICT`

- [ ] F4. **范围保真检查** — `deep`
  每任务读"What to do" vs 实际 diff；验证 1:1（不遗漏不超建）；检查 Must NOT do 合规；检测跨任务污染（尤其 15/20/37 三处规则边界、17/18 分库边界、32/33 与 51 装配边界）；标记未记录变更。
  输出：`Tasks [N/N compliant] | Contamination [CLEAN/N issues] | Unaccounted [CLEAN/N files] | VERDICT`

---

## Commit Strategy
- `feat(scope): desc` — file.ts, npm test / cargo test
- 按 Wave 分批提交，每 Wave 一组 commit；**Wave 0 单独一组 `docs(design):` commit，且必须在用户确认设计图之前完成**
- 预提交：`cargo test` + `cargo deny check` + `vitest run` 通过
- ⚠️ 禁止把签名私钥、证书私钥、激活码明文、真实机器码提交进仓库

---

## Success Criteria

### 验证命令
```bash
cargo test --workspace                     # Expected: all pass
cargo clippy --workspace -- -D warnings    # Expected: no warnings
cargo deny check                           # Expected: licenses & advisories pass
cd tauri-shell && cargo tauri build        # Expected: 签名安装包产出（NSIS/MSI）
cd web-console && npm run test && npm run build     # Expected: vitest pass + dist
cd ui-kit && npm run test                           # Expected: 共享组件单测 pass
cd admin-console && npm run test && npm run build   # Expected: vitest pass + dist
bash deploy/docker/pack-offline.sh         # Expected: 多架构镜像 + offline tar.gz + SHA256SUMS
cosign verify --key cosign.pub <image>@<digest>     # Expected: 签名校验通过
cd deploy/docker && docker compose up -d && curl -I localhost:8080   # Expected: 200
strings -n 8 target/release/iot-daq | grep -cE 'api\.|secret|BEGIN (RSA|PRIVATE)'   # Expected: 0（Tier-1 字符串混淆生效）
cargo test --package daemon formula        # Expected: all pass（含环检测、失败语义、AST 缓存计数）
```
> ⚠️ `cargo tauri build` **不在 workspace 根执行**（Tauri 工程位于 `tauri-shell/`）；`vitest` 需在各自前端工程目录执行（`web-console/`、`admin-console/` 为两个独立前端）。
> ⚠️ `strings` 检查需在 **release** 产物上执行（debug 产物含符号，会误报）。

### 最终清单
- [x] **Wave 0 六份设计图 + 两份界面设计产出且经用户 explicit 确认**（`docs/design/`，含 `container-delivery.svg` 与 `ui-*.md` + `prototype/`）—— 2026-09-23 确认；SVG 由架构师按规格章节补绘归档
- [ ] 所有 Must Have 存在
- [ ] 所有 Must NOT Have 不存在
- [ ] 所有测试通过（Rust 全 workspace + 两个前端）
- [ ] 签名安装包产出（Windows NSIS/MSI + Linux AppImage/deb/rpm）
- [ ] **Linux Docker 交付通过**：多架构镜像 + 离线包 + 一键脚本；`cosign verify` 通过；compose 以 digest 固定；无外网现场可部署
- [ ] **容器反绕过通过**：同卷重建不重置试用、空卷重建仍被识别为同设备、跨宿主拷卷授权失效、容器内 machine-id 篡改无效
- [ ] **授权端到端通过**：一机一码绑定成功 / 第二台设备激活被拒 / 后台废弃 → 原设备降级停发 / 重发 → 新机激活成功 / 审计留痕
- [ ] **防破解验收通过**：安装包与镜像签名可校验、篡改被拒、删试用标记不重置、系统时间回拨不延长试用
- [ ] **防逆向 Tier-1 通过**：release 二进制已 strip + 关键常量混淆（`strings` 抽查无明文 endpoint/密钥）；段哈希自检可检出 patch；前端资源有完整性清单且**无授权判定逻辑与机密**；未引入 Tier-2/Tier-3 手段
- [ ] **二次校验三档通过**：A 档重放/超窗/篡改被拒；**B 档（默认）回执无业务数值、断网仍持续转发、跳空可告警**；C 档仅心跳
- [ ] **北向双编码通过**：每路出口可独立选 protobuf/json；超 2^53−1 整数以字符串输出且无精度丢失；两种编码**语义一致、验签结果相同**；JSON 性能基线与推荐设备上限已产出
- [ ] **界面交付通过**：客户端 14 页 + 后台 10 页与设计稿一致；两端共用 `ui-kit/` 无视觉漂移；实时监控 1s 节流生效；点位表导入导出闭环可用（导出→改→导入 0 错误）；**客户端可点元素扫描无解绑/重置试用入口**；三角色 RBAC 可见性与 403 行为符合设计
- [ ] **公式计算通过**：计算点按依赖拓扑序求值且物理点先算完（周期屏障）；公式拿到的是**换算后**的工程单位值；成环公式（含跨行成环）被拒并给出**环路径**；除零/NaN/输入超时 → `calc_failed` 且 hold_last 不伪装 Good；AST 已编译缓存（重复周期解析 1 次）；长度/深度/超时保护生效（`pow(10,10^9)` 不拖垮采集）
- [ ] **公式安全通过**：无 `eval` / 脚本引擎；非白名单函数与赋值/循环在解析期被拒；前端编辑器**不可绕过校验**（`validate` 不通过无法保存；试算结果来自后端 `dry-run`）
- [ ] **公式可追溯**：公式变更写入审计（含改前/改后）；`docs/` 下《公式与计算点手册》齐全并已被索引；改公式后**历史数据不变**（未重算）
- [ ] 200 设备完成一轮验收；50 设备 × 100ms 无丢数据；断网 1h 补发 1,800,000 条且无重复
