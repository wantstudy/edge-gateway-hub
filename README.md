# IoT-DAQ Gateway — 工业边缘数据采集网关

**IoT-DAQ Gateway** 是一款面向工业现场的边缘数据采集网关：从 PLC / 仪表采集数据，本地缓存与规则计算，并转发到 MQTT 北向平台。本仓库提供 Windows 安装包与 Docker 镜像两种交付形态。

## 功能特性

- **多协议采集**：Modbus TCP / RTU、OPC UA
- **北向转发**：MQTT（v3.1.1 / v5），每路独立 protobuf / JSON 编码
- **本地韧性**：断网续传（SQLite 队列）、背压控制
- **规则与公式**：点位级计算、告警规则
- **Web 控制台**：设备 / 点位 / 转发 / 告警 / 审计全功能管理
- **安全**：最小权限容器（非 root、只读根、cap_drop ALL）

## 下载

| 交付物 | 说明 |
|---|---|
| `IoT-DAQ-Gateway_0.1.0_x64-setup.exe` | Windows 10/11 x64 安装包（约 5.3MB） |
| Docker 镜像 | `docker pull wantstudy/iot-daq-gateway:0.1.0`（linux/amd64，93.8MB） |

---

## 快速开始 A：Windows 安装包（推荐）

1. 下载 `IoT-DAQ-Gateway_0.1.0_x64-setup.exe`
2. 双击安装（需要 WebView2 Runtime，Win10/11 通常已内置）
3. 从开始菜单启动 **IoT-DAQ Gateway**，桌面控制台自动打开（或直接访问 `http://127.0.0.1:8080`）
4. 安装版会以服务方式常驻并开机自启；卸载走「设置 → 应用」或安装目录自带卸载器

---

## 快速开始 B：Docker（Linux amd64）

### 一键部署（三行搞定）

```bash
# 1. 导入镜像
docker pull wantstudy/iot-daq-gateway:0.1.0

# 2. 初始化（创建目录、生成密钥、启动容器）
bash -c 'sudo mkdir -p /opt/iot-daq/gateway/{data,secrets} && sudo head -c 32 /dev/urandom | sudo tee /opt/iot-daq/gateway/secrets/iot-daq-fingerprint-key >/dev/null && sudo chown -R 65532:65532 /opt/iot-daq/gateway/data /opt/iot-daq/gateway/secrets && sudo chmod 600 /opt/iot-daq/gateway/secrets/iot-daq-fingerprint-key'

# 3. 启动（从 docker/ 目录执行）
cd docker && docker compose up -d
```

### 查看状态

```bash
docker logs -f iot-daq-gateway  # 看到 "bootstrap: daemon running" 即成功
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

- 当前版本：**v0.1.0**
- 反馈问题请提 [Issues](https://github.com/wantstudy/edge-gateway-hub/issues)。

## License

[Apache License 2.0](./LICENSE)
