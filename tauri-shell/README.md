# tauri-shell — Windows 桌面壳

Windows 交付形态：Tauri 2.x 桌面应用，承载 `web-console`（WebView 管理界面）与 `daemon` 核心，产出 NSIS/MSI 签名安装包。

**当前状态**：目录占位。由 **Wave 4 task 32** 初始化 Tauri 工程，**Wave 5 task 38** 接入打包与 CI（tauri-action）；本阶段不创建 Cargo/前端工程，避免空壳干扰 workspace 构建。

关键约束（设计定稿）：

- 安装包签名与完整性校验；篡改安装包后校验必须失败
- WebView/JS 层**无授权判定逻辑**（授权判定一律在 Rust 侧）
- 防逆向 Tier-1：代码签名、strip/LTO/panic=abort、段哈希自检
- CI 仅 windows-latest 构建（双平台矩阵不含 macOS）
