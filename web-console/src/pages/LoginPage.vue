<template>
  <!--
    LoginPage —— 登录页（仅 real 模式可达，见 router.ts 守卫）。

    设计对齐：
      · Arco 表单组件 + ui-kit token（颜色全部走 CSS 变量，无硬编码色值）；
      · 独立全屏布局（App.vue 在 /login 路由下不渲染顶栏 / 侧栏外壳）；
      · 401 → 「账号或密码错误」；网络失败 / 5xx → 透传错误信息，不崩页面。
  -->
  <div class="wc-login">
    <div class="wc-login__card">
      <div class="wc-login__brand">
        <span class="wc-login__logo">GW</span>
        <h1 class="wc-login__title">数据网关控制台</h1>
      </div>
      <p class="wc-login__sub">请使用运维账号登录本机网关管理台</p>

      <a-form :model="form" layout="vertical">
        <a-form-item field="username" label="账号" required>
          <a-input v-model="form.username" placeholder="请输入账号" allow-clear />
        </a-form-item>
        <a-form-item field="password" label="密码" required>
          <a-input-password v-model="form.password" placeholder="请输入密码" />
        </a-form-item>

        <a-alert v-if="errorMsg" type="error" class="wc-login__alert">{{ errorMsg }}</a-alert>

        <a-button
          type="primary"
          long
          size="large"
          :loading="loading"
          class="wc-login__submit"
          @click="onSubmit"
        >
          登 录
        </a-button>
      </a-form>

      <p class="wc-login__foot">本地联调模式 · 服务地址 127.0.0.1:8080</p>
    </div>
  </div>
</template>

<script setup lang="ts">
/**
 * @file LoginPage.vue
 * @module web-console/pages/login
 * @description 登录页：real 模式下 401 跳转至此；成功后写 session + 预取真实数据并回跳。
 */
import { reactive, ref } from 'vue';
import { useRoute, useRouter } from 'vue-router';
import { ApiError, apiLogin, setStoredBackendRole, setStoredToken } from '@/api/client';
import { preloadRealData } from '@/api/repo';
import { mapBackendRole, session } from '@/store/session';

const route = useRoute();
const router = useRouter();

/** 表单草稿（账号预填运维主账号，密码留空防误触）。 */
const form = reactive({ username: 'ops', password: '' });

/** 提交中状态（按钮 loading + 防重复提交）。 */
const loading = ref(false);

/** 错误提示（401 / 网络 / 5xx）。 */
const errorMsg = ref('');

/** 提交登录。 */
async function onSubmit(): Promise<void> {
  if (loading.value) {
    return;
  }
  if (!form.username.trim() || !form.password) {
    errorMsg.value = '请输入账号与密码';
    return;
  }
  loading.value = true;
  errorMsg.value = '';
  try {
    const res = await apiLogin(form.username.trim(), form.password);
    setStoredToken(res.token);
    setStoredBackendRole(res.role);
    // 写会话（后端角色原文 + 前端可见性映射）
    session.login(form.username.trim(), { backendRole: res.role, role: mapBackendRole(res.role) });
    //
    // D-01 修复：登录成功后**立即回跳**，preload 转后台执行。
    // 旧实现 `await preloadRealData()` 混入了 `/api/events`（无限 SSE 流，
    // `res.text()` 永不 resolve）→ 登录按钮永久 loading、页面卡在登录页。
    // preload 内部另有 8s 超时护栏，完成后由 `dataVersion` 驱动页面刷新。
    void preloadRealData().catch((cause: unknown) => {
      console.warn('[web-console] 登录后预取真实数据失败，页面回退 mock / 诚实空态', cause);
    });
    const redirect = typeof route.query.redirect === 'string' && route.query.redirect ? route.query.redirect : '/overview';
    await router.replace(redirect);
  } catch (cause) {
    if (cause instanceof ApiError && cause.status === 401) {
      errorMsg.value = '账号或密码错误（401）';
    } else if (cause instanceof ApiError && cause.status === 403) {
      errorMsg.value = '权限不足（403），请联系管理员';
    } else if (cause instanceof ApiError && cause.status === 0) {
      errorMsg.value = '无法连接网关服务，请确认 daemon 已启动';
    } else {
      errorMsg.value = cause instanceof Error ? cause.message : '登录失败，请稍后重试';
    }
  } finally {
    loading.value = false;
  }
}
</script>

<style scoped>
/* 全屏登录布局（颜色全部取自 ui-kit token） */
.wc-login {
  display: flex;
  align-items: center;
  justify-content: center;
  height: 100vh;
  min-width: 1180px;
  background: var(--bg-app);
}
.wc-login__card {
  width: 380px;
  padding: 36px 32px 24px;
  background: var(--bg-card);
  border: 1px solid var(--border);
  border-radius: var(--radius);
  box-shadow: var(--shadow);
}
.wc-login__brand {
  display: flex;
  align-items: center;
  gap: 12px;
}
.wc-login__logo {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 40px;
  height: 40px;
  border-radius: var(--radius-sm);
  background: var(--brand);
  color: #fff;
  font-weight: 700;
  font-size: 16px;
}
.wc-login__title {
  margin: 0;
  font-size: 18px;
  font-weight: 600;
  color: var(--text-1);
}
.wc-login__sub {
  margin: 10px 0 24px;
  font-size: 13px;
  color: var(--text-3);
}
.wc-login__alert {
  margin-bottom: 16px;
}
.wc-login__submit {
  margin-top: 4px;
}
.wc-login__foot {
  margin: 20px 0 0;
  text-align: center;
  font-size: 12px;
  color: var(--text-3);
}
</style>
