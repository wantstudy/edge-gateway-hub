<template>
  <!--
    StartupPage —— 启动与自启（运维分组，路由 `/startup`）。

    真实能力边界（不许造数）：
      · 运行信息取自 `GET /api/overview`（部署形态 / 主机名 / 端口 / 版本 / 标识）；
      · 容器 / 原生形态识别等能力由后端按需提供，页面只展示真实可得字段；
      · 守护进程（崩溃重启 / 看门狗 / 启动失败保护）与异常重启累计（restarts）
        为桌面端能力，浏览器访问时诚实降级为「需在桌面端中使用」；
      · 计划重启（定时 / 周期）后端暂无调度能力，页面呈现诚实空态（不放假开关）；
      · 危险动作契约不变（P0-6）：重启 / 停止走 ui-kit `DangerConfirmModal`
        —— 影响清单 + 原因必填 + **服务名二次校验**；real 模式调真实
        `POST /api/ops/restart` / `POST /api/ops/stop`，body `{actor, confirm, reason}`，
        `confirm` 由页面自动回显网关标识（`/api/overview` 的 `name` = gateway_id）。
  -->
  <div class="wc-content">
    <!-- KPI 四卡 -->
    <div class="wc-grid wc-grid--4">
      <StatCard label="服务状态" :value="serviceStatusText" :sub="serviceStatusSub" icon-tone="teal">
        <template #icon>▶</template>
      </StatCard>
      <StatCard label="已运行" :value="gateway.uptimeText" sub="自网关启动起累计" icon-tone="ink">
        <template #icon>◷</template>
      </StatCard>
      <StatCard label="上次启动" :value="startedAtText" sub="随网关进程启动" icon-tone="amber">
        <template #icon>↻</template>
      </StatCard>
      <StatCard label="异常重启" :value="crashRestartText" :sub="crashRestartSub" tone="warn" icon-tone="violet">
        <template #icon>!</template>
      </StatCard>
    </div>

    <!-- 运行信息（真实字段） -->
    <section class="wc-card">
      <div class="wc-card__head">
        <h3>运行信息</h3>
        <span class="wc-tag wc-tag--neutral">实时运行数据</span>
      </div>
      <div class="wc-card__body">
        <dl class="wc-kv">
          <dt>网关标识</dt>
          <dd class="wc-mono">{{ gateway.name || '—' }}</dd>
          <dt>部署形态</dt>
          <dd class="wc-mono">{{ gateway.deployMode || '—' }}</dd>
          <dt>主机名</dt>
          <dd class="wc-mono">{{ gateway.hostname || '—' }}</dd>
          <dt>管理端口</dt>
          <dd class="wc-mono">{{ gateway.port || '—' }}</dd>
          <dt>版本</dt>
          <dd class="wc-mono">{{ gateway.version || '—' }}</dd>
        </dl>
      </div>
    </section>

    <div class="wc-grid wc-grid--2">
      <!-- 启动策略（自启写入能力以真实 GET / PUT /api/service/autostart 为准） -->
      <section class="wc-card">
        <div class="wc-card__head">
          <h3>启动策略</h3>
          <span class="wc-tag" :class="autostartCapability.cls">{{ autostartCapability.text }}</span>
        </div>
        <div class="wc-card__body">
          <div class="su-row">
            <div class="su-row__text">
              <div class="su-row__title">
                开机自启
                <span class="wc-tag wc-tag--neutral" data-testid="autostart-state">{{ autostartStateText }}</span>
              </div>
              <div class="su-row__desc">
                {{ autostartDesc }}
              </div>
            </div>
            <!-- 容器形态：自启由宿主 docker.service + compose restart 策略托管，
                 网关在容器内无权也无能力改写 → 不给开关，只给只读说明（诚实空态）。 -->
            <RoleGate v-if="!isContainerForm" :allowed="canManage">
              <UiSwitch
                v-model="autostartEnabled"
                on-text="已启用"
                off-text="已关闭"
                :disabled="autostartBusy || !autostartWritable"
                @update:model-value="onAutostart"
              />
            </RoleGate>
            <span v-if="!isContainerForm && !canManage" class="wc-tag wc-tag--neutral">仅管理员可改</span>
            <span v-else-if="isContainerForm" class="wc-tag wc-tag--neutral">由容器编排托管</span>
          </div>
          <p v-if="isContainerForm && autostartGuaranteeHint" class="su-hint">
            {{ autostartGuaranteeHint }}
          </p>

          <div class="su-row">
            <div class="su-row__text">
              <div class="su-row__title">崩溃自动重启</div>
              <div class="su-row__desc">进程异常退出后自动重启，指数退避（1s→2s→4s，上限 60s）。</div>
            </div>
            <RoleGate :allowed="canManage">
              <UiSwitch
                :model-value="crashRestart"
                :disabled="!shellAvailable || svBusy"
                on-text="已启用"
                off-text="已关闭"
                @update:model-value="(v: boolean) => onSupervisor('crash_restart', v)"
              />
            </RoleGate>
            <span v-if="!shellAvailable" class="wc-tag wc-tag--neutral">桌面端能力</span>
          </div>

          <div class="su-row">
            <div class="su-row__text">
              <div class="su-row__title">看门狗</div>
              <div class="su-row__desc">心跳超时 90s 判定为假死，自动重启进程（panic 隔离）。</div>
            </div>
            <RoleGate :allowed="canManage">
              <UiSwitch
                :model-value="watchdog"
                :disabled="!shellAvailable || svBusy"
                on-text="已启用"
                off-text="已关闭"
                @update:model-value="(v: boolean) => onSupervisor('watchdog', v)"
              />
            </RoleGate>
            <span v-if="!shellAvailable" class="wc-tag wc-tag--neutral">桌面端能力</span>
          </div>

          <div class="su-row">
            <div class="su-row__text">
              <div class="su-row__title">启动失败保护</div>
              <div class="su-row__desc">连续 5 次启动失败后暂停自启并触发告警，避免无限重启循环。</div>
            </div>
            <RoleGate :allowed="canManage">
              <UiSwitch
                :model-value="bootFailureGuard"
                :disabled="!shellAvailable || svBusy"
                on-text="已启用"
                off-text="已关闭"
                @update:model-value="(v: boolean) => onSupervisor('boot_failure_guard', v)"
              />
            </RoleGate>
            <span v-if="!shellAvailable" class="wc-tag wc-tag--neutral">桌面端能力</span>
          </div>

          <p v-if="shellAvailable && supervisor && supervisor.supported" class="su-sv">
            守护状态：运行中 {{ supervisor.running ? '是' : '否' }} · 已重启 {{ supervisor.restarts }} 次 ·
            连续失败 {{ supervisor.consecutive_failures }} 次 · 上次退出 {{ supervisor.last_exit || '—' }}
          </p>
          <p v-else-if="shellAvailable && supervisor" class="su-hint">
            当前安装未随包分发守护能力{{ supervisor.reason ? `：${supervisor.reason}` : '' }}
          </p>
          <p v-else-if="!shellAvailable" class="su-hint">
            崩溃自动重启 / 看门狗 / 启动失败保护需在桌面端（IoT-DAQ Gateway）中使用，浏览器访问时状态不可得。
          </p>
        </div>
      </section>

      <!-- 计划重启（后端暂无定时/周期重启能力，诚实空态；立即重启见下方「危险操作」） -->
      <section class="wc-card">
        <div class="wc-card__head">
          <h3>计划重启</h3>
          <span class="wc-tag wc-tag--neutral">未开放</span>
        </div>
        <div class="wc-card__body">
          <EmptyState
            title="计划重启暂未开放"
            desc="网关目前支持手动立即重启（见下方「危险操作」）。定时 / 周期重启需要后端调度能力，将在后续版本提供。"
          />
        </div>
      </section>
    </div>

    <!-- 危险操作 -->
    <section class="wc-card">
      <div class="wc-card__head">
        <h3>危险操作</h3>
        <span class="wc-card__sub">需二次确认 + 原因 + 网关标识校验</span>
      </div>
      <div class="wc-card__body">
        <div class="su-impact">
          <p class="su-impact__title"><span aria-hidden="true">⚠</span>重启与停止服务的影响</p>
          <ul>
            <li>重启期间<b>采集暂停约 5–15 秒</b>；已入队数据不会丢失，恢复后自动补发。</li>
            <li>停止服务后<b>北向转发中断</b>，且不再自动启动（除非自启开启）。</li>
            <li>操作需二次确认并填写原因，同时写入审计日志。</li>
          </ul>
        </div>

        <div class="su-ops">
          <button type="button" class="wc-btn wc-btn--danger" data-testid="startup-restart" @click="openRestart">立即重启服务</button>
          <button type="button" class="wc-btn wc-btn--danger" data-testid="startup-stop" @click="openStop">{{ stopButtonText }}</button>
          <p v-if="isContainerForm" class="su-hint">
            容器部署下「停止」只会让容器内进程退出，compose 的 restart 策略会立即把容器拉起；
            如需彻底停止，请在宿主执行 <code>docker compose down</code>。
          </p>
        </div>

        <div v-if="lastAction" class="su-result" :class="`is-${lastKind}`">
          <span aria-hidden="true">{{ lastKind === 'ok' ? '✓' : '⚠' }}</span>
          <span>{{ lastAction }}</span>
        </div>
      </div>
    </section>
  </div>

  <!-- 重启服务：影响清单 + 原因必填 + gateway_id 全名二次校验 -->
  <DangerConfirmModal
    :open="restartModal.open"
    :title="`重启服务 · ${gateway.name || '网关'}`"
    :impacts="restartModal.impacts"
    :facts="restartModal.facts"
    :reasons="RESTART_REASONS"
    :min-note-length="10"
    confirm-mode="full"
    :confirm-value="gatewayId"
    confirm-label="风险二次确认（输入网关标识全名）"
    :confirm-placeholder="`输入网关标识 ${gatewayId || '（当前不可得）'} 以确认`"
    confirm-text="确认重启"
    @close="restartModal.open = false"
    @submit="confirmRestart"
  />

  <!-- 停止服务：原因必填 + gateway_id 全名二次校验 -->
  <DangerConfirmModal
    :open="stopModal.open"
    :title="`停止服务 · ${gateway.name || '网关'}`"
    :impacts="stopModal.impacts"
    :facts="stopModal.facts"
    :reasons="STOP_REASONS"
    :min-note-length="10"
    confirm-mode="full"
    :confirm-value="gatewayId"
    confirm-label="风险二次确认（输入网关标识全名）"
    :confirm-placeholder="`输入网关标识 ${gatewayId || '（当前不可得）'} 以确认`"
    confirm-text="确认停止"
    @close="stopModal.open = false"
    @submit="confirmStop"
  />
