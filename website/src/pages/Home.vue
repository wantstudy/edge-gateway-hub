<template>
  <div class="home">
    <!-- Hero -->
    <section class="hero">
      <div class="container">
        <div class="hero-content">
          <div class="hero-badge">开源 · 工业级 · 边缘计算</div>
          <h1 class="hero-title">
            IoT-DAQ Gateway<br />
            <span class="highlight">工业边缘数据采集网关</span>
          </h1>
          <p class="hero-subtitle">
            从 PLC / 仪表采集数据，本地缓存与规则计算，并转发到 MQTT 北向平台。<br />
            支持 Windows 安装包与 Docker 镜像，开箱即用。
          </p>
          <div class="hero-actions">
            <a href="#download" class="btn-primary">立即下载</a>
            <router-link to="/docs" class="btn-secondary">查看文档</router-link>
          </div>
          <div class="hero-stats">
            <div class="stat">
              <span class="stat-value">v0.2.0</span>
              <span class="stat-label">当前版本</span>
            </div>
            <div class="stat">
              <span class="stat-value">93.8MB</span>
              <span class="stat-label">Docker 镜像</span>
            </div>
            <div class="stat">
              <span class="stat-value">5.3MB</span>
              <span class="stat-label">Windows 安装包</span>
            </div>
          </div>
        </div>
        <div class="hero-visual">
          <div class="visual-card">
            <div class="code-block">
              <div class="code-header">
                <span class="dot red"></span>
                <span class="dot yellow"></span>
                <span class="dot green"></span>
                <span>gateway.toml</span>
              </div>
              <pre><code><span class="comment"># 最小配置即可启动</span>
<span class="tag">[gateway]</span>
<span class="key">name</span> = <span class="string">"plant-01-gw"</span>

<span class="comment"># 设备采集：Modbus TCP</span>
[[devices]]
driver = <span class="string">"modbus-tcp"</span>
host = <span class="string">"192.168.1.100"</span>
port = <span class="string">502</span>

