<template>
  <!--
    LoginPage —— 管理员会话登录（页面清单第 1 项）。
    真实系统应接后台自身鉴权（licensing-api.md §4：管理员登录态 + 超时）；
    本原型仅做最小可用登录，任何非空账号均可进入，用于演示后续 RBAC。
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
        <UiField :label="isReal ? '密码（探测后端鉴权）' : '密码'" required :hint="passwordHint">
          <UiInput v-model="password" type="password" placeholder="请输入密码" />
        </UiField>
        <!-- real 模式独有：默认租户 ID（发放 / 废弃 / 重发请求的租户来源） -->
        <UiField
          v-if="isReal"
          label="默认租户 ID"
          required
          hint="后端未提供租户列表端点（缺口 #2），请直接填写 licensing-server 中的租户 ID（如 t-1）"
        >
          <UiInput v-model="tenantId" placeholder="t-1" />
        </UiField>
      </div>

      <p v-if="error" class="ac-login__err" role="alert">{{ error }}</p>

      <button type="submit" class="ac-btn ac-btn--primary" :disabled="!canSubmit">登录</button>

      <div class="ac-hint">
        登录后可在顶栏切换「运营 / 授权运营 / 风控 / 系统」四种角色，
        用于验证页面级与操作级权限门控（无权角色下菜单隐藏、高危按钮禁用）。
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
 * real 模式鉴权策略（后端缺口 #1 的优雅降级，绝不伪造「已鉴权」假象）：
 *  1. 先按 licensing-api.md §4 契约探测 `POST /admin/auth/login`；
 *  2. `401 / 403` → 后端已启用鉴权但凭证错误 → 行内报错，不放行；
 *  3. `404 / 405`（端点缺失）→ 以本地会话放行，同时推全局横幅说明「后端未鉴权，
 *     操作者以 X-Actor-Id 头标识」；
 *  4. 网络不通（status 0）→ 行内报错，不放行（绝不静默假登录）。
 * 登录成功后立即跳转并触发 `preloadRealData()`（mount 不等 preload——联调 P0 教训）。
 */
import { computed, ref } from 'vue';
import { useRouter } from 'vue-router';
import { UiField, UiInput, firstAllowedPage } from '@ui-kit';
import { session } from '../store/session';
import { API_MODE, ApiError, adminLogin } from '../api/client';
import { preloadRealData, pushNotice } from '../api/repo';

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

/** 密码框提示（随模式变化）。 */
const passwordHint = computed(() =>
  isReal ? '后端未实现登录端点时任意 ≥6 位可进入（见横幅说明）' : '原型环境：任意 ≥6 位字符即可登录',
);

/** 可提交条件（两项都非空；real 模式还要求租户 ID 非空）。 */
const canSubmit = computed(
  () => account.value.trim().length > 0 && password.value.length >= 6 && (!isReal || tenantId.value.trim().length > 0),
);

/** real 模式登录探测（返回是否放行）。 */
async function submitReal(accountName: string): Promise<boolean> {
  try {
    await adminLogin(accountName, password.value);
    // 后端未来实现登录端点且凭证正确：正常放行
    return true;
  } catch (cause) {
    const status = cause instanceof ApiError ? cause.status : -1;
    if (status === 401 || status === 403) {
      error.value = `账号或密码错误（HTTP ${status}，后端管理员鉴权已启用）`;
      return false;
    }
    if (status === 0) {
      error.value = cause instanceof Error ? cause.message : '网络请求失败';
      return false;
    }
    // 404 / 405 / 其他：登录端点缺失（缺口 #1）——清晰降级进入，绝不伪装已鉴权
    pushNotice(
      'warn',
      `后端未提供管理员登录 / RBAC 端点（缺口 #1，探测 HTTP ${status}）：已以本地会话进入；操作者以 X-Actor-Id 头标识，licensing-server 当前对 /admin/* 无鉴权。`,
    );
    return true;
  }
}

/** 登录：写入会话并跳转到该角色首个可见页面（登录后立即跳转，不等 preload）。 */
async function submit(): Promise<void> {
  if (!canSubmit.value) {
    error.value = isReal
      ? '请填写管理员账号、密码（≥6 位）与默认租户 ID'
      : '请填写管理员账号与密码（密码至少 6 位）';
    return;
  }
  error.value = '';
  const accountName = account.value.trim();
  if (isReal) {
    const allowed = await submitReal(accountName);
    if (!allowed) {
      return;
    }
    session.login(accountName, tenantId.value.trim());
    void preloadRealData();
    void router.push({ name: firstAllowedPage(session.state.role) });
    return;
  }
  session.login(accountName);
  void router.push({ name: firstAllowedPage(session.state.role) });
}
</script>