</template>

<script setup lang="ts">
/**
 * @file StartupPage.vue
 * @module web-console/pages/StartupPage
 * @description 启动与自启：运行信息（真实）+ 启动策略（守护开关接桌面壳）+ 高危运维动作。
 *
 * 危险动作一律经 `DangerConfirmModal`（影响清单 + 原因必填 + 服务名二次校验），
 * 确认后调真实 `repo.ops.restart` / `repo.ops.stop`（body {actor, confirm, reason}，
 * confirm 自动回显网关标识），后端结果原样呈现。未落地的能力一律显式留空，不给假数据。
 */
import { computed, onMounted, reactive, ref, watch } from 'vue';
import {
  StatCard,
  RoleGate,
  UiSwitch,
  DangerConfirmModal,
  EmptyState,
} from '@ui-kit';
import { dataVersion, repo, type AutostartStatus, type GatewayInfo } from '@/api/repo';
import { shellAvailable, supervisorStatus, setSupervisor, type SupervisorStatus } from '@/api/shell';
import { session } from '../store/session';

/** 网关信息（真实：`GET /api/overview`）。 */
const gateway = ref<GatewayInfo>(repo.getGateway());
watch(dataVersion, () => {
  gateway.value = repo.getGateway();
});

/**
 * 二次校验锚点：后端冻结语义为 `confirm === gateway_id`（`remote_ops.rs`），
 * `/api/overview` 的 `name` 即当前 gateway_id（如 `gw-local-dev`）。
 * 前端只透传真实值供用户对照输入，**禁止硬编码**。
 */
