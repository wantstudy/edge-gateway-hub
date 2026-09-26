<template>
  <!--
    LoginPage —— 登录页（仅 real 模式可达，见 router.ts 守卫）。

    设计对齐：方案 A·冰川（`docs/design/prototype/gateway-v2a-glacier.html`）
      · 左：深海军蓝品牌栏（#0F1B3D）+ 青绿品牌块（#17C3B2）；
      · 右：纯白大圆角卡（radius 16px，Sora 标题字）；
      · 明暗双主题：底色/文字走 CSS 变量，品牌色、圆角、字体两态同值（见 ui-kit/theme.ts）。

    双形态（首次初始化）：
      · `initialized`  → 常规登录（默认）；
      · `uninitialized` → 展示「创建首个管理员」主入口（由 `GET /api/auth/state` 的
                          `initialized === false` 驱动，字段语义见下方 AuthStateResponse）；
      · `unavailable`   → 网关尚未提供该查询接口时，保持**诚实空态**：
                          不伪造「试用」按钮，仅说明原因，登录表单照常可用。

    错误语义：401 → 「账号或密码错误」；403 → 权限不足；status 0 → 连不上 daemon；
              其余透传后端 message。网络失败 / 5xx 不崩页面。
  -->
  <div class="wc-login">
    <!-- 左：品牌栏（深海军蓝，A·冰川 :20,:77-100） -->
    <aside class="wc-login__panel">
      <div class="wc-login__panel-top">
        <span class="wc-login__logo">GW</span>
        <div class="wc-login__brand-text">
          <strong>IoT-DAQ</strong>
          <span>数据网关控制台</span>
        </div>
      </div>

      <div class="wc-login__panel-mid">
        <h2 class="wc-login__slogan">
          让每一条产线数据<br />
          可信、可达、可审计
        </h2>
        <p class="wc-login__panel-desc">
          工业边缘数据汇聚网关 · 本机控制台。所有操作留痕，授权状态常驻可见。
        </p>
      </div>

      <div class="wc-login__panel-foot">
        <span class="wc-login__panel-dot">●</span>
        <span>{{ gatewayName }}</span>
      </div>
    </aside>

    <!-- 右：登录卡（纯白大圆角） -->
    <main class="wc-login__side">
      <div class="wc-login__card">
        <div v-if="authState === 'initialized'" class="wc-login__head">
          <h1 class="wc-login__title">登录控制台</h1>
          <p class="wc-login__sub">使用网关账号登录以继续</p>
        </div>

        <!-- 未初始化：首个管理员入口（由 `GET /api/auth/state` 的 initialized === false 驱动） -->
        <div v-if="authState === 'uninitialized'" class="wc-login__head">
          <h1 class="wc-login__title">初始化网关</h1>
          <p class="wc-login__sub">本机尚无任何账号，请先创建首个管理员</p>
        </div>

        <!-- 接口未就绪 / 查询中 / 已初始化：主标题 + 可选说明 -->
        <div v-else class="wc-login__head">
          <h1 class="wc-login__title">
            {{ authState === 'loading' ? '正在查询初始化状态…' : authState === 'unavailable' ? '登录控制台' : '登录控制台' }}
          </h1>
          <p v-if="authState === 'unavailable'" class="wc-login__sub">
            网关未提供初始化状态查询接口
            <code class="wc-mono">GET /api/auth/state</code>
            ，无法判断本机是否需要先创建首个管理员；可直接用已有账号登录。
          </p>
          <p
            v-if="authNote"
            class="wc-login__note"
            data-testid="login-state-note"
          >
            {{ authNote }}
          </p>
        </div>

        <a-form
          :model="form"
          layout="vertical"
          class="wc-login__form"
        >
          <a-form-item field="username" label="账号" required>
            <a-input
              v-model="form.username"
              placeholder="请输入账号"
              allow-clear
              data-testid="login-username"
              @keyup.enter="onSubmit"
            />
          </a-form-item>
          <a-form-item field="password" label="密码" required>
            <a-input-password
              v-model="form.password"
              placeholder="请输入密码"
              data-testid="login-password"
              @keyup.enter="onSubmit"
            />
          </a-form-item>

          <a-alert v-if="errorMsg" type="error" class="wc-login__alert">{{ errorMsg }}</a-alert>

          <!-- 未初始化：主入口切换为「创建首个管理员」 -->
          <a-button
            v-if="authState === 'uninitialized'"
            type="primary"
            long
            size="large"
            class="wc-login__submit"
            data-testid="login-bootstrap"
            :loading="bootstrapLoading"
            @click="onBootstrap"
          >
            创建首个管理员
          </a-button>
          <template v-else>
            <a-button
              type="primary"
              long
              size="large"
              class="wc-login__submit"
              data-testid="login-submit"
              :loading="loading"
              @click="onSubmit"
            >
              登 录
            </a-button>
            <!-- 未就绪时提供一次显式重试（不伪造结果） -->
            <button
              v-if="authState === 'unavailable'"
              type="button"
              class="wc-login__retry"
              data-testid="login-retry-state"
              @click="loadAuthState"
            >
              重新查询初始化状态
            </button>
          </template>
        </a-form>
      </div>
    </main>
  </div>
