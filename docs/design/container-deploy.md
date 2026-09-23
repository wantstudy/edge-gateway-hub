# 容器部署设计（task 59 · 补充设计）

> 归属：`iot-daq` Wave 0 设计基线 · 计划任务 59（容器化交付）· 2026-09-23 定稿
> 前置阅读：`container-delivery.svg`、`container-machine-binding.md`（宿主锚点与挂载命令）、`container-persistence-layout.md`（卷布局）、`container-supply-chain.md`（镜像校验链）

## 1. 目标与范围

定义 iot-daq 网关容器在客户现场 Linux 宿主上的**运行参数、设备接入、离线安装流程**。设计基调：最小权限容器——只开声明过的设备节点与网络能力，不用 `--privileged`，不挂 `docker.sock`；全部参数以随包 `docker-compose.yml` 为唯一事实源，现场人工零编辑。

## 2. 设备接入与运行参数

### 2.1 串口（南向 Modbus RTU / RS-485）

```yaml
devices:
  - /dev/ttyUSB0      # 按现场实际枚举调整，声明几个开几个
  - /dev/ttyS0
group_add:
  - dialout           # 容器内进程以 dialout 组获得读写权限
```

- 原理：`--device` 精确放行指定设备节点 cgroup，`group_add dialout` 匹配宿主串口节点组属主（常见 `crw-rw---- root dialout`）——**替代** privileged 全开；
- 采集脚本随包提供 `deploy/detect-serial.sh`，枚举 `/dev/ttyUSB* /dev/ttyACM* /dev/ttyS*` 并生成建议配置行；
- 现场换口（拔插顺序变化导致 ttyUSB 编号漂移）时，手册建议用 `/dev/serial/by-id/` 稳定路径替换。

### 2.2 网络（`--network host`）

```yaml
network_mode: host
```

- **理由**：Modbus TCP 广播发现、部分南向设备 UDP 组播、北向平台回调地址简单化——host 网络下容器与宿主共享协议栈，广播/组播不经 NAT 失真；
- 管理界面端口：host 网络下应用直接监听 `0.0.0.0:8080`（Web 控制台）；建议文档化「仅绑定内网网卡/配合防火墙放行」而非在 compose 内改绑，保持默认参数零编辑；
- 风险缓解：容器内进程仅监听业务端口（8080 管理面 + 协议端口），不新增监听；宿主防火墙（ufw/firewalld）策略由现场手册给出模板；
- 放弃 bridge+port-mapping 方案的原因：广播发现经 docker-proxy 不可达，属已知坑。

### 2.3 资源限制与重启策略

```yaml
mem_limit: 512m
cpus: 1.0
restart: unless-stopped
```

- `--memory 512m --cpus 1.0`：采集进程异常（死循环、内存泄漏）时被内核约束，不拖垮宿主；限值随设备规模在厂商发布说明中给出推荐区间（≤8 设备用默认值）；
- `restart: unless-stopped`：现场断电/宿主重启后容器自愈拉起；与 `always` 的区别——人工 `docker stop` 后不违反运维意图；
- 租约状态不受容器重启影响：心跳计时基于持久卷内的本地时间戳与上次服务器响应（见 `activation-rules.md`），重启只是恢复，不重置宽限。

### 2.4 日志驱动

```yaml
logging:
  driver: json-file
  options:
    max-size: "20m"
    max-file: "5"
```

- 防容器日志写爆宿主盘（工业现场无人值守）；上限约 100MB 滚动；
- **审计/授权相关日志不走 stdout**：落持久卷 `logs/`（task 44 审计回执、授权判定记录），滚动策略由应用自身管理，避免被日志驱动截断丢失。

## 3. 离线现场一键安装流程

前置：已按 `container-supply-chain.md` §7 完成 `verify.sh`（哈希 + load + cosign 验签）。