const gatewayId = computed<string>(() => {
  const id = (gateway.value.name || '').trim();
  return id && id !== '—' ? id : '';
});

/** 上次启动时刻（epoch 秒字符串 → 本地时间文本；非法值原样展示）。 */
const startedAtText = computed<string>(() => {
  const raw = (gateway.value.startedAt || '').trim();
  if (!/^\d{1,15}$/.test(raw)) {
    return raw || '—';
  }
  const ms = Number(raw) * (raw.length <= 10 ? 1000 : 1);
  if (!Number.isFinite(ms)) {
    return raw;
  }
  const d = new Date(ms);
  const p = (n: number): string => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
});

/** 服务状态文案：以「是否拿到网关自检数据」为准，不假装知道进程状态。 */
const serviceStatusText = computed(() =>
  gateway.value.uptimeText && gateway.value.uptimeText !== '—' ? '运行中' : '未知',
);

/** 服务状态副文案。 */
const serviceStatusSub = computed(() => '最近一次自检应答正常');

/**
 * 异常重启卡：桌面端展示守护进程真实累计重启次数（`supervisor_status` 的
 * `restarts`），浏览器环境诚实降级为「需在桌面端中使用」，不伪造 0。
 */
const crashRestartText = computed<string>(() =>
  shellAvailable && supervisor.value ? String(supervisor.value.restarts) : '—',
);

