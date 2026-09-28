<template>
  <div class="contact">
    <div class="container">
      <div class="contact-hero">
        <h1>联系我们</h1>
        <p>有技术问题或商务需求？我们随时为您解答</p>
      </div>

      <div class="contact-grid">
        <div class="contact-info">
          <div class="info-card">
            <div class="info-icon">📧</div>
            <h3>邮件联系</h3>
            <a href="mailto:support@iot-daq.com">support@iot-daq.com</a>
          </div>
          <div class="info-card">
            <div class="info-icon">🐙</div>
            <h3>GitHub</h3>
            <a href="https://github.com/wantstudy/edge-gateway-hub" target="_blank">wantstudy/edge-gateway-hub</a>
            <p>查看源码、提交 Issue</p>
          </div>
          <div class="info-card">
            <div class="info-icon">💬</div>
            <h3>问题反馈</h3>
            <a href="https://github.com/wantstudy/edge-gateway-hub/issues" target="_blank">GitHub Issues</a>
            <p>报告 bug 或提出功能建议</p>
          </div>
        </div>

        <div class="contact-form-wrap">
          <div class="form-header">
            <h2>发送消息</h2>
            <p>我们将在 1-2 个工作日内回复</p>
          </div>
          <a-form :model="form" layout="vertical" @submit="handleSubmit">
            <a-form-item label="姓名" field="name" :rules="[{ required: true, message: '请输入姓名' }]">
              <a-input v-model="form.name" placeholder="您的姓名" allow-clear />
            </a-form-item>
            <a-form-item label="邮箱" field="email" :rules="[{ required: true, type: 'email', message: '请输入有效邮箱' }]">
              <a-input v-model="form.email" placeholder="your@email.com" allow-clear />
            </a-form-item>
            <a-form-item label="主题" field="subject">
              <a-input v-model="form.subject" placeholder="问题简述" allow-clear />
            </a-form-item>
            <a-form-item label="内容" field="message" :rules="[{ required: true, message: '请输入消息内容' }]">
              <a-textarea
                v-model="form.message"
                placeholder="请详细描述您的问题或需求..."
                :auto-size="{ minRows: 4, maxRows: 8 }"
              />
            </a-form-item>
            <a-form-item>
              <a-button type="primary" html-type="submit" :loading="loading" style="width: 140px">
                {{ loading ? '发送中...' : '提交' }}
              </a-button>
            </a-form-item>
          </a-form>
          <a-alert v-if="submitted" type="success" banner closable style="margin-top: 16px">
            消息已发送，感谢您的反馈！
          </a-alert>
        </div>
      </div>
    </div>
  </div>
</template>

<script setup lang="ts">
import { ref, reactive } from 'vue'

const form = reactive({ name: '', email: '', subject: '', message: '' })
const loading = ref(false)
const submitted = ref(false)

function handleSubmit() {
  loading.value = true
  setTimeout(() => {
    loading.value = false
    submitted.value = true
    form.name = ''
    form.email = ''
    form.subject = ''
    form.message = ''
  }, 1200)
}
</script>

<style scoped>
.contact {
  padding: 60px 0 80px;
  background: var(--surface);
  min-height: calc(100vh - 64px);
}
.container { max-width: 1000px; margin: 0 auto; padding: 0 24px; }
.contact-hero {
  text-align: center;
  margin-bottom: 48px;
}
.contact-hero h1 { font-size: 36px; font-weight: 700; margin-bottom: 12px; }
.contact-hero p { color: var(--text-muted); font-size: 16px; }

.contact-grid {
  display: grid;
  grid-template-columns: 300px 1fr;
  gap: 40px;
  align-items: start;
}
.contact-info { display: flex; flex-direction: column; gap: 16px; }
.info-card {
  background: rgba(255,255,255,0.04);
  border: 1px solid rgba(255,255,255,0.08);
  border-radius: 10px;
  padding: 20px;
}
.info-icon { font-size: 24px; margin-bottom: 8px; }
.info-card h3 { font-size: 15px; font-weight: 600; color: #fff; margin-bottom: 6px; }
.info-card a {
  display: block;
  color: var(--brand);
  text-decoration: none;
  font-size: 14px;
  margin-bottom: 4px;
}
.info-card a:hover { text-decoration: underline; }
.info-card p { font-size: 13px; color: var(--text-muted); }

.contact-form-wrap {
  background: rgba(255,255,255,0.03);
  border: 1px solid rgba(255,255,255,0.08);
  border-radius: 12px;
  padding: 28px;
}
.form-header { margin-bottom: 24px; }
.form-header h2 { font-size: 20px; font-weight: 600; margin-bottom: 4px; }
.form-header p { font-size: 13px; color: var(--text-muted); }

:deep(.arco-form-item-label) { color: var(--text-muted); }
:deep(.arco-input) { background: rgba(255,255,255,0.05); border-color: rgba(255,255,255,0.15); color: #fff; }
:deep(.arco-input:focus, .arco-input:hover) { border-color: var(--brand); }
:deep(.arco-textarea) { background: rgba(255,255,255,0.05); border-color: rgba(255,255,255,0.15); color: #fff; }

@media (max-width: 768px) {
  .contact-grid { grid-template-columns: 1fr; }
}
</style>
