<template>
  <!--
    LoginPage —— 管理员会话登录（页面清单第 1 项）。
    real 模式：调 `POST /admin/auth/login`（后端已落地管理员鉴权），成功存
    token + role（localStorage）并跳转控制台；失败显示后端错误消息。
    mock 模式：保持原型最小可用登录（任意非空账号 + ≥6 位密码），行为零回归。
  -->
  <div class="ac-login">
    <form class="ac-login__card" @submit.prevent="submit">
      <div class="ac-login__brand">
        <span class="ac-brand__logo">LIC</span>
        <span>IoT-DAQ 授权管理后台</span>
      </div>
      <p class="ac-login__desc">
        厂商侧内部系统 · 仅授权运营人员使用<br />
        所有写操作将记入审计日志（操作者 / 对象 / 原因 / 来源 IP / 结果）
      </p>

      <div class="ac-login__fields">
        <UiField label="管理员账号" required :error="error">
          <UiInput v-model="account" placeholder="如 li.gong" :invalid="error.length > 0" />
        </UiField>
        <UiField :label="isReal ? '密码（后端管理员鉴权）' : '密码'" required :hint="passwordHint">
          <UiInput v-model="password" type="password" placeholder="请输入密码" />
        </UiField>
        <!-- real 模式独有：默认租户 ID（发放 / 废弃 / 重发请求的租户来源） -->
        <UiField
          v-if="isReal"
          label="默认租户 ID"
          required
          hint="写端点 X-Tenant-Id 头的兜底租户；请填写 licensing-server 中的租户 ID（如 t-1）"
        >
          <UiInput v-model="tenantId" placeholder="t-1" />
        </UiField>
      </div>

      <p v-if="error" class="ac-login__err" role="alert">{{ error }}</p>

      <button type="submit" class="ac-btn ac-btn--primary" :disabled="submitting || !canSubmit">
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
 * @description 登录页。草稿（账号/密码）为页面局部状态，登录成功后交给 session 并跳转。
 *
 * real 模式鉴权（契约 = crates/licensing-server/src/admin_auth.rs）：
 *  1. `POST /admin/auth/login` → `{token, role}`（HS256 JWT，1h TTL）；
 *  2. 成功：token + role 持久化（localStorage），session 记录后端签发角色并跳转；
 *  3. `401` → 凭证错误，行内报错，不放行（后端不区分原因，防账号枚举）；
 *  4. 网络不通（status 0）/ 其他错误 → 行内显示后端消息，不放行（绝不静默假登录）。
 * 登录成功后立即跳转并触发 `preloadRealData()`（mount 不等 preload——联调 P0 教训）。
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
/** 行内错误。 */
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

/** real 模式登录（返回是否放行）。 */
async function submitReal(accountName: string): Promise<boolean> {
  try {
    const { token, role } = await adminLogin(accountName, password.value);
    session.login(accountName, tenantId.value.trim(), token, role);
    return true;
  } catch (cause) {
    const status = cause instanceof ApiError ? cause.status : -1;
    if (status === 401 || status === 403) {
      error.value = '账号或密码错误（后端管理员鉴权已启用）';
      return false;
    }
    error.value = cause instanceof Error ? cause.message : '登录失败，请稍后重试';
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