/** 异常重启卡副文案（区分浏览器 / 桌面端未随包分发两种降级）。 */
const crashRestartSub = computed<string>(() => {
  if (!shellAvailable) {
    return '需在桌面端（IoT-DAQ Gateway）中使用';
  }
  const status = supervisor.value;
  if (!status) {
    return '守护状态不可得';
  }
  if (!status.supported) {
    return status.reason ? `当前安装未随包分发守护能力：${status.reason}` : '当前安装未随包分发守护能力';
  }
  return '守护进程崩溃重启累计次数';
});

/** 是否可管理系统级自启（仅管理员）。 */
const canManage = computed(() => session.state.role === 'admin');

// ---------------------------------------------------------------------------
// 启动策略（自启写入以真实 GET / PUT /api/service/autostart 为准）
// ---------------------------------------------------------------------------

const autostartEnabled = ref(true);

// ---------------------------------------------------------------------------
// 守护进程（崩溃重启 / 看门狗 / 启动失败保护）—— 桌面端真实能力，浏览器诚实降级
// ---------------------------------------------------------------------------

/** 守护真实状态（非桌面端为 null，开关禁用）。 */
const supervisor = ref<SupervisorStatus | null>(null);

/** 守护写进行中（防重复下发）。 */
const svBusy = ref(false);

/** 三个守护开关的当前值（由 supervisorStatus 真实填充，非假绑定）。 */
const crashRestart = ref(false);
const watchdog = ref(false);
const bootFailureGuard = ref(false);

