<template>
  <footer class="site-footer">
    <div class="container footer-inner">
      <div class="footer-brand">
        <div class="logo">
          <div class="logo-icon">⬡</div>
          <span class="logo-text">IoT-DAQ Gateway</span>
        </div>
        <p class="footer-desc">工业边缘数据采集网关 · 让每一比特数据都有价值</p>
      </div>
      <div class="footer-links">
        <div class="link-group">
          <h4>产品</h4>
          <a href="/">首页</a>
          <a href="/docs">文档</a>
          <a href="/contact">联系我们</a>
        </div>
        <div class="link-group">
          <h4>资源</h4>
          <a href="https://github.com/wantstudy/edge-gateway-hub" target="_blank">GitHub</a>
          <a href="https://github.com/wantstudy/edge-gateway-hub/issues" target="_blank">问题反馈</a>
        </div>
        <div class="link-group">
          <h4>联系</h4>
          <a href="mailto:support@iot-daq.com">support@iot-daq.com</a>
        </div>
      </div>
    </div>
    <div class="footer-bottom">
      <div class="container">
        <p>© 2024 IoT-DAQ Gateway. Apache License 2.0</p>
      </div>
    </div>
  </footer>
</template>

<style scoped>
.site-footer {
  background: var(--navy);
  border-top: 1px solid rgba(23, 195, 178, 0.15);
  padding: 48px 0 24px;
  margin-top: 80px;
}
.footer-inner {
  display: grid;
  grid-template-columns: 1fr 2fr;
  gap: 48px;
}
.footer-brand .logo {
  margin-bottom: 12px;
}
.footer-desc {
  color: var(--text-muted);
  font-size: 14px;
  line-height: 1.6;
}
.footer-links {
  display: grid;
  grid-template-columns: repeat(3, 1fr);
  gap: 32px;
}
.link-group h4 {
  color: var(--brand);
  font-size: 14px;
  font-weight: 600;
  margin-bottom: 12px;
  text-transform: uppercase;
  letter-spacing: 0.5px;
}
.link-group a {
  display: block;
  color: var(--text-muted);
  text-decoration: none;
  font-size: 14px;
  padding: 4px 0;
  transition: color 0.2s;
}
.link-group a:hover {
  color: var(--brand);
}
.footer-bottom {
  margin-top: 40px;
  padding-top: 24px;
  border-top: 1px solid rgba(255,255,255,0.1);
  text-align: center;
  color: var(--text-muted);
  font-size: 13px;
}
@media (max-width: 768px) {
  .footer-inner { grid-template-columns: 1fr; }
  .footer-links { grid-template-columns: 1fr 1fr; }
}
</style>