```
步骤                          动作                                     产物/判据
─────────────────────────────────────────────────────────────────────────────
① 校验与导入                 verify.sh（已含 sha256 -c + docker load  "OK: 供应链校验通过"
                              + cosign verify）                        镜像已在本地
② 采集宿主指纹               host-fingerprint-collect.sh              /var/lib/iot-daq/
                              （machine-id/product_uuid/MAC 三锚点，   host-fingerprint.json
                              HMAC 签名；详见 machine-binding.md）
③ 首次启动                   docker compose up -d                     容器 Running
④ 健康检查                   见 §3.1 判据                             两项全绿 → 交维
⑤ 记录安装回执               采集 docker inspect 摘要 + 指纹指纹值     现场安装单（纸/表）
```

### 3.1 健康检查判据

1. `docker compose ps` 显示 gateway `running`（`restart: unless-stopped` 下观察 60s 无重启抖动）；
2. 宿主 `curl -fsS http://127.0.0.1:8080/healthz` 返回 200，且 body 中 `mode` 为预期值（新装=未激活/试用中）；
3. 未通过时：`docker compose logs --tail=200` 排查；**禁止**以改参数方式绕过（如删指纹文件重生成——会被 task 43 自检判为环境变更）。

### 3.2 失败处理原则

- 指纹注入失败（锚点缺失且采集脚本降级签名失败）→ 停止安装，回厂核对宿主型号，不在现场造数据；
- 健康检查失败且日志指向授权模块 → 按手册导出脱敏日志（含 lease 诊断位，不含私钥/业务数据）联系厂商。

## 4. 完整 compose 参考片段（随包唯一事实源）

```yaml
services:
  gateway:
    image: iot-daq/iot-daq-gateway@sha256:<digest>       # CI 注入，见 supply-chain §6
    network_mode: host
    devices:
      - /dev/ttyUSB0
    group_add:
      - dialout
    mem_limit: 512m
    cpus: 1.0
    restart: unless-stopped
    logging:
      driver: json-file
      options: { max-size: "20m", max-file: "5" }
    volumes:
      - /var/lib/iot-daq:/var/lib/iot-daq                 # 唯一持久根，见 persistence-layout
      - /etc/machine-id:/etc/machine-id:ro                # 宿主只读锚点，见 machine-binding
      - /sys/class/dmi/id/product_uuid:/sys/class/dmi/id/product_uuid:ro
    read_only: false
    security_opt:
      - no-new-privileges:true
```

说明：`no-new-privileges:true` 阻断容器内提权；容器内 root 仅为读取串口/挂载点的必要形态，应用进程自身降权运行（镜像内 USER 指令 + dialout 组）。

## 5. 明确不做清单（红线五条）

| # | 不做 | 替代/理由 |
|---|------|----------|
| 1 | 不做 Kubernetes / Helm（首期） | 单机网关场景，compose 已覆盖；K8s 化留待多站点集中管控需求出现后评估 |
| 2 | 不做镜像内自更新 | OTA 走 task 35 应用层更新（应用下载新版本文件，持久卷内落位，宿主侧脚本替换容器），容器镜像保持不可变 |
| 3 | 不挂载 `/var/run/docker.sock` | 容器获得宿主 Docker 控制权 = 变相 root，且破坏「容器不知道自己是容器」的指纹原则 |
| 4 | 不用 `--privileged` | 全开能力面远超业务所需；设备/网络能力均已按 §2 最小化声明 |
| 5 | 不做 Windows 容器 | Windows 环境走 Tauri 原生安装包（同一 Rust 数据面代码复用），不做 windows 容器镜像 |

## 6. 与其他设计的关系

- task 34（SQLCipher）：库文件在持久卷，密钥由机器码派生——容器重建不丢数据、跨机不可解（`container-persistence-layout.md` 结论一）；
- task 35（OTA）：只更新应用产物，不动镜像与宿主锚点（本文件红线 2）；
- task 40/41：容器部署形态的指纹仅取宿主锚点，绑定关系落在授权侧（`container-machine-binding.md` 链接推导表）；
- task 43：首次启动自检读取持久卷与镜像 digest，三者对不上即进入失败行为分支。