/** 读取真实守护状态（浏览器端 supervisorStatus 直接返回 null）。 */
async function loadSupervisor(): Promise<void> {
  try {
    const status = await supervisorStatus();
    supervisor.value = status;
    if (status) {
      crashRestart.value = status.crash_restart;
      watchdog.value = status.watchdog;
      bootFailureGuard.value = status.boot_failure_guard;
    }
  } catch {
    supervisor.value = null;
  }
}

/** 把后端返回的真实状态写回本地开关。 */
function applySupervisor(s: SupervisorStatus): void {
  crashRestart.value = s.crash_restart;
  watchdog.value = s.watchdog;
  bootFailureGuard.value = s.boot_failure_guard;
  supervisor.value = s;
}

/**
 * 守护开关变更：真实 `setSupervisor`（桌面端）。
 *
 * 开关用 `:model-value` 单向绑定，仅成功返回后由 `applySupervisor` 回写，
 * 因此失败无需手动回滚；失败时把真实原因展示给用户，绝不伪造成功。
 */
async function onSupervisor(
  field: 'crash_restart' | 'watchdog' | 'boot_failure_guard',
  next: boolean,
): Promise<void> {
  if (svBusy.value || !shellAvailable) {
    return;
  }
  svBusy.value = true;
  try {
    const patch: { crash_restart?: boolean; watchdog?: boolean; boot_failure_guard?: boolean } = {};
    if (field === 'crash_restart') {
      patch.crash_restart = next;
    } else if (field === 'watchdog') {
      patch.watchdog = next;
    } else {
      patch.boot_failure_guard = next;
    }
    const result = await setSupervisor(patch);
    if (result.ok) {
      applySupervisor(result.state);
      note(result.message, 'ok');
    } else {
      note(`守护配置未变更：${result.message}`, 'warn');
    }
  } catch (cause) {
    const raw = cause instanceof Error ? cause.message : String(cause);
    note(`守护配置未变更：${raw}`, 'warn');
  } finally {
    svBusy.value = false;
  }
}

// ---------------------------------------------------------------------------
// 开机自启（真实：GET / PUT /api/service/autostart）
// ---------------------------------------------------------------------------

/** 自启真实状态（未取到 = null，页面按「未知」呈现，不猜）。 */
const autostartStatus = ref<AutostartStatus | null>(null);

/**
 * 部署形态：优先取自启端点的 `form`（后端由 `platform::detect()` 实时派生），
 * 兜底 `/api/overview` 的 `deployMode`；两者都不可得 → `'unknown'`（诚实降级）。
 */
const deployForm = computed<string>(() => {
  const fromApi = (autostartStatus.value?.form || '').trim();
  if (fromApi && fromApi !== 'unknown') {
    return fromApi;
  }
  const fromOverview = (gateway.value.deployMode || '').trim();
  return fromOverview && fromOverview !== '—' ? fromOverview : 'unknown';
});

/**
 * 是否容器部署形态：该形态下「开机自启」由宿主 docker.service + compose
 * restart 策略托管，网关在容器内**改写不了宿主**，故本页不提供自启开关。
 */
const isContainerForm = computed<boolean>(() => deployForm.value === 'docker');

/** 自启保障声明（容器形态给 restart 策略声明值 + 宿主前提说明）。 */
const autostartGuarantee = computed<{ value: string; provenance: string; hint: string }>(
  () => autostartStatus.value?.autostartGuaranteed ?? { value: '', provenance: 'unknown', hint: '' },
);

/** 自启保障提示文案：优先后端 hint，缺失时给诚实兜底（不编造策略值）。 */
const autostartGuaranteeHint = computed<string>(() => {
  const g = autostartGuarantee.value;
  if (g.hint) {
    return g.hint;
  }
  return '自启保障状态不可得：网关未上报容器重启策略声明值，请确认 compose 已注入 IOT_DAQ_RESTART_POLICY。';
});

/** 写入进行中（防重复下发）。 */
const autostartBusy = ref(false);

