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
        <UiField label="密码" required hint="原型环境：任意 ≥6 位字符即可登录">
          <UiInput v-model="password" type="password" placeholder="请输入密码" />
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
 */
import { computed, ref } from 'vue';
import { useRouter } from 'vue-router';
import { UiField, UiInput, firstAllowedPage } from '@ui-kit';
import { session } from '../store/session';

const router = useRouter();

/** 账号草稿。 */
const account = ref('li.gong');
/** 密码草稿。 */
const password = ref('');
/** 行内错误。 */
const error = ref('');

/** 可提交条件（两项都非空）。 */
const canSubmit = computed(() => account.value.trim().length > 0 && password.value.length >= 6);

/** 登录：写入会话并跳转到该角色首个可见页面。 */
function submit(): void {
  if (!canSubmit.value) {
    error.value = '请填写管理员账号与密码（密码至少 6 位）';
    return;
  }
  error.value = '';
  session.login(account.value.trim());
  void router.push({ name: firstAllowedPage(session.state.role) });
}
</script>
