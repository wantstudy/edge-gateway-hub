# IoT-DAQ Gateway — 工业边缘数据采集网关

**IoT-DAQ Gateway** 是一款面向工业现场的边缘数据采集网关：从 PLC / 仪表采集数据，本地缓存与规则计算，并转发到 MQTT 北向平台。本仓库提供 Windows 安装包与 Docker 镜像两种交付形态。

## 功能特性

- **多协议采集**：Modbus TCP/RTU、S7、FINS、MC、ADS、EtherNet/IP、OPC UA、HTTP 等 8 类协议
- **HTTP 脚本扩展**：前置脚本登录换取 Token、后置脚本解析响应赋值点表；全局变量按设备跨采集周期缓存；纯 Rust JS 引擎，无外部运行时依赖
- **北向转发**：MQTT（v3.1.1 / v5），每路独立 protobuf / JSON 编码
- **本地韧性**：断网续传（SQLite 队列）、背压控制
- **历史数据**：本地加密时序库，按时间/点位分页查询，图表与表格双视图；密钥轮换后旧行逐行隔离降级，不整页报错
- **规则与公式**：点位级计算、告警规则
- **Web 控制台**：设备 / 点位 / 转发 / 告警 / 审计全功能管理
- **安全**：最小权限容器（非 root、只读根、cap_drop ALL）

## 下载

| 交付物 | 说明 |
|---|---|
| `IoT-DAQ-Gateway_0.2.7_x64-setup.exe` | Windows 10/11 x64 安装包（约 8.7MB） |
| Docker 镜像 | `wantstudy/iot-daq-gateway:latest`（linux/amd64，拉取：`docker pull wantstudy/iot-daq-gateway:latest`） |

---

## 快速开始 A：Windows 安装包（推荐）

1. 下载 `IoT-DAQ-Gateway_0.2.7_x64-setup.exe`
2. 双击安装（需要 WebView2 Runtime，Win10/11 通常已内置）
3. 从开始菜单启动 **IoT-DAQ Gateway**，桌面控制台自动打开（或直接访问 `http://127.0.0.1:29105`；端口被占用时会自动顺延，实际地址见控制台顶栏）
4. 安装版会以服务方式常驻并开机自启；卸载走「设置 → 应用」或安装目录自带卸载器

---

## 快速开始 B：Docker（Linux amd64）

### 一键部署

```bash
# 拉取镜像（Docker Hub）
docker pull wantstudy/iot-daq-gateway:latest

docker run -d \
--name edge-gateway-hub \
--network host \
--restart=always \
-v /data/edge-gateway:/var/lib/iot-daq:rw \
-v /etc/machine-id:/host/etc/machine-id:ro \
-v /sys/class/dmi/id:/host/sys/class/dmi/id:ro \
-e TZ=Asia/Shanghai \
wantstudy/iot-daq-gateway:latest
```

> 镜像使用滚动标签 `latest`，每次拉取都是当前最新版；需要锁定版本时把 `latest` 换成具体版本号（例如 `wantstudy/iot-daq-gateway:0.2.3`）。**容器镜像与 Windows 安装包独立出包、版本可能不同步**——以 Docker Hub 上实际存在的标签为准，可用 `docker pull wantstudy/iot-daq-gateway:<tag>` 验证。

> 机器码自动取自宿主机（machine-id / DMI）；指纹密钥与授权状态随 `/data/edge-gateway` 持久化，首次启动自动生成，无需手工初始化。授权服务地址已内置镜像，无需任何环境变量。

### 查看状态

```bash
docker logs -f edge-gateway-hub  # 看到 "bootstrap: daemon running" 即成功
```

### Web 控制台地址

- **Web 控制台**：`http://<宿主IP>:29205`
- **管理 API**：`http://<宿主IP>:29105`

端口是 IoT-DAQ 专用段默认值；被宿主进程占用时会自动 +1 顺延（上限 10 个），可用 `IOT_DAQ_WEB_PORT` / `IOT_DAQ_HTTP_PORT` / `IOT_DAQ_PORT_PROBES` 覆盖。顺延后的实际端口写入容器内 `/run/iot-daq-ports.env`，容器 healthcheck 会自动跟随。

---

## 配置

v2 起配置的唯一真相源是 **SQLite**（`/var/lib/iot-daq/data/iot-daq.db`），**不再读取也不再生成 `gateway.toml` / `config.toml`**。挂载 `config/` 目录不再必要；设备、点位、北向转发、告警规则全部在 Web 控制台完成配置并直接落库。

持久卷里真正需要的只有数据根：

| 卷挂载 | 内容 |
|---|---|
| `/data/edge-gateway` → `/var/lib/iot-daq` | SQLite 配置库、断网队列、历史时序库、审计日志、授权/试用状态、指纹密钥 |

删除持久卷等同于重置试用与全部配置。

## 串口采集（可选）

Modbus RTU 等串口场景：编辑 `docker/docker-compose.yml`，取消 `devices` / `group_add` 两段注释并确认节点存在（如 `/dev/ttyUSB0`），然后 `docker compose up -d` 重建。

## 常见问题

**Q: Web 控制台打不开？**
`curl http://127.0.0.1:29105/api/overview` 有 JSON 返回说明网关正常，检查 29205 端口与防火墙；若改过端口或被顺延，先 `docker exec edge-gateway-hub cat /run/iot-daq-ports.env` 取实际端口。