/** 后端是否具备写入能力（诚实：`writeSupported:false` → 开关禁用 + 展示原因）。 */
const autostartWritable = computed<boolean>(() => autostartStatus.value?.writeSupported === true);

/** 启动策略卡头能力标签：以真实 writeSupported 为准，false 时展示后端真实原因。 */
const autostartCapability = computed<{ text: string; cls: string }>(() => {
  const status = autostartStatus.value;
  if (!status) {
    return { text: '自启能力状态未知', cls: 'wc-tag--neutral' };
  }
  if (status.writeSupported) {
    return { text: '可写入自启配置', cls: 'wc-tag--ok' };
  }
  return { text: status.writeReason || '本端当前未开放自启写入能力', cls: 'wc-tag--warn' };
});

/** 自启状态短标签（未注册 / 已注册 / 未知；容器形态为「由编排托管」）。 */
const autostartStateText = computed<string>(() => {
  const status = autostartStatus.value;
  if (!status) {
    return isContainerForm.value ? '由编排托管' : '未知';
  }
  if (isContainerForm.value) {
    return '由编排托管';
  }
  if (status.registered === true) {
    return '已注册';
  }
  return status.registered === false ? '未注册' : '未知';
});

/** 自启说明：优先真实状态与命令，后端不支持写入时展示真实原因。 */
const autostartDesc = computed<string>(() => {
  const status = autostartStatus.value;
  // 容器形态：不给「开关语义」的描述，只说清由谁托管 + 声明的策略值（诚实）。
  if (isContainerForm.value) {
    const g = autostartGuarantee.value;
    const policy = g.value ? g.value : '未声明';
    const provenance =
      g.provenance === 'declared'
        ? '该值为 compose 注入的声明值，网关在容器内无法实测'
        : '该值不可得';
    return `自启由容器编排托管（restart 策略：${policy}，${provenance}）。本页不提供开关——如需变更请在宿主修改 compose 的 restart 字段。`;
  }
  if (!status) {
    return '对应 systemctl enable，宿主开机时由服务管理器自动拉起。';
  }
  const base =
    status.managedBy === 'systemd'
      ? '对应宿主 systemd unit，宿主开机时由服务管理器自动拉起。'
      : '对应系统自启注册表，宿主开机时自动拉起。';
  // 注册目标（容错读 target / targetKind，缺失不报错）
  const targetText = status.target
    ? ` 注册目标：${status.target}${status.targetKind ? `（${status.targetKind}）` : ''}`
    : '';
  const command = status.command.trim();
  const real =
    status.registered === null
      ? '当前平台未提供自启查询。'
      : `${base} 自启命令：${command || '—'}${targetText}`;
  if (!status.writeSupported) {
    return `（${real}）${status.writeReason || '本端当前未开放自启写入能力'}`;
  }
  return `（${real}）`;
});

/** 读取真实自启状态。 */
async function loadAutostart(): Promise<void> {
  try {
    const status = await repo.ops.autostartStatus();
    autostartStatus.value = status;
    if (status.registered === true || status.registered === false) {
      autostartEnabled.value = status.registered;
    }
  } catch (cause) {
    autostartStatus.value = null;
    const raw = cause instanceof Error ? cause.message : String(cause);
    note(`自启状态读取失败：${raw}`, 'warn');
  }
}

/**
 * 自启开关变更：真实 `PUT /api/service/autostart`。
 *
 * 失败时把开关**回滚到变更前的值**（界面不谎称成功），并展示后端真实原因。
 */
async function onAutostart(next: boolean): Promise<void> {
  if (autostartBusy.value) {
    return;
  }
  const previous = !next;
  autostartBusy.value = true;
  try {
    const result = await repo.ops.setAutostart({
      enable: next,
      reason: next ? '启用开机自启' : '关闭开机自启',
      note: '由「启动与自启」页开关下发，写入审计日志。',
    });
    if (result.ok) {
      note(result.message, 'ok');
      await loadAutostart();
    } else {
      autostartEnabled.value = previous;
      note(`自启未变更：${result.message}`, 'warn');
    }
  } catch (cause) {
    autostartEnabled.value = previous;
    const raw = cause instanceof Error ? cause.message : String(cause);
    note(`自启未变更：${raw}`, 'warn');
  } finally {
    autostartBusy.value = false;
  }
}

