# headless — Linux 网关服务

Linux 交付形态：headless 服务（原生 AppImage/deb/rpm + systemd），同时作为 Docker 容器镜像的运行体，托管 HTTP server 与 Vue 管理界面。

**当前状态**：目录占位。由 **Wave 4 task 33** 初始化（容器友好的路径与信号处理、优雅停机 flush）；容器化交付资产在 `deploy/`（task 60）。本阶段不创建 Cargo 工程。

关键约束（设计定稿）：

- 容器内机器码锚点必须取自宿主机（只读挂载 `/etc/machine-id`、`/sys/class/dmi/id/*`，MAC 由安装脚本注入）——**禁止采容器内 machine-id / 容器 MAC / 容器主机名**
- 试用标记、租约、授权状态必须落宿主机持久卷（`docker rm && docker run` 不得重置）
- 串口经 `--device` 映射 + `--group-add dialout`；南向直连用 `--network host`
- musl 静态链接规避 glibc 版本绑定（构建期决策）
