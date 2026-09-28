<template>
  <div class="docs">
    <div class="docs-layout">
      <!-- Sidebar -->
      <aside class="docs-sidebar">
        <div class="sidebar-title">文档</div>
        <nav class="sidebar-nav">
          <a href="#getting-started" class="nav-item">快速开始</a>
          <a href="#windows-install" class="nav-item">Windows 安装</a>
          <a href="#docker-deploy" class="nav-item">Docker 部署</a>
          <a href="#config" class="nav-item">配置说明</a>
          <a href="#serial-port" class="nav-item">串口采集</a>
          <a href="#faq" class="nav-item">常见问题</a>
        </nav>
      </aside>

      <!-- Content -->
      <main class="docs-content">
        <div class="container">
          <div class="docs-hero">
            <h1>文档中心</h1>
            <p>IoT-DAQ Gateway 部署、配置与使用指南</p>
          </div>

          <section class="doc-section" id="getting-started">
            <h2>快速开始</h2>
            <p class="lead">选择适合您环境的部署方式，5 分钟内完成初始化。</p>
            <a-tabs default-active-key="windows">
              <a-tab-pane key="windows" title="Windows">
                <ol class="doc-steps">
                  <li>下载 <a href="https://github.com/wantstudy/edge-gateway-hub/releases/download/v0.1.0/IoT-DAQ-Gateway_0.1.0_x64-setup.exe" target="_blank">IoT-DAQ-Gateway_0.1.0_x64-setup.exe</a></li>
                  <li>双击安装包（需要 WebView2 Runtime，Win10/11 通常已内置）</li>
                  <li>从开始菜单启动 <strong>IoT-DAQ Gateway</strong>，桌面控制台自动打开</li>
                  <li>访问 <code>http://127.0.0.1:8080</code> 进入 Web 控制台</li>
                </ol>
              </a-tab-pane>
              <a-tab-pane key="docker" title="Docker">
                <div class="code-block">
                  <div class="code-header"><span>bash</span></div>
                  <pre><code><span class="comment"># 1. 拉取镜像</span>
docker pull wantstudy/iot-daq-gateway:0.1.0

<span class="comment"># 2. 初始化目录与密钥</span>
sudo mkdir -p /opt/iot-daq/gateway/{data,secrets}
sudo head -c 32 /dev/urandom | sudo tee /opt/iot-daq/gateway/secrets/iot-daq-fingerprint-key >/dev/null
sudo chown -R 65532:65532 /opt/iot-daq/gateway/data /opt/iot-daq/gateway/secrets
sudo chmod 600 /opt/iot-daq/gateway/secrets/iot-daq-fingerprint-key

<span class="comment"># 3. 启动（从 docker/ 目录执行）</span>
cd docker && docker compose up -d

<span class="comment"># 4. 确认运行</span>
docker logs -f iot-daq-gateway
<span class="comment"># 看到 "bootstrap: daemon running" 即成功</span></code></pre>
                </div>
              </a-tab-pane>
            </a-tabs>
          </section>

          <section class="doc-section" id="config">
            <h2>配置说明</h2>
            <p>网关读取 <code>/etc/iot-daq/gateway.toml</code>。最小配置即可启动，设备、点位、转发规则全部在 Web 控制台完成配置。</p>
            <div class="code-block">
              <div class="code-header"><span>gateway.toml</span></div>
              <pre><code><span class="comment"># 网关名（可选）</span>
<span class="tag">[gateway]</span>
<span class="key">name</span> = <span class="string">"plant-01-gw"</span></code></pre>
            </div>
            <p>容器把 <code>/opt/iot-daq/gateway/data/config/</code> 挂载为 <code>/etc/iot-daq</code>，修改配置后重启容器即可生效。</p>
          </section>

          <section class="doc-section" id="serial-port">
            <h2>串口采集（可选）</h2>
            <p>Modbus RTU 等串口场景，编辑 <code>docker/docker-compose.yml</code>，取消 <code>devices</code> / <code>group_add</code> 两段注释并确认节点存在（如 <code>/dev/ttyUSB0</code>），然后 <code>docker compose up -d</code> 重建。</p>
          </section>

          <section class="doc-section" id="faq">
            <h2>常见问题</h2>
            <div class="faq-list">
              <div class="faq-item">
                <div class="faq-q">Web 控制台打不开？</div>
                <div class="faq-a"><code>curl http://127.0.0.1:9011/api/overview</code> 有 JSON 返回说明网关正常，检查 9012 端口与防火墙；若 <code>.env</code> 改过端口，访问端口要跟着改。</div>
              </div>
              <div class="faq-item">
                <div class="faq-q">Docker 里怎么确认机器码取的是宿主机？</div>
                <div class="faq-a">容器内锚点已被关闭，机器码只可能来自宿主机 machine-id / DMI / MAC。</div>
              </div>
              <div class="faq-item">
                <div class="faq-q">如何升级？</div>
                <div class="faq-a">下载新版本交付物，替换 <code>.env</code> 中 <code>IOT_DAQ_IMAGE</code> 版本号（Windows 直接跑新安装包覆盖安装）。<strong>不要</strong>删除持久卷/数据目录——授权与数据都在里面。</div>
              </div>
            </div>
          </section>
        </div>
      </main>
    </div>
  </div>
