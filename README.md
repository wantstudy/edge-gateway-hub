# IoT-DAQ Gateway — 工业边缘数据采集网关

**IoT-DAQ Gateway** 是一款面向工业现场的边缘数据采集网关：从 PLC / 仪表采集数据，本地缓存与规则计算，并转发到 MQTT 北向平台。本仓库只分发**网关端**运行产物（Windows 安装包 + Docker 镜像），开箱即用。

> 本仓库**不包含授权端**。激活 / 授权 / 系统更新由厂商云端授权服务托管（`http://license.webscad.cn/licensing`），用户无需自行部署任何服务端组件。

## 功能特性

- **多协议采集**：Modbus TCP / RTU、OPC UA（更多协议持续增加）
- **北向转发**：MQTT（v3.1.1 / v5），每路独立 protobuf / JSON 编码
- **本地韧性**：断网续传（SQLite 队列，断网 1 小时 @500 条/s 无丢失）、背压控制
- **规则与公式**：点位级计算、告警规则
- **Web 控制台**：设备 / 点位 / 转发 / 告警 / 审计全功能管理
- **安全**：一机一码激活、授权判定全在本地 Rust 侧、最小权限容器（非 root、只读根、cap_drop ALL）

## 下载

| 交付物 | 说明 |
|---|---|
| [`IoT-DAQ-Gateway_0.1.0_x64-setup.exe`](https://github.com/wantstudy/edge-gateway-hub/raw/main/downloads/IoT-DAQ-Gateway_0.1.0_x64-setup.exe) | Windows 10/11 x64 安装包（NSIS，含 daemon 与桌面控制台） |
| **Docker 镜像** | `docker pull wantstudy/iot-daq-gateway:0.1.0`（linux/amd64，93.8MB 源镜像） |

---

## 快速开始 A：Windows 安装包（推荐）

1. 下载 [`IoT-DAQ-Gateway_0.1.0_x64-setup.exe`](https://github.com/wantstudy/edge-gateway-hub/raw/main/downloads/IoT-DAQ-Gateway_0.1.0_x64-setup.exe)。
2. 双击安装（需要 WebView2 Runtime，Win10/11 通常已内置；缺失时安装器会引导）。
3. 从开始菜单启动 **IoT-DAQ Gateway**，桌面控制台自动打开（也可直接访问 `http://127.0.0.1:8080`）。
4. 首次启动自动进入 **3 天试用**，无需注册。控制台「授权」页显示**机器码**，把它发给厂商即可获取正式激活码。

> 安装版会以服务方式常驻并开机自启；卸载走「设置 → 应用」或安装目录自带卸载器。

## 快速开始 B：Docker（Linux amd64）

### 1. 拉取镜像

```bash
docker pull wantstudy/iot-daq-gateway:0.1.0
# → 93.8MB 源镜像，约 26MB 最终容器体积
```

### 2. 准备宿主目录

```bash
sudo mkdir -p /opt/iot-daq/gateway/data /opt/iot-daq/gateway/secrets

# 指纹 HMAC 盐：首次安装生成一次，之后【禁止改动或删除】
sudo head -c 32 /dev/urandom | sudo tee /opt/iot-daq/gateway/secrets/iot-daq-fingerprint-key >/dev/null
sudo chown -R 65532:65532 /opt/iot-daq/gateway/data
sudo chown 65532:65532 /opt/iot-daq/gateway/secrets/iot-daq-fingerprint-key
sudo chmod 600 /opt/iot-daq/gateway/secrets/iot-daq-fingerprint-key
```

### 3. 启动

```bash
cd docker
cp .env.example .env        # 按需修改（默认即可跑通）
docker compose up -d
docker logs -f iot-daq-gateway
```

看到 `bootstrap: daemon running` 即启动成功：

- **Web 控制台**：`http://<宿主IP>:9012`
- **管理 API**：`http://<宿主IP>:9011`

> 端口冲突？`.env` 里改 `IOT_DAQ_HTTP_PORT` / `IOT_DAQ_WEB_PORT` 即可（避免占用 80/443 等既有服务端口）。

### 串口采集（可选）

Modbus RTU 等串口场景：编辑 `docker/docker-compose.yml`，取消 `devices` / `group_add` 两段注释并确认节点存在（如 `/dev/ttyUSB0`），然后 `docker compose up -d` 重建。

---

## 激活说明（一机一码）

| 状态 | 行为 |
|---|---|
| **试用** | 首次启动自动获得 **3 天**全功能试用，倒计时见控制台「授权」页 |
| **试用到期** | 自动降级**免费版**（**不会**停止数据采集），北向转发受限 |
| **正式激活** | 控制台「授权」页复制**机器码** → 发给厂商 → 输入激活码，立即生效 |

- 机器码由**宿主机**锚点生成（machine-id / DMI / MAC），Docker 重建容器、Windows 重装系统**之外**的任何操作都不会改变机器码。
- 授权状态与数据全部落在持久卷（Windows：安装目录 `data\`；Docker：`/opt/iot-daq/gateway/data`）。**删除该目录 = 重置试用并丢失授权与历史数据。**
- 网关需要能访问 `http://license.webscad.cn`（HTTP 80）完成激活与心跳；离线宽限期 **7 天**，期间按最后已知授权状态运行。

## 配置

容器把 `data/config/` 挂载为 `/etc/iot-daq`，网关读取其中的 `gateway.toml`。**最小配置即可启动**（文件可以只有注释），设备、点位、北向转发、告警规则全部在 Web 控制台完成配置，无需手改文件。

```toml
# /opt/iot-daq/gateway/data/config/gateway.toml —— 最小占位
# 网关名（可选，默认显示为未命名网关）
# [gateway]
# name = "plant-01-gw"
```

## 常见问题

**Q: Web 控制台打不开？**
`curl http://127.0.0.1:9011/api/overview` 有 JSON 返回说明网关正常，检查 9012 端口与防火墙；若 `.env` 改过端口，访问端口要跟着改。

**Q: 激活失败 / 一直显示试用？**
确认网关能出网访问 `license.webscad.cn`（`curl -s http://license.webscad.cn/licensing/updates/manifest` 应返回 200）；检查控制台「授权」页的错误信息。

**Q: Docker 里怎么确认机器码取的是宿主机？**
控制台「授权」页查看机器码来源标注；容器内锚点已被 `IOT_DAQ_ALLOW_CONTAINER_ANCHORS=0` 硬性关闭，机器码只可能来自宿主机 machine-id / DMI / MAC。

**Q: 升级？**
下载新版本交付物，替换 `.env` 中 `IOT_DAQ_IMAGE` 版本号（Windows 直接跑新安装包覆盖安装）。**不要**删除持久卷/数据目录——授权与数据都在里面。

## 版本

- 当前版本：**v0.1.0**
- 反馈问题请提 [Issues](https://github.com/wantstudy/edge-gateway-hub/issues)。

## License

[Apache License 2.0](./LICENSE)
