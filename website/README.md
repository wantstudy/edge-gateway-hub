# IoT-DAQ Gateway 官方网站

单文件静态产品官网，基于 `iot-daq-gateway-v2.html` 的暗色工程风格（CSS / JS 全部内联，**无需构建步骤**）。

## 文件

| 文件 | 说明 |
|------|------|
| `index.html` | **站点主文件**（自包含，内联全部 CSS/JS，可直接部署） |
| `robots.txt` | 抓取规则：**允许 `/assets/` 图片抓取**，并声明 sitemap 地址 |
| `sitemap.xml` | 站点地图；新增页面时必须同步补 `<loc>` 与 `lastmod` |
| `scripts/baidu-push.sh` | 百度主动推送脚本（详见「SEO 与收录」） |
| `public/favicon.svg` | 原 Vite 项目遗留的 favicon（站点已改为内联 SVG favicon，此文件可选） |
| `src/` `package.json` `vite.config.ts` | 原 Vite + Vue 3 项目遗留，现已不再用于构建本站点 |

> 说明：本站点已不再依赖 Vite 构建流程。`index.html` 本身就是最终交付物，可直接由任意静态文件服务器托管。

## 本地预览

```bash
# 任选其一
# 1) 直接用浏览器打开
open index.html        # macOS
# 或用任意静态服务器
python -m http.server 8080   # 访问 http://localhost:8080
npx serve .
```

## 部署

`index.html` 是单文件、无构建产物依赖，部署到任意静态服务器即可（nginx / 宝塔 / GitHub Pages / CDN / 对象存储）。

### nginx 示例配置

```nginx
server {
    listen 80;
    server_name your-domain.com;
    root /path/to/website;          # 指向 website/ 目录
    index index.html;

    location / {
        try_files $uri $uri/ /index.html;
    }
}
```

### 部署到 edge.webscad.cn（示例）

```bash
# 1. 登录服务器
ssh root@60.205.8.146

# 2. 创建网站目录并上传单文件
mkdir -p /www/wwwroot/edge.webscad.cn/website
scp F:/prod-fee-prj/edge-gateway-hub/website/index.html \
    root@60.205.8.146:/www/wwwroot/edge.webscad.cn/website/

# 3. 在宝塔 / nginx 中将该目录指向网站根，无需构建
```

## SEO 与收录

站点为单页静态站，百度收录依赖三条通道：

| 通道 | 载体 | 说明 |
|------|------|------|
| 蜘蛛发现 | `robots.txt` + `sitemap.xml` | robots 声明 sitemap；**不得屏蔽 `/assets/`**（存放 favicon / logo / 二维码等图片资源，屏蔽即无法抓取也无法进入图片搜索） |
| 自动推送 | `index.html` 内嵌 `push.js` | 仅在**有真实用户访问**时触发；新站零访问等于零推送 |
| 主动推送 | `scripts/baidu-push.sh` | 由发布流程主动调用，不依赖访问量，是新站最有效的收录加速手段 |

主动推送用法（准入密钥在百度搜索资源平台「普通收录 → 主动推送」获取，
**只经环境变量注入，不写入仓库**）：

```bash
BAIDU_PUSH_TOKEN=<token> ./scripts/baidu-push.sh --dry-run    # 先看将推送哪些 URL
BAIDU_PUSH_TOKEN=<token> ./scripts/baidu-push.sh              # 推送 sitemap.xml 内全部 URL
BAIDU_PUSH_TOKEN=<token> ./scripts/baidu-push.sh --limit 10   # 限制条数，保护每日配额
BAIDU_PUSH_TOKEN=<token> ./scripts/baidu-push.sh https://edge.webscad.cn/faq/   # 推送单个新页面
```

退出码：`0` 成功；`2` 前置不满足（token 缺失 / URL 为空）；`3` 接口返回失败。

> ⚠️ 本机若经抓包或企业代理出网，TLS 校验会因代理证书失败（`schannel: SNI or certificate check failed`），
> 脚本会给出明确提示。请改在服务器 / CI 直连执行，或设置 `CURL_CA_BUNDLE` 指向可信根证书。

> 上线后需在百度搜索资源平台依次完成：站点验证 → 「抓取诊断」自查 → 提交 sitemap → 配置主动推送 token。

## 页面内容

- **首屏**：数据流拓扑（采集侧 → 网关 → MQTT 北向）
- **核心特性**：多协议采集 / 北向转发 / 本地韧性 / 规则与公式 / Web 控制台 / 安全设计
- **快速下载**：Windows 安装包（GitHub Releases）+ Docker 镜像
- **Docker 部署**：一键 `docker run`（host 网络，镜像 `wantstudy/iot-daq-gateway:latest`，端口 `9011` API / `9012` Web，数据 `/data/edge-gateway`，授权地址已内置、无需任何配置）
- **快速开始**：三步部署
- **页脚**：联系方式 + ICP 备案号

## 修改说明（相对 v2.html 源）

- 新增「Docker 部署」独立章节，采用简化部署指令（9011 / 9012 端口，授权服务地址已内置镜像，无需任何环境变量）。
- 删除短语「最小配置即可启动」（配置章节标题改为「极简配置，快速接入」，终端注释同步调整）。
- Web 控制台访问地址更新为 `http://127.0.0.1:9012`（与部署端口一致）。
- 联系方式与 ICP 备案号沿用 v2.html 源中的内容（见页脚），如需更新请直接编辑 `index.html` 页脚对应字段。

## License

Apache License 2.0 — 与 IoT-DAQ Gateway 主项目保持一致。
