<template>
  <!--
    LoginPage —— 路由级独立登录页（`/#/login`）。
    · 未登录访问任何受保护路由 → 守卫重定向到本页（router.ts beforeEach）；
    · real 模式：调 `POST /admin/auth/login`（后端已落地管理员鉴权），成功存
      token + role 并跳转该角色首个可见页面；失败展示**后端 message 原文**；
    · mock 模式：原型最小可用登录（任意非空账号 + ≥6 位密码），行为零回归。
    本页自带样式（冰川主题：--bg-app 底 / 白卡 16px 圆角 / --brand 主色按钮），
    不再从 global.css 取 .ac-login*（避免两处样式漂移）。
  -->
  <div class="ac-login">
    <form class="ac-login__card" data-testid="login-card" @submit.prevent="submit">
      <!-- 品牌区：纯文字品牌，沿用 AppShell 的 .ac-brand 写法，不引入任何图片 -->
      <div class="ac-login__brand">
        <span class="ac-brand__logo">LIC</span>
        <span>IoT-DAQ 授权管理</span>
      </div>
      <p class="ac-login__desc">
        厂商侧内部系统 · 仅授权运营人员使用<br />
        所有写操作将记入审计日志（操作者 / 对象 / 原因 / 来源 IP / 结果）
      </p>

      <div class="ac-login__fields">
        <UiField label="管理员账号" required :error="error">
          <UiInput
            v-model="account"
            type="text"
            autocomplete="username"
            placeholder="如 li.gong"
            :disabled="submitting"
          />
        </UiField>
        <UiField :label="isReal ? '密码（后端管理员鉴权）' : '密码'" required :hint="passwordHint">
          <UiInput
            v-model="password"
            type="password"
            autocomplete="current-password"
            placeholder="请输入密码"
            :disabled="submitting"
          />
        </UiField>
        <!-- real 模式独有：默认租户 ID（发放 / 废弃 / 重发请求的租户来源） -->
        <UiField
          v-if="isReal"
          label="默认租户 ID"
          required
          hint="写端点 X-Tenant-Id 头的兜底租户；请填写 licensing-server 中的租户 ID（如 t-1）"
        >
          <UiInput v-model="tenantId" placeholder="t-1" :disabled="submitting" />
        </UiField>
      </div>

      <button
        type="submit"
        class="ac-btn ac-btn--primary ac-login__submit"
        data-testid="login-submit"
        :disabled="submitting || !canSubmit"
      >
        {{ submitting ? '登录中…' : '登录' }}
      </button>

      <div class="ac-hint">
        登录后按后端签发的角色（运营 / 授权运营 / 风控 / 系统）进入对应权限视图；
        登录态有效期 1 小时，过期后自动回到本页。
      </div>
    </form>
  </div>
</template>

<script setup lang="ts">
/**
 * @file LoginPage.vue
 * @module admin-console/pages/LoginPage
 * @description 登录页。草稿（账号 / 密码 / 租户）为页面局部状态，登录成功后交给 session 并跳转。
 *
 * real 模式鉴权（契约 = crates/licensing-server/src/admin_auth.rs）：
 *  1. `POST /admin/auth/login` → `{token, role}`（HS256 JWT，1h TTL）；
 *  2. 成功：token + role 持久化（localStorage），session 记录后端签发角色并跳转；
 *  3. 失败：`ApiError.message` 即后端统一信封的 `message` 原文（如 401 `invalid
 *     credentials`）——**原样展示**，不做二次改写，避免掩盖真实原因；
 *  4. 网络不通（status 0）/ 其他错误 → 同样展示原始 message，绝不静默假登录。
 * 登录后立即跳转并触发 `preloadRealData()`（mount 不等 preload——联调 P0 教训）。
 */
import { computed, ref } from 'vue';
import { useRouter } from 'vue-router';
import { UiField, UiInput, firstAllowedPage } from '@ui-kit';
import { session } from '../store/session';
import { API_MODE, ApiError, adminLogin } from '../api/client';
import { preloadRealData } from '../api/repo';

const router = useRouter();

/** 是否 real 模式（构建期常量）。 */
const isReal = API_MODE === 'real';