**Q: Docker 里怎么确认机器码取的是宿主机？**
容器内锚点已被关闭，机器码只可能来自宿主机 machine-id / DMI / MAC。

**Q: 升级？**
下载新版本交付物，替换 `.env` 中 `IOT_DAQ_IMAGE` 版本号（Windows 直接跑新安装包覆盖安装）。**不要**删除持久卷/数据目录——授权与数据都在里面。

## 版本

- 当前版本：**v0.2.7**（Windows 安装包）
- 反馈问题请提 [Issues](https://github.com/wantstudy/edge-gateway-hub/issues)。

## 更新日志

### v0.2.7

- **修复「检查更新」报「请求失败（HTTP 0）」**：升级包下载在慢网络下可能耗时十几秒以上，超过浏览器 15 秒默认超时，前端就报「请求失败（HTTP 0）」，但后台其实仍在正常下载——出现「报失败、升级却已完成」的假失败。现在「检查更新」点击后立即返回「正在后台下载 X MB」的进度状态，不再等下载完成；下载完成后自动收敛为「有新版本可安装」，用户无需反复点检查。
- **升级检查独立超时**：升级检查走独立的 30 秒超时（其他接口仍为 15 秒），慢网络下不再误报超时；升级包仍在后台下载时界面会提示等待，而不是错误地显示失败。

### v0.2.6

- **在线升级修复**：授权端与网关不在同一前缀下（例如授权端经反向代理部署在 `/licensing` 之后）时，网关解析升级包下载地址会落到网关自己的前端页面，于是「下载回来的不是安装包」。现在直接按授权端给出的相对地址推导同级下载地址并逐个候选重试，不会再被自己的页面劫持；已用生产授权端端到端验证，升级包可完整下载并通过验签。
- **可视化大屏支持全屏**：大屏页新增全屏按钮，运行时不支持全屏或请求被拒绝时如实提示，不出现假全屏状态。
- **修复大屏迷你趋势图折线不显示**：折线的模板绑定写法有误（`points=` 应为 `:points=`），导致画布上报错且折线画不出来。

### v0.2.5

- **升级安装不再弹「已安装」提示页**：直接覆盖安装、静默卸载旧版后继续，全程无需选择。已安装的版本会被自动识别并原地升级，用户数据（授权、指纹密钥、历史数据）保持不变。
- **网关起不来时给出真实原因并支持一键重试**：此前 daemon 启动失败后守护会进入启动失败保护并停止重试，界面只显示「网关未启动」，既看不到原因也无法恢复。现在登录页会直接显示 daemon 最近一次的真实失败日志，并提供「重新启动网关」按钮立即重试；系统设置页同步展示失败原因。

### v0.2.4

- **修复卸载后重装无法启动**：卸载程序会清空整个数据目录树，重装后首次启动时 SQLite 库的父目录不存在，网关直接报 `unable to open database file` 退出。现在数据目录在拉起服务前就地补建（桌面壳 + 服务端双重保险），重装即可正常启动。
- **不再生成配置文件**：`config.toml` 已彻底移除，其中全部参数固化为代码内置默认值。设置页的「配置备份」会明确提示配置已落 SQLite，并指向数据库级备份接口。
- **端口改为专用段并支持自动顺延**：管理 API 默认 **29105**、Web 控制台默认 **29205**（避开 `8080` 等常用端口）。任一端口被宿主进程占用时自动 +1 顺延，上限 10 个；桌面端、Docker 端均支持，顺延结果会自动同步给健康检查与前端连接地址。

### v0.2.3

- **安全加固：未授权时北向 MQTT 完全静默**。授权缺失（免费版降级 / 已停用 / 授权尚未装配）时，网关不再向第三方 MQTT broker 发布任何控制写报文——拒绝发生在 TCP 握手之前，并返回可解释的原因与恢复路径；南向写面向本机现场设备，不受该门控影响。
- **系统更新（OTA）修复**：配置源切到 SQLite 后，旧版落盘的「在线更新未启用」不再永久卡住；容器场景的 `IOT_DAQ_OTA_*` 环境变量覆盖同步恢复生效。在线更新拆为「检查清单 + 下载地址」双接口，旧版客户端保持兼容。

### v0.2.2

- 与 v0.2.1 功能一致，同步代码版本。

### v0.2.1

- **HTTP 设备前置 / 后置脚本**：内置纯 Rust JS 引擎，前置脚本在主请求之前登录换取 Token，后置脚本解析响应并赋值点表；全局变量 `vars` 按设备跨采集周期缓存，控制台内置脚本工具栏与语法速查。
- **自定义请求头**：`Name:Value` 分号分隔的静态配置，也可由脚本改写；认证方式支持 无 / Bearer / Basic，非法组合（如未知字面量、Bearer 缺 token）在保存时即报错而非静默降级。
- **修复历史数据接口 500**：数据行解密失败改为逐行隔离并计数，不再因单行损坏导致整页不可用；历史数据默认以图表视图展示。

### v0.2.0

- 历史数据分页与图表视图、公式点位当前值跨组可见、授权地址不入库、重启后遥测续采。

## License

[Apache License 2.0](./LICENSE)
