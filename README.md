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
| `IoT-DAQ-Gateway_0.2.3_x64-setup.exe` | Windows 10/11 x64 安装包（约 8.3MB） |
| Docker 镜像 | `wantstudy/iot-daq-gateway:latest`（linux/amd64，拉取：`docker pull wantstudy/iot-daq-gateway:latest`） |

---

## 快速开始 A：Windows 安装包（推荐）

1. 下载 `IoT-DAQ-Gateway_0.2.3_x64-setup.exe`
2. 双击安装（需要 WebView2 Runtime，Win10/11 通常已内置）
3. 从开始菜单启动 **IoT-DAQ Gateway**，桌面控制台自动打开（或直接访问 `http://127.0.0.1:8080`）
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

> 镜像同时发布固定版本标签（如 `0.2.3`）与滚动标签 `latest`：上面示例使用 `latest`，每次拉取都是当前最新版；需要锁定版本时把 `latest` 换成具体版本号，例如 `wantstudy/iot-daq-gateway:0.2.3`。

> 机器码自动取自宿主机（machine-id / DMI）；指纹密钥与授权状态随 `/data/edge-gateway` 持久化，首次启动自动生成，无需手工初始化。授权服务地址已内置镜像，无需任何环境变量。

### 查看状态

```bash
docker logs -f edge-gateway-hub  # 看到 "bootstrap: daemon running" 即成功
```

### Web 控制台地址

- **Web 控制台**：`http://<宿主IP>:9012`
- **管理 API**：`http://<宿主IP>:9011`

---

## 配置

容器把 `/opt/iot-daq/gateway/data/config/` 挂载为 `/etc/iot-daq`，网关读取其中的 `gateway.toml`。**最小配置即可启动**（文件可以只有注释），设备、点位、北向转发、告警规则全部在 Web 控制台完成配置。

```toml
# /opt/iot-daq/gateway/data/config/gateway.toml —— 最小占位
# 网关名（可选）
# [gateway]
# name = "plant-01-gw"
```

## 串口采集（可选）

Modbus RTU 等串口场景：编辑 `docker/docker-compose.yml`，取消 `devices` / `group_add` 两段注释并确认节点存在（如 `/dev/ttyUSB0`），然后 `docker compose up -d` 重建。

## 常见问题

**Q: Web 控制台打不开？**
`curl http://127.0.0.1:9011/api/overview` 有 JSON 返回说明网关正常，检查 9012 端口与防火墙；若 `.env` 改过端口，访问端口要跟着改。

**Q: Docker 里怎么确认机器码取的是宿主机？**
容器内锚点已被关闭，机器码只可能来自宿主机 machine-id / DMI / MAC。

**Q: 升级？**
下载新版本交付物，替换 `.env` 中 `IOT_DAQ_IMAGE` 版本号（Windows 直接跑新安装包覆盖安装）。**不要**删除持久卷/数据目录——授权与数据都在里面。

## 版本

- 当前版本：**v0.2.3**
- 反馈问题请提 [Issues](https://github.com/wantstudy/edge-gateway-hub/issues)。

## 更新日志

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