<span class="comment"># 北向转发：MQTT v5</span>
[northbound]
protocol = <span class="string">"mqtt"</span>
broker = <span class="string">"tcp://mqtt-server:1883"</span></code></pre>
            </div>
          </div>
          <div class="floating-tag tag-1">📡 Modbus TCP/RTU</div>
          <div class="floating-tag tag-2">🔌 OPC UA</div>
          <div class="floating-tag tag-3">📤 MQTT v3.1.1/v5</div>
          <div class="floating-tag tag-4">💾 SQLite 队列</div>
        </div>
      </div>
    </section>

    <!-- Features -->
    <section class="features" id="features">
      <div class="container">
        <div class="section-header">
          <h2>核心特性</h2>
          <p>面向工业现场的全栈数据采集解决方案</p>
        </div>
        <div class="features-grid">
          <div class="feature-card">
            <div class="feature-icon">📡</div>
            <h3>多协议采集</h3>
            <p>支持 Modbus TCP / RTU、OPC UA 等主流工业协议，覆盖 PLC、仪表、传感器等设备。</p>
          </div>
          <div class="feature-card">
            <div class="feature-icon">📤</div>
            <h3>北向转发</h3>
            <p>MQTT（v3.1.1 / v5）发布，每路独立 protobuf / JSON 编码，适配各类云平台。</p>
          </div>
          <div class="feature-card">
            <div class="feature-icon">💾</div>
            <h3>本地韧性</h3>
            <p>断网续传（SQLite 队列）、背压控制，确保数据不丢失、网络恢复后自动补传。</p>
          </div>
          <div class="feature-card">
            <div class="feature-icon">⚙️</div>
            <h3>规则与公式</h3>
            <p>点位级计算、告警规则灵活配置，边缘侧完成数据预处理与阈值判断。</p>
          </div>
          <div class="feature-card">
            <div class="feature-icon">🖥️</div>
            <h3>Web 控制台</h3>
            <p>设备 / 点位 / 转发 / 告警 / 审计全功能管理，本地访问 http://127.0.0.1:8080。</p>
          </div>
          <div class="feature-card">
            <div class="feature-icon">🔒</div>
            <h3>安全设计</h3>
            <p>最小权限容器（非 root、只读根文件系统、cap_drop ALL），降低攻击面。</p>
          </div>
        </div>
      </div>
    </section>

    <!-- System Diagrams -->
    <section class="diagrams" id="diagrams">
      <div class="container">
        <div class="section-header">
          <h2>网关程序系统图</h2>
          <p>一图看懂采集、边缘计算与北向转发</p>
        </div>
        <div class="diagram-grid">
          <figure class="diagram-card">
            <img src="/diagram-arch.svg" alt="系统架构图" loading="lazy" />
            <figcaption>系统架构 · 边云协同</figcaption>
          </figure>
          <figure class="diagram-card">
            <img src="/diagram-flow.svg" alt="数据采集流程图" loading="lazy" />
            <figcaption>数据采集流程 · 本地韧性</figcaption>
          </figure>
          <figure class="diagram-card">
            <img src="/diagram-deploy.svg" alt="部署拓扑图" loading="lazy" />
            <figcaption>部署拓扑 · 最小权限容器</figcaption>
          </figure>
        </div>
      </div>
    </section>

    <!-- Download -->
    <section class="download" id="download">
      <div class="container">
        <div class="section-header">
          <h2>快速下载</h2>
          <p>选择适合您环境的交付形态</p>
        </div>
        <div class="download-cards">
          <a href="https://github.com/wantstudy/edge-gateway-hub/raw/main/downloads/IoT-DAQ-Gateway_0.2.0_x64-setup.exe" class="download-card" target="_blank">
            <div class="card-icon">🪟</div>
            <div class="card-info">
              <h3>Windows 安装包</h3>
              <p>Windows 10/11 x64 · 约 5.3MB</p>
              <span class="card-tag">推荐</span>
            </div>
            <div class="card-action">
              <a-button type="primary" shape="round">下载</a-button>
            </div>
          </a>
          <a href="https://github.com/wantstudy/edge-gateway-hub/raw/main/downloads/iot-daq-gateway-0.2.0-linux-amd64.tar.gz" class="download-card" target="_blank">
            <div class="card-icon">🐳</div>
            <div class="card-info">
              <h3>Docker 镜像</h3>
              <p>Linux AMD64 · 93.8MB</p>
              <span class="card-tag">docker load &lt; iot-daq-gateway-0.2.0-linux-amd64.tar.gz</span>
            </div>
            <div class="card-action">
              <a-button type="secondary" shape="round">查看文档</a-button>
            </div>
          </a>
        </div>
      </div>
    </section>

    <!-- Quick Start -->
    <section class="quickstart">
      <div class="container">
        <div class="section-header">
          <h2>快速开始</h2>
          <p>三行命令完成部署</p>
        </div>
        <div class="steps">
          <div class="step">
            <div class="step-num">1</div>
            <div class="step-content">
              <h4>下载并安装</h4>
              <p>运行安装包或拉取 Docker 镜像</p>
            </div>
          </div>
          <div class="step">
            <div class="step-num">2</div>
            <div class="step-content">
              <h4>配置设备与点位</h4>
              <p>通过 Web 控制台添加设备、点位和转发规则</p>
            </div>
          </div>
          <div class="step">
            <div class="step-num">3</div>
            <div class="step-content">
              <h4>启动采集</h4>
              <p>数据自动采集、缓存、转发至 MQTT 平台</p>
            </div>
          </div>
        </div>
      </div>
    </section>
  </div>
</template>

<script setup lang="ts">
</script>