onMounted(() => {
  void loadAutostart();
  if (shellAvailable) {
    void loadSupervisor();
  }
});

/** 最近一次操作反馈。 */
const lastAction = ref('');
const lastKind = ref<'ok' | 'warn'>('ok');

/** 结果区提示。 */
function note(text: string, kind: 'ok' | 'warn' = 'ok'): void {
  lastAction.value = text;
  lastKind.value = kind;
}


// ---------------------------------------------------------------------------
// 危险操作：重启 / 停止（DangerConfirmModal 契约）
// ---------------------------------------------------------------------------

const RESTART_REASONS: readonly string[] = [
  '配置变更需要生效',
  '进程异常需恢复',
  '例行维护窗口',
  '其它（请在补充说明中描述）',
];

const STOP_REASONS: readonly string[] = [
  '现场停机 / 检修',
  '更换硬件或迁移部署',
  '配置整改需短暂停机',
  '其它（请在补充说明中描述）',
];

const restartModal = reactive<{ open: boolean; impacts: readonly string[]; facts: readonly { label: string; value: string }[] }>({
  open: false,
  impacts: [],
  facts: [],
});

/**
 * 停止按钮文案：容器形态下原文案「不自动启动」是**假陈述**——
 * `restart: unless-stopped` 会在进程退出后立刻拉起容器，故如实改写。
 */
const stopButtonText = computed<string>(() =>
  isContainerForm.value ? '停止服务（容器将被自动拉起）' : '停止服务（不自动启动）',
);

const stopModal = reactive<{ open: boolean; impacts: readonly string[]; facts: readonly { label: string; value: string }[] }>({
  open: false,
  impacts: [],
  facts: [],
});

/** 危险动作的公共对象摘要（真实字段）。 */
function actionFacts(): readonly { label: string; value: string }[] {
  return [
    { label: '网关标识', value: gateway.value.name || '—' },
    { label: '部署形态', value: gateway.value.deployMode || '—' },
    { label: '操作者', value: session.state.displayName || '—' },
  ];
}

function openRestart(): void {
  if (!gatewayId.value) {
    note('无法二次确认：未取到网关标识（gateway_id），请刷新页面后重试。', 'warn');
    return;
  }
  restartModal.impacts = [
    '采集暂停约 5–15 秒；已入队数据不丢，恢复后自动补发。',
    '北向转发短暂中断，客户 Broker 可能出现一个空档。',
    '若队列中有待补发数据，将优先 flush 后再退出（优雅停机）。',
    '操作不可撤销；原因与补充说明将写入审计日志。',
    `需输入网关标识 ${gatewayId.value || '（当前不可得）'} 完成二次校验。`,
  ];
  restartModal.facts = actionFacts();
  restartModal.open = true;
}

function openStop(): void {
  if (!gatewayId.value) {
    note('无法二次确认：未取到网关标识（gateway_id），请刷新页面后重试。', 'warn');
    return;
  }
  stopModal.impacts = [
    '北向转发立即中断，客户 Broker 不再收到数据。',
    '本地采集也会停止；停机为优雅停机（先 flush 队列再退出）。',
    '是否再次拉起由 Supervisor / 服务管理器决定（本端点不承诺自动重启）。',
    `操作不可撤销；需输入网关标识 ${gatewayId.value || '（当前不可得）'} 完成二次校验。`,
  ];
  stopModal.facts = actionFacts();
  stopModal.open = true;
}

