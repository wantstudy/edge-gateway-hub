# scripts/ —— 本机一键打包 + 同步发布脚本集

面向 **Windows Git Bash** 的发布工具链：从 `iot-daq` 源码仓构建 Windows 安装包与 Docker 镜像，
同步到分发仓 `edge-gateway-hub`，并发布到 Docker Hub / 官网服务器 / GitHub。

设计原则：**fail-closed**（前置不满足立即非零退出）、**显式**（关键命令执行前先打印）、
**不删文件**（本机安全删除机制会拦截 `rm -rf`，一律改用 `mv` 到系统 Temp）。

---

## 1. 脚本清单

| 脚本 | 作用 | 是否有副作用 |
|---|---|---|
| `lib/common.sh` | 公共库：路径解析、版本读取、常量、日志、Rust 环境、代理切换 | 否（被 source，不单独执行） |
| `check-consistency.sh` | 一致性门禁：7 项校验，任一项 FAIL 即非零退出 | 只读 |
| `build-windows.sh` | 构建 Windows NSIS 安装包并同步到 `downloads/` | 是（编译 + 复制） |
| `build-docker.sh` | 构建 Docker 镜像并打 Hub 双 tag（**不 push**） | 是（构建） |
| `publish.sh` | 发布到外部（Docker Hub / 官网 / git / 服务器离线镜像） | 默认 `--dry-run`，需 `--execute` |
| `release.sh` | 串起全流程：门禁 → Windows → Docker → 门禁 → 提示发布 | 是（调用上面两个） |

---

## 2. 通用约定

- **版本单一真源**：`iot-daq/deploy/win/tauri-shell/src-tauri/tauri.conf.json` 的 `version` 字段。
  exe 文件名由它决定，**不从 `Cargo.toml` 继承**。所有脚本都用 `read_version()` 读它，
  可用 `--version x.y.z` 覆盖。版本号一律按**字符串**处理。
- **源码仓定位**：① 环境变量 `IOT_DAQ_SRC`；② 分发仓同级目录 `../iot-daq`；③ 都没有 → 报错退出。
- **代理切换**（最容易踩的坑，脚本内已显式处理）：
  - `cargo` / `npm` / `vite build`：**必须清代理**（本机代理会让 crates.io / npm 拉取失败）。
  - `docker push` / `git push` / `scp` / `ssh`：**必须保留代理**（本机靠 `HTTP_PROXY` 出网）。
  - 出网代理地址可用 `LOCAL_PROXY` 环境变量覆盖，默认 `http://127.0.0.1:60210`。
- **路径**：exe 产物名含空格（`IoT-DAQ Gateway_0.2.0_x64-setup.exe`），脚本内全程加引号；
  同步到分发仓时空格替换为连字符（`IoT-DAQ-Gateway_0.2.0_x64-setup.exe`）。
  docker 命令的路径参数统一转成 `C:/` 正斜杠形式。
- **不删除**：清目录用 `mv` 到 `%TEMP%`，不使用 `rm -rf`。

---

## 3. 各脚本用法

### 3.1 `check-consistency.sh` —— 一致性门禁

```bash
bash scripts/check-consistency.sh                       # 全量 7 项
bash scripts/check-consistency.sh --version 0.2.0       # 覆盖版本
bash scripts/check-consistency.sh --only exe,image      # 只跑部分分组
```

分组：`exe`（①②③）/ `image`（④）/ `prefix`（⑤）/ `version`（⑥⑦）/ `all`（默认）。

校验项：

1. `downloads/IoT-DAQ-Gateway_<VER>_x64-setup.exe` 存在
2. `website/index.html` 引用的 exe 文件名 == 实际文件名
3. `README.md` 引用的 exe 文件名 == 实际文件名
4. 镜像引用四处一致：官网 `docker run` == README `docker run` == `docker/docker-compose.yml` 的 `image:` == `docker/.env.example` 的 `IOT_DAQ_IMAGE`
5. 全仓（排除 `.git` / `node_modules` / `dist` / `scripts`）搜索 `iot-daq-gateway:` 但**缺少** `wantstudy/` 前缀的引用
6. iot-daq 侧 `Cargo.toml` workspace version == `tauri.conf.json` version（源码仓存在才查）
7. 官网 `v<VER>` 版本标签与 tauri 版本一致

输出 PASS/FAIL 表，末尾汇总 `N/M 通过`；有 FAIL 时退出码 1。

### 3.2 `build-windows.sh`

```bash
bash scripts/build-windows.sh
bash scripts/build-windows.sh --skip-cargo      # 沿用已有 target/，仍重跑 npm run build
bash scripts/build-windows.sh --version 0.2.1
bash scripts/build-windows.sh --no-check        # 结束后不跑门禁
```

链路：清理 Tauri 构建缓存 → `cargo build --release --bin iot-daq-daemon` →
复制到 `src-tauri/resources/` → `npm run build`（Node tauri CLI，本机**没有** `cargo tauri`）→
取 `bundle/nsis/` 产物 → 复制到 `downloads/`（空格转连字符）→ 跑门禁的 exe+version 分组。

### 3.3 `build-docker.sh`

```bash
bash scripts/build-docker.sh
bash scripts/build-docker.sh --no-frontend             # 前端未变时跳过重建
bash scripts/build-docker.sh --platform linux/amd64    # 默认
```