</template>

<script setup lang="ts">
/**
 * @file LoginPage.vue
 * @module web-console/pages/login
 * @description 登录页：real 模式下 401 / 未登录跳转至此；成功后写 session + 预取真实数据并回跳。
 */
import { onMounted, reactive, ref, watch } from 'vue';
import { useRoute, useRouter } from 'vue-router';
import { ApiError, apiLogin, setStoredBackendRole, setStoredToken } from '@/api/client';
import { dataVersion, preloadRealData, repo } from '@/api/repo';
import { mapBackendRole, session } from '@/store/session';

const route = useRoute();
const router = useRouter();

/** 表单草稿（正式环境：不预填任何账号）。 */
const form = reactive({ username: '', password: '' });

/** 提交中状态（按钮 loading + 防重复提交）。 */
const loading = ref(false);

/** 初始化创建中状态。 */
const bootstrapLoading = ref(false);

/** 错误提示（401 / 网络 / 5xx）。 */
const errorMsg = ref('');

// ---------------------------------------------------------------------------
// 首次初始化状态（真实接口驱动，禁止猜测）
// ---------------------------------------------------------------------------

/**
 * `GET /api/auth/state` 响应（免认证、无副作用；字段语义与 daemon
 * `crates/daemon/src/mgmt/auth_login.rs` 的 `auth_state` 一致）。
 *
 * `note` 仅在上游拿不到授权运行期时附带如实原因（如
 * `license runtime not assembled; trial state unknown`），前端只原样透出，不翻译、不编造。
 */
interface AuthStateResponse {
  initialized?: boolean;
  accounts?: number;
  roles?: number;
  trial_enabled?: boolean;
  trial_days_left?: number;
  server_time?: string;
  note?: string;
}

/**
 * 初始化探测状态。
 *
 * - `loading`         查询中；
 * - `initialized`    网关已有账号（后端 `initialized === true`）→ 常规登录；
 * - `uninitialized`  网关无任何账号 → 首要任务是创建首个管理员；
 * - `unavailable`    `GET /api/auth/state` 尚未随网关提供（契约落地前的 daemon 会
 *                    返回 404）→ 诚实空态：不伪造「试用」入口，仅说明原因。
 */
type AuthState = 'loading' | 'initialized' | 'uninitialized' | 'unavailable';

/** 当前初始化状态（挂载时探测一次）。 */
const authState = ref<AuthState>('loading');

/** `GET /api/auth/state` 附带的如实说明（无 note 字段时为空串）。 */
const authNote = ref('');

/** 网关名（真实值；探测失败时显示「网关标识取得中」，绝不写死演示名）。 */
const gatewayName = ref<string>('网关标识取得中');

/** 探测 `GET /api/auth/state`（免认证；契约未落地时后端返回 404 → unavailable）。 */
async function loadAuthState(): Promise<void> {
  authState.value = 'loading';
  authNote.value = '';
  try {
    const res = await fetch('/api/auth/state', { headers: { Accept: 'application/json' } });
    if (res.ok) {
      const body = (await res.json()) as AuthStateResponse;
      authNote.value = typeof body.note === 'string' ? body.note : '';
      // 字段缺失按「已初始化」处理：宁可多一步登录，也不能跳过一次性写入闸门
      authState.value = body.initialized === false ? 'uninitialized' : 'initialized';
      return;
    }
    authState.value = 'unavailable';
  } catch {
    // 网络层失败（daemon 未起 / 代理中断）同样按「不可判定」处理，不伪造状态
    authState.value = 'unavailable';
  }
}