<style scoped>
.hero {
  padding: 80px 0 60px;
  background: linear-gradient(135deg, var(--navy) 0%, #0a1628 100%);
  position: relative;
  overflow: hidden;
}
.hero::before {
  content: '';
  position: absolute;
  top: -50%;
  right: -20%;
  width: 600px;
  height: 600px;
  background: radial-gradient(circle, rgba(23,195,178,0.08) 0%, transparent 70%);
  border-radius: 50%;
}
.container { max-width: 1200px; margin: 0 auto; padding: 0 24px; }
.hero-content {
  max-width: 560px;
  position: relative;
  z-index: 1;
}
.hero-badge {
  display: inline-block;
  background: rgba(23, 195, 178, 0.15);
  color: var(--brand);
  padding: 4px 16px;
  border-radius: 20px;
  font-size: 13px;
  font-weight: 500;
  margin-bottom: 24px;
  border: 1px solid rgba(23, 195, 178, 0.3);
}
.hero-title {
  font-size: 48px;
  font-weight: 800;
  line-height: 1.15;
  margin-bottom: 20px;
  color: #fff;
}
.highlight { color: var(--brand); }
.hero-subtitle {
  font-size: 17px;
  color: var(--text-muted);
  line-height: 1.7;
  margin-bottom: 32px;
}
.hero-actions {
  display: flex;
  gap: 16px;
  margin-bottom: 48px;
}
.btn-primary, .btn-secondary {
  display: inline-flex;
  align-items: center;
  padding: 12px 28px;
  border-radius: 8px;
  font-size: 15px;
  font-weight: 600;
  text-decoration: none;
  transition: all 0.2s;
}
.btn-primary {
  background: var(--brand);
  color: #fff;
}
.btn-primary:hover { background: var(--brand-dim); }
.btn-secondary {
  background: rgba(255,255,255,0.08);
  color: var(--text);
  border: 1px solid rgba(255,255,255,0.15);
}
.btn-secondary:hover { background: rgba(255,255,255,0.12); }
.hero-stats {
  display: flex;
  gap: 40px;
}
.stat { display: flex; flex-direction: column; }
.stat-value { font-size: 22px; font-weight: 700; color: var(--brand); }
.stat-label { font-size: 13px; color: var(--text-muted); margin-top: 4px; }

.hero-visual {
  position: absolute;
  right: 0;
  top: 50%;
  transform: translateY(-50%);
  width: 45%;
  max-width: 520px;
}
.visual-card {
  background: rgba(255,255,255,0.04);
  border: 1px solid rgba(255,255,255,0.1);
  border-radius: 12px;
  overflow: hidden;
  backdrop-filter: blur(10px);
}
.code-block { padding: 0; }
.code-header {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 12px 16px;
  background: rgba(0,0,0,0.3);
  border-bottom: 1px solid rgba(255,255,255,0.06);
  font-size: 13px;
  color: var(--text-muted);
}
.dot { width: 12px; height: 12px; border-radius: 50%; display: inline-block; }
.dot.red { background: #ff5f57; }
.dot.yellow { background: #febc2e; }
.dot.green { background: #28c840; }
pre {
  margin: 0;
  padding: 20px;
  font-family: 'Fira Code', 'Consolas', monospace;
  font-size: 13px;
  line-height: 1.7;
  color: var(--text);
  background: transparent;
}
.comment { color: #6b7280; }
.tag { color: var(--brand); }
.key { color: #7dd3fc; }
.string { color: #86efac; }

.floating-tag {
  position: absolute;
  background: rgba(23, 195, 178, 0.12);
  border: 1px solid rgba(23, 195, 178, 0.3);
  color: var(--brand);
  padding: 8px 16px;
  border-radius: 8px;
  font-size: 13px;
  font-weight: 500;
}
.tag-1 { top: 10%; right: 10%; }
.tag-2 { top: 35%; right: -5%; }
.tag-3 { bottom: 30%; left: -5%; }
.tag-4 { bottom: 8%; right: 15%; }

.features {
  padding: 80px 0;
  background: var(--surface);
}
.section-header {
  text-align: center;
  margin-bottom: 48px;
}
.section-header h2 {
  font-size: 32px;
  font-weight: 700;
  margin-bottom: 12px;
}
.section-header p {
  color: var(--text-muted);
  font-size: 16px;
}
.features-grid {
  display: grid;
  grid-template-columns: repeat(3, 1fr);
  gap: 24px;
}
.feature-card {
  background: rgba(255,255,255,0.03);
  border: 1px solid rgba(255,255,255,0.08);
  border-radius: 12px;
  padding: 28px;
  transition: all 0.2s;
}
.feature-card:hover {
  border-color: rgba(23, 195, 178, 0.3);
  background: rgba(23, 195, 178, 0.05);
}
.feature-icon { font-size: 32px; margin-bottom: 16px; }
.feature-card h3 {
  font-size: 18px;
  font-weight: 600;
  margin-bottom: 8px;
  color: #fff;
}
.feature-card p {
  font-size: 14px;
  color: var(--text-muted);
  line-height: 1.6;
}

.download {
  padding: 80px 0;
  background: linear-gradient(180deg, var(--surface) 0%, var(--navy) 100%);
}
.download-cards {
  display: grid;
  grid-template-columns: repeat(2, 1fr);
  gap: 24px;
  max-width: 720px;
  margin: 0 auto;
}
.download-card {
  display: flex;
  align-items: center;
  gap: 20px;
  background: rgba(255,255,255,0.04);
  border: 1px solid rgba(255,255,255,0.1);
  border-radius: 12px;
  padding: 24px;
  text-decoration: none;
  transition: all 0.2s;
}
.download-card:hover {
  border-color: var(--brand);
  background: rgba(23, 195, 178, 0.08);
}
.card-icon { font-size: 36px; flex-shrink: 0; }
.card-info { flex: 1; }
.card-info h3 { font-size: 16px; font-weight: 600; color: #fff; margin-bottom: 4px; }
.card-info p { font-size: 13px; color: var(--text-muted); margin-bottom: 8px; }
.card-tag {
  display: inline-block;
  background: rgba(23, 195, 178, 0.15);
  color: var(--brand);
  padding: 2px 10px;
  border-radius: 4px;
  font-size: 12px;
  font-family: monospace;
}
.card-action {}

.quickstart {
  padding: 80px 0;
  background: var(--navy);
}
.steps {
  display: grid;
  grid-template-columns: repeat(3, 1fr);
  gap: 32px;
  max-width: 800px;
  margin: 0 auto;
}
.step {
  display: flex;
  gap: 16px;
  align-items: flex-start;
}
.step-num {
  width: 40px;
  height: 40px;
  background: var(--brand);
  color: #fff;
  border-radius: 50%;
  display: flex;
  align-items: center;
  justify-content: center;
  font-weight: 700;
  font-size: 16px;
  flex-shrink: 0;
}
.step-content h4 {
  font-size: 16px;
  font-weight: 600;
  margin-bottom: 4px;
  color: #fff;
}
.step-content p {
  font-size: 14px;
  color: var(--text-muted);
}

.diagrams { padding: 80px 0; background: var(--surface); }
.diagram-grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(260px, 1fr));
  gap: 24px;
}
.diagram-card {
  background: rgba(255,255,255,0.03);
  border: 1px solid rgba(255,255,255,0.08);
  border-radius: 12px;
  overflow: hidden;
  margin: 0;
  transition: all 0.2s;
}
.diagram-card:hover {
  border-color: rgba(23,195,178,0.3);
  background: rgba(23,195,178,0.05);
}
.diagram-card img {
  display: block;
  width: 100%;
  height: auto;
  background: #0f1b3d;
}
.diagram-card figcaption {
  padding: 14px 16px;
  font-size: 14px;
  color: var(--text-muted);
  text-align: center;
}

@media (max-width: 900px) {
  .hero-visual { display: none; }
  .features-grid { grid-template-columns: repeat(2, 1fr); }
}
@media (max-width: 600px) {
  .hero-title { font-size: 32px; }
  .features-grid { grid-template-columns: 1fr; }
  .download-cards { grid-template-columns: 1fr; }
  .steps { grid-template-columns: 1fr; }
}
</style>