链路：前端 `vite build` → 同步到 `deploy/docker/context/web-console-dist/` →
跑守卫 `check-web-console-dist.sh`（占位页会 fail-closed）→ `render-dockerfile-digests.sh` →
`docker buildx build --platform linux/amd64 --file Dockerfile.rendered -t iot-daq-gateway:<VER> --load <仓库根>` →
打 `wantstudy/iot-daq-gateway:<VER>` 与 `:latest` 双 tag。

构建耗时 6~25 分钟。**只构建不推送**。

### 3.4 `publish.sh`

```bash
bash scripts/publish.sh                              # 全量 dry-run（默认）
bash scripts/publish.sh --execute                    # 真正执行已开启的子任务
bash scripts/publish.sh --execute --skip-docker
bash scripts/publish.sh --execute --website --git
bash scripts/publish.sh --execute --server-load
```

| 参数 | 含义 |
|---|---|
| `--dry-run` | 默认。只打印命令，不执行 |
| `--execute` | 真正执行 |
| `--skip-docker` | 跳过 `docker push` 双 tag |
| `--website` | 官网 vite build + `scp` 部署 + `ssh nginx -s reload` |
| `--git` | 分发仓 `git commit` + `push origin main`（提交信息带 iot-daq commit hash） |
| `--server-load` | `docker save \| gzip \| ssh docker load`（服务器不可达 Docker Hub 时用） |

发布前会先跑一次一致性门禁，未通过直接中止。

### 3.5 `release.sh`

```bash
bash scripts/release.sh                        # 门禁 → Windows → Docker → 门禁
bash scripts/release.sh --only check           # check / windows / docker / all
bash scripts/release.sh --only docker --version 0.2.1
bash scripts/release.sh --skip-check-first
```

透传参数：`--version` / `--platform` / `--skip-cargo` / `--no-frontend`。
结束后只**提示** `publish.sh` 命令，不代替用户发布。

---

## 4. 典型流程：完整发版

```bash
# 0) 在 iot-daq 侧统一版本号（三处：workspace Cargo.toml / tauri-shell Cargo.toml / tauri.conf.json）
#    改完先自检
bash scripts/check-consistency.sh --only version

# 1) 全流程构建（耗时较长：cargo + npm + docker buildx）
bash scripts/release.sh

# 2) 演练发布（只读 + 打印命令）
bash scripts/publish.sh --dry-run

# 3) 确认无误后真正发布（按需组合）
bash scripts/publish.sh --execute                      # 推 Docker Hub 双 tag
bash scripts/publish.sh --execute --website --git      # + 部署官网 + 推分发仓
bash scripts/publish.sh --execute --server-load        # 服务器离线导入镜像
```

只想重出 Windows 包、不动镜像：

```bash
bash scripts/release.sh --only windows
bash scripts/publish.sh --execute --skip-docker --git
```

---

## 5. 本机环境前置要求

| 依赖 | 说明 |
|---|---|
| Git Bash | 所有脚本的解析与运行 shell（需 `bash` 4+、`sed`/`awk`/`grep` GNU 版） |
| Rust 工具链 | `RUSTUP_HOME=D:/rust/rustup`、`CARGO_HOME=D:/rust/cargo`；mingw-gnu 工具链在 `D:/rust/mingw64` |
| Node / npm | Node 版 tauri CLI（`npm run build` = `tauri build`）；本机**没有** `cargo tauri` |
| Docker Desktop | 29.7.2（WSL2 后端），需 `buildx`；构建上下文走 `--load` |
| 出网代理 | 默认 `http://127.0.0.1:60210`（`LOCAL_PROXY` 可覆盖）；`docker login` 需已登录 |
| SSH | `~/.ssh/id_ed25519_github`（`GIT_SSH_KEY` 可覆盖）；服务器 `root@60.205.8.146` 免密（`SERVER_SSH_HOST` 可覆盖） |

默认目标常量（均可用环境变量覆盖）：

```
IMAGE_REPO     = wantstudy/iot-daq-gateway
IMAGE_LATEST   = wantstudy/iot-daq-gateway:latest
CONTAINER_NAME = edge-gateway-hub
SERVER_SSH_HOST= root@60.205.8.146
SERVER_WEB_ROOT= /www/wwwroot/edge.webscad.cn/
GIT_SSH_KEY    = ~/.ssh/id_ed25519_github
LOCAL_PROXY    = http://127.0.0.1:60210
```

---

## 6. 已知坑位（脚本已处理，改脚本时勿回退）

1. Tauri 构建缓存残留旧绝对路径 → `tauri-build` 读 plugin permissions 失败。
   → 每次构建前把 `src-tauri/target/release/build` `mv` 到 Temp（**不用 `rm -rf`**）。
2. `rustup` 自带 self-contained mingw 链接器会盖掉系统 mingw64。
   → `PATH` 里 `/d/rust/mingw64/bin` 必须排在 rustup 之前。
3. 镜像里的 web-console 是占位页。→ 前端构建 + 同步 + 守卫脚本三步缺一不可。
4. README 里 `docker run` 用了没有 `wantstudy/` 前缀的镜像名，用户会拉到本地残留镜像。
   → 门禁第 ⑤ 项专门扫这个。
5. `MSYS2_ARG_CONV_EXCL="*"` 会连 `-f` 参数一起禁掉路径转换。
   → 只在 `docker run` 的挂载参数处用，本脚本集一律 unset。
6. 服务器离线导入镜像时若同名旧镜像未删除，load 后镜像 ID 不变、tag 不生效。
   → `--server-load` 先 `docker rmi` 再 `docker load`。