onMounted(() => {
  void loadAuthState();
  refreshGatewayName();
});

/** 预取回填真实网关名（未取到时保持「网关标识取得中」，绝不写死演示名）。 */
watch(dataVersion, refreshGatewayName);

/** 网关标识（真实值；`/api/overview` 的 name 即 gateway_id，随预取结果刷新）。 */
function refreshGatewayName(): void {
  const name = repo.getGateway().name;
  // 「—」是 repo 的诚实空值占位，不能当作已取得的真实标识展示
  if (name && name !== '—') {
    gatewayName.value = name;
  }
}

/**
 * 创建首个管理员（`POST /api/auth/bootstrap`）。
 *
 * 后端契约（`auth_login.rs` 的 `bootstrap`）的关键分支 → 前端如实呈现，不重试、不兜底：
 * - 400 入参非法（账号空 / 口令 < 8）→ 透传后端原因；
 * - 403 非回环来源 → 入口在本网桥上根本不该出现；
 * - 409 `already_initialized` → 账号体系已初始化，一次性写入闸门永久关闭；
 * - 其余透传后端 `message`。
 *
 * 成功后后端**直接签发 token**（与 login 同构），因此沿用同一条登录成功链路。
 */
async function onBootstrap(): Promise<void> {
  if (bootstrapLoading.value || loading.value) {
    return;
  }
  const username = form.username.trim();
  if (!username || form.password.length < 8) {
    errorMsg.value = '首个管理员需要账号，以及至少 8 位口令';
    return;
  }
  bootstrapLoading.value = true;
  errorMsg.value = '';
  try {
    const res = await fetch('/api/auth/bootstrap', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ username, password: form.password }),
    });
    if (!res.ok) {
      const text = await res.text().catch(() => '');
      throw new ApiError(res.status, `创建失败（HTTP ${res.status}）`, text || null);
    }
    const body = (await res.json()) as { token?: string; role?: string };
    if (!body.token) {
      throw new ApiError(res.status, '网关未返回登录凭据，请改用已有账号登录');
    }
    setStoredToken(body.token);
    setStoredBackendRole(body.role ?? 'system');
    session.login(username, {
      backendRole: body.role ?? 'system',
      role: mapBackendRole(body.role ?? 'system'),
    });
    refreshGatewayName();
    authState.value = 'initialized';
    errorMsg.value = '';
    void preloadRealData().catch((cause: unknown) => {
      console.warn('[web-console] 初始化后预取真实数据失败，页面按诚实空态呈现', cause);
    });
    const redirect = typeof route.query.redirect === 'string' && route.query.redirect ? route.query.redirect : '/overview';
    await router.replace(redirect);
  } catch (cause) {
    errorMsg.value = describeBootstrapFailure(cause);
  } finally {
    bootstrapLoading.value = false;
  }
}