/** 账号草稿。 */
const account = ref('li.gong');
/** 密码草稿。 */
const password = ref('');
/** 默认租户 ID 草稿（real 模式独有）。 */
const tenantId = ref('t-1');
/** 行内错误（原样透传后端 message）。 */
const error = ref('');
/** 提交中（防重复点击）。 */
const submitting = ref(false);

/** 密码框提示（随模式变化）。 */
const passwordHint = computed(() =>
  isReal ? 'licensing-server 管理员凭证（env 注入，非空即可提交）' : '原型环境：任意 ≥6 位字符即可登录',
);

/** 可提交条件（账号非空 + 密码非空；real 模式还要求租户 ID 非空）。 */
const canSubmit = computed(
  () =>
    account.value.trim().length > 0 &&
    password.value.length >= (isReal ? 1 : 6) &&
    (!isReal || tenantId.value.trim().length > 0),
);

/**
 * 结构化错误消息：优先后端 message 原文。
 *
 * 后端错误一律经 `ApiError`（信封 `message`，见 api/client.ts），登录失败时
 * 只呈现该文本 → 出现「为什么失败」的单一真源，不再前端自造文案。
 */
function messageOf(cause: unknown): string {
  if (cause instanceof ApiError) {
    const raw = cause.message.trim();
    if (raw) {
      return raw;
    }
    // 后端未回 message（非空信封 / 无网络细节）时的兜底，仍保留状态码这一结构化信息
    return cause.status === 401 || cause.status === 403
      ? '账号或密码错误（后端返回 HTTP 401）'
      : `登录失败（HTTP ${cause.status || '网络异常'}）`;
  }
  return cause instanceof Error ? cause.message : '登录失败，请稍后重试';
}

/** real 模式登录（返回是否放行）。 */
async function submitReal(accountName: string): Promise<boolean> {
  try {
    const { token, role } = await adminLogin(accountName, password.value);
    session.login(accountName, tenantId.value.trim(), token, role);
    return true;
  } catch (cause) {
    error.value = messageOf(cause);
    return false;
  }
}

/** 登录：写入会话并跳转到该角色首个可见页面（登录后立即跳转，不等 preload）。 */
async function submit(): Promise<void> {
  if (!canSubmit.value || submitting.value) {
    error.value = isReal
      ? '请填写管理员账号、密码与默认租户 ID'
      : '请填写管理员账号与密码（密码至少 6 位）';
    return;
  }
  error.value = '';
  submitting.value = true;
  try {
    const accountName = account.value.trim();
    if (isReal) {
      const allowed = await submitReal(accountName);
      if (!allowed) {
        return;
      }
      void preloadRealData();
      void router.push({ name: firstAllowedPage(session.state.role) });
      return;
    }
    session.login(accountName);
    void router.push({ name: firstAllowedPage(session.state.role) });
  } finally {
    submitting.value = false;
  }
}
</script>

<style scoped>
/* 冰川主题（设计权威 gateway-v2a-glacier.html · 方案 A 亮色洁净派）：
   底色取自 --bg-app / --sidebar-bg-2，卡片白底 --bg-card + 16px 圆角，
   主按钮 --brand(#17C3B2)，字体沿用 body 的 Sora 栈，色值一律走 token 不硬编码。 */
.ac-login {
  min-height: 100vh;
  display: flex;
  align-items: center;
  justify-content: center;
  padding: 24px;
  background: linear-gradient(165deg, var(--sidebar-bg-2) 0%, var(--bg-app) 62%);
}
.ac-login__card {
  width: 400px;
  max-width: 100%;
  background: var(--bg-card);
  border: 1px solid var(--border);
  border-radius: var(--radius);
  box-shadow: var(--shadow-2);
  padding: 28px 28px 24px;
  display: flex;
  flex-direction: column;
  gap: 16px;
}
.ac-login__brand {
  display: flex;
  align-items: center;
  gap: 10px;
  font-weight: 600;
  font-size: 16px;
  color: var(--text-1);
}
.ac-login__desc {
  margin: 0;
  margin-top: -10px;
  font-size: var(--fs-caption);
  color: var(--text-3);
  line-height: 1.6;
}
.ac-login__fields {
  display: flex;
  flex-direction: column;
  gap: 12px;
}
.ac-login__submit {
  width: 100%;
  min-height: 38px;
  border-radius: var(--radius-btn);
  font-size: var(--fs-body);
}
</style>