</template>

<script setup lang="ts">
</script>

<style scoped>
.docs {
  padding: 40px 0 80px;
  background: var(--surface);
  min-height: calc(100vh - 64px);
}
.docs-layout {
  display: grid;
  grid-template-columns: 220px 1fr;
  min-height: calc(100vh - 64px);
}
.docs-sidebar {
  position: sticky;
  top: 64px;
  height: calc(100vh - 64px);
  padding: 24px 16px;
  border-right: 1px solid rgba(255,255,255,0.06);
  overflow-y: auto;
}
.sidebar-title {
  font-size: 13px;
  font-weight: 600;
  color: var(--brand);
  text-transform: uppercase;
  letter-spacing: 0.5px;
  padding: 0 8px 12px;
  border-bottom: 1px solid rgba(255,255,255,0.08);
  margin-bottom: 8px;
}
.sidebar-nav {
  display: flex;
  flex-direction: column;
  gap: 2px;
}
.nav-item {
  display: block;
  padding: 8px 12px;
  color: var(--text-muted);
  text-decoration: none;
  font-size: 14px;
  border-radius: 6px;
  transition: all 0.15s;
}
.nav-item:hover {
  color: var(--brand);
  background: rgba(23, 195, 178, 0.08);
}
.docs-content {
  padding: 40px 0;
  background: var(--surface);
}
.container { max-width: 800px; margin: 0 auto; padding: 0 24px; }
.docs-hero {
  margin-bottom: 40px;
  padding-bottom: 24px;
  border-bottom: 1px solid rgba(255,255,255,0.08);
}
.docs-hero h1 { font-size: 36px; font-weight: 700; margin-bottom: 8px; }
.docs-hero p { color: var(--text-muted); font-size: 16px; }
.doc-section {
  margin-bottom: 48px;
  scroll-margin-top: 80px;
}
.doc-section h2 {
  font-size: 22px;
  font-weight: 600;
  margin-bottom: 16px;
  color: #fff;
}
.lead { color: var(--text-muted); margin-bottom: 20px; }
code {
  background: rgba(23, 195, 178, 0.12);
  color: var(--brand);
  padding: 2px 6px;
  border-radius: 4px;
  font-size: 13px;
  font-family: 'Fira Code', monospace;
}
.doc-steps {
  padding-left: 20px;
  display: flex;
  flex-direction: column;
  gap: 12px;
}
.doc-steps li { color: var(--text-muted); line-height: 1.6; }
.doc-steps a { color: var(--brand); }
.code-block {
  background: rgba(0,0,0,0.4);
  border: 1px solid rgba(255,255,255,0.08);
  border-radius: 8px;
  overflow: hidden;
  margin: 16px 0;
}
.code-header {
  display: flex;
  align-items: center;
  padding: 8px 16px;
  background: rgba(0,0,0,0.3);
  font-size: 13px;
  color: var(--text-muted);
  border-bottom: 1px solid rgba(255,255,255,0.06);
}
.code-block pre {
  margin: 0;
  padding: 16px;
  font-family: 'Fira Code', monospace;
  font-size: 13px;
  line-height: 1.7;
  color: var(--text);
  overflow-x: auto;
}
.comment { color: #6b7280; }
.tag { color: var(--brand); }
.key { color: #7dd3fc; }
.string { color: #86efac; }

.faq-list { display: flex; flex-direction: column; gap: 16px; }
.faq-item {
  background: rgba(255,255,255,0.03);
  border: 1px solid rgba(255,255,255,0.08);
  border-radius: 8px;
  padding: 16px 20px;
}
.faq-q { font-weight: 600; color: #fff; margin-bottom: 8px; }
.faq-a { color: var(--text-muted); font-size: 14px; line-height: 1.7; }

.arco-tabs { margin-top: 16px; }

@media (max-width: 768px) {
  .docs-layout { grid-template-columns: 1fr; }
  .docs-sidebar { display: none; }
}
</style>