/** bootstrap 失败文案：状态码 → 人话，其余透传后端 `message`。 */
function describeBootstrapFailure(cause: unknown): string {
  if (!(cause instanceof ApiError)) {
    return '创建首个管理员失败，请稍后重试';
  }
  const backend = () => {
    try {
      const parsed = typeof cause.body === 'string' ? JSON.parse(cause.body) : cause.body;
      const message = (parsed as { message?: string } | null)?.message;
      return typeof message === 'string' ? message : '';
    } catch {
      return '';
    }
  };
  switch (cause.status) {
    case 400:
      return backend() || '账号或口令不满足要求（账号非空，口令至少 8 位）';
    case 403:
      return backend() || '该操作仅允许本机回环调用';
    case 409:
      return backend() || '账号体系已初始化，创建首个管理员的入口已永久关闭';
    default:
      return cause.message;
  }
}

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
    refreshGatewayName();
    //
    // D-01 修复：登录成功后**立即回跳**，preload 转后台执行。
    // 旧实现 `await preloadRealData()` 混入了 `/api/events`（无限 SSE 流，
    // `res.text()` 永不 resolve）→ 登录按钮永久 loading、页面卡在登录页。
    // preload 内部另有 8s 超时护栏，完成后由 `dataVersion` 驱动页面刷新。
    void preloadRealData().catch((cause: unknown) => {
      console.warn('[web-console] 登录后预取真实数据失败，页面按诚实空态呈现', cause);
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
/* 全屏登录布局：左侧品牌栏 + 右侧登录卡（A·冰川）。颜色全部取自 ui-kit token。 */
.wc-login {
  display: flex;
  min-height: 100vh;
  min-width: 1180px;
  background: var(--bg-app);
}

/* ---------- 左：深海军蓝品牌栏（原型 :20,:77-105） ----------
   注意：这是**登录页的品牌装饰块**，与导航侧栏不同源。
   `--sidebar-*` 是导航侧栏 token（明色态已改为浅底），此处在元素上显式复位为
   深海军蓝，使左栏昼夜切换时保持稳定，且不会读成「浅底上的浅字」。 */
.wc-login__panel {
  --sidebar-bg: #0f1b3d;
  --sidebar-bg-2: #16244c;
  --sidebar-line: rgba(255, 255, 255, 0.08);
  --sidebar-hover: rgba(255, 255, 255, 0.07);
  --sidebar-text: #b9c2dc;
  --sidebar-text-active: #ffffff;
  --sidebar-muted: #8a94b8;
  flex: 0 0 400px;
  display: flex;
  flex-direction: column;
  justify-content: space-between;
  padding: 40px 40px 32px;
  color: var(--sidebar-text);
  background:
    linear-gradient(180deg, var(--sidebar-bg-2) 0%, var(--sidebar-bg) 46%, var(--sidebar-bg) 100%);
}
.wc-login__panel-top {
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
  flex: 0 0 40px;
  border-radius: var(--radius-sm);
  background: var(--brand);
  color: #04241f;
  font-size: 16px;
  font-weight: 700;
  letter-spacing: -0.02em;
}
.wc-login__brand-text {
  display: flex;
  flex-direction: column;
  line-height: 1.35;
}
.wc-login__brand-text strong {
  font-family: var(--font-display);
  font-size: 16px;
  font-weight: 600;
  letter-spacing: 0.02em;
  color: #ffffff;
}
.wc-login__brand-text span {
  font-size: 11px;
  letter-spacing: 0.14em;
  text-transform: uppercase;
  color: var(--sidebar-muted);
}
.wc-login__panel-mid {
  padding: 8px 0;
}
.wc-login__slogan {
  margin: 0 0 14px;
  font-family: var(--font-display);
  font-size: 28px;
  line-height: 1.35;
  font-weight: 600;
  color: #ffffff;
}
.wc-login__panel-desc {
  margin: 0;
  max-width: 300px;
  font-size: var(--fs-caption);
  line-height: 1.8;
  color: var(--sidebar-text);
}
.wc-login__panel-foot {
  display: flex;
  align-items: center;
  gap: 8px;
  padding-top: 18px;
  border-top: 1px solid var(--sidebar-line);
  font-size: var(--fs-caption);
  color: var(--sidebar-muted);
}
.wc-login__panel-dot {
  color: var(--brand);
  font-size: 9px;
  line-height: 1;
}

/* ---------- 右：登录卡 ---------- */
.wc-login__side {
  flex: 1;
  display: flex;
  align-items: center;
  justify-content: center;
  padding: 40px;
}
.wc-login__card {
  width: 380px;
  padding: 36px 32px 28px;
  background: var(--bg-card);
  border: 1px solid var(--border);
  border-radius: var(--radius);
  box-shadow: var(--shadow-2);
}
.wc-login__head {
  margin-bottom: 22px;
}
.wc-login__title {
  margin: 0 0 6px;
  font-family: var(--font-display);
  font-size: 22px;
  font-weight: 600;
  color: var(--text-1);
}
.wc-login__sub {
  margin: 0;
  font-size: var(--fs-caption);
  line-height: 1.7;
  color: var(--text-3);
}
.wc-login__form {
  display: flex;
  flex-direction: column;
}
.wc-login__alert {
  margin-bottom: 16px;
}
.wc-login__submit {
  margin-top: 6px;
}
/* 初始化状态查询附带的如实说明（后端 note 原文，不翻译不编造） */
.wc-login__note {
  margin: 10px 0 0;
  padding: 8px 10px;
  border-radius: var(--radius-sm);
  border: 1px solid var(--warn-border);
  background: var(--warn-bg);
  color: var(--warn-fg);
  font-size: var(--fs-caption);
  line-height: 1.6;
}
.wc-login__retry {
  margin-top: 12px;
  padding: 4px 0;
  border: 0;
  background: none;
  font-family: inherit;
  font-size: var(--fs-caption);
  color: var(--brand);
  cursor: pointer;
  text-decoration: underline;
}
</style>