/**
 * 确认重启：真实 `POST /api/ops/restart`。
 *
 * 冻结语义：body `confirm` 必须等于当前 gateway_id。弹窗在 `confirmMode="full"`
 * 下把用户输入**原文**透传给本页，本页经 `WriteMeta` 原样上送（不 trim / 不拼接），
 * 由服务端比对；失败（400 confirm_mismatch / 403）时弹窗保持打开并把真实原因
 * 写入对象摘要，不伪造成功。
 */
async function confirmRestart(payload: { reason: string; note: string; confirm: string }): Promise<void> {
  const result = await repo.ops.restart({
    actor: session.state.displayName,
    confirm: payload.confirm,
    reason: payload.reason,
    note: payload.note,
  });
  if (result.ok) {
    restartModal.open = false;
    note(`${result.message}（原因：${payload.reason} · ${payload.note}）`, 'ok');
  } else {
    restartModal.facts = [...actionFacts(), { label: '上次结果', value: result.message }];
    note(`重启未成功：${result.message}`, 'warn');
  }
}

/** 确认停止：真实 `POST /api/ops/stop`（契约与 restart 一致，confirm 同样透传原文）。 */
async function confirmStop(payload: { reason: string; note: string; confirm: string }): Promise<void> {
  const result = await repo.ops.stop({
    actor: session.state.displayName,
    confirm: payload.confirm,
    reason: payload.reason,
    note: payload.note,
  });
  if (result.ok) {
    stopModal.open = false;
    note(`${result.message}（原因：${payload.reason} · ${payload.note}）`, 'ok');
  } else {
    stopModal.facts = [...actionFacts(), { label: '上次结果', value: result.message }];
    note(`停止未成功：${result.message}`, 'warn');
  }
}
</script>

<style scoped>
/* 开关行 */
.su-row {
  display: flex;
  align-items: center;
  gap: 16px;
  flex-wrap: wrap;
  padding: 12px 0;
  border-bottom: 1px solid var(--divider);
}
.su-row:last-of-type {
  border-bottom: 0;
}
.su-row__text {
  flex: 1 1 auto;
  min-width: 220px;
}
.su-row__title {
  font-size: var(--fs-body);
  font-weight: 600;
  color: var(--text-1);
  display: flex;
  align-items: center;
  gap: 8px;
}
.su-row__desc {
  font-size: var(--fs-caption);
  color: var(--text-3);
  margin-top: 4px;
  line-height: 1.6;
}

/* 影响块 */
.su-impact {
  padding: 12px 14px;
  border: 1px solid var(--warn-border);
  background: var(--warn-bg);
  border-radius: var(--radius);
  margin-bottom: 12px;
}
.su-impact__title {
  margin: 0;
  font-size: var(--fs-table);
  font-weight: 600;
  color: var(--warn-fg);
}
.su-impact ul {
  margin: 6px 0 0;
  padding-left: 18px;
  font-size: var(--fs-caption);
  color: var(--text-2);
  line-height: 1.75;
}

/* 危险动作按钮行 + 结果区 */
.su-ops {
  display: flex;
  gap: 10px;
  flex-wrap: wrap;
}
.su-result {
  display: flex;
  align-items: flex-start;
  gap: 8px;
  margin-top: 12px;
  padding: 10px 12px;
  border-radius: var(--radius-sm);
  font-size: var(--fs-caption);
  line-height: 1.6;
  border: 1px solid var(--ok-border);
  background: var(--ok-bg);
  color: var(--ok-fg);
}
.su-result.is-warn {
  border-color: var(--warn-border);
  background: var(--warn-bg);
  color: var(--warn-fg);
}

/* 守护状态 / 浏览器降级提示 */
.su-sv,
.su-hint {
  margin: 8px 0 0;
  font-size: var(--fs-caption);
  line-height: 1.6;
}
.su-sv {
  color: var(--text-2);
}
.su-hint {
  color: var(--text-3);
}
</style>
