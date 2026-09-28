<!--
  =============================================================================
  UpdatePage —— 系统更新（设计 §3.7 / 原型 gateway-v2a-glacier「update」）
  =============================================================================
  真实能力边界（不许造数）：
    · 后端提供「当前版本」（`GET /api/overview`）、「更新检查」（`GET /api/updates/check`）
      与「执行更新」（`POST /api/updates/apply`）；
    · 「开始更新」经 DangerConfirmModal 四要素确认（影响清单 + 原因必填 + 网关标识二次校验）
      后真实下发 `{reason, note, confirm}` 三要素；后端返回 `accepted:true, applied:false`
      时**如实**说明「新包已下载并通过验签，重启后由宿主安装器完成替换」——绝不写「更新成功」；
      后端 `supported:false` / `accepted:false` 时原文展示其 `reason`，不自行编造文案；
    · 版本回滚 / 离线包导入的能力尚未接入，相关动作诚实留空（`—`）并按真实结果反馈；
    · 通道与策略开关为**本机界面状态**（暂不保存到网关），显式标注，不伪装成已落库；
    · 「执行更新」按钮按后端声明的真实能力禁用——**不允许点了才报错，也不允许假装可点**。
-->
<template>
  <div class="wc-content">
    <!-- 工具条：检查更新 / 导入离线包（原页头右侧按钮迁入） -->
    <div class="pg-toolbar">
      <span class="wc-tag" :class="updateBannerClass" data-testid="update-banner">{{ updateBanner }}</span>
      <span class="wc-spacer" />
      <RoleGate :allowed="canEdit" mode="disable" deny-text="当前角色无权执行系统更新">
        <button type="button" class="wc-btn wc-btn--sm" :disabled="checking" data-testid="update-check" @click="checkUpdate">
          {{ checking ? '检查中…' : '检查更新' }}
        </button>
      </RoleGate>
      <RoleGate :allowed="canEdit" mode="disable" deny-text="当前角色无权导入离线包">
        <button type="button" class="wc-btn wc-btn--sm" data-testid="update-offline" @click="openOffline">导入离线包</button>
      </RoleGate>
    </div>

    <p v-if="actionMessage" class="wc-hint" data-testid="update-message">{{ actionMessage }}</p>

    <!-- ══ KPI ═════════════════════════════════════════════════════════ -->
    <div class="wc-grid wc-grid--4">
      <div class="wc-kpi">
        <span class="wc-kpi__label">当前版本</span>
        <span class="wc-kpi__value" data-testid="kpi-current-version">{{ currentVersion }}</span>
        <span class="wc-kpi__sub">网关上报</span>
      </div>
      <div class="wc-kpi">
        <span class="wc-kpi__label">最新可用</span>
        <span class="wc-kpi__value" data-testid="kpi-latest-version">{{ latestVersion || '—' }}</span>
        <span class="wc-kpi__sub">{{ latestVersionSub }}</span>
      </div>
      <div class="wc-kpi">
        <span class="wc-kpi__label">更新包签名</span>
        <span class="wc-kpi__value">{{ updateAvailable ? '已通过验签' : '—' }}</span>
        <span class="wc-kpi__sub">{{ updateAvailable ? 'Ed25519 + SHA-256' : '无待安装更新包' }}</span>
      </div>
      <div class="wc-kpi">
        <span class="wc-kpi__label">更新通道</span>
        <span class="wc-kpi__value">{{ channelLabel }}</span>
        <span class="wc-kpi__sub">{{ channel }}</span>
      </div>
    </div>

    <!-- ══ 通道策略 + 更新进度 ═════════════════════════════════════════ -->
    <div class="wc-grid wc-grid--2-1">
      <section class="wc-card">
        <div class="wc-card__head">
          <h3>更新通道与策略</h3>
          <span class="wc-tag wc-tag--info">本机界面状态 · 暂不保存到网关</span>
        </div>
        <div class="wc-card__body">
          <UiRadio v-model="channel" :options="channelOptions" :disabled="!canEdit" />

          <div class="wc-list">
            <div v-for="toggle in toggles" :key="toggle.label" class="up-toggle">
              <div class="up-toggle__main">
                <span class="up-toggle__label">{{ toggle.label }}</span>
                <span class="up-toggle__desc">{{ toggle.desc }}</span>
              </div>
              <UiSwitch v-model="toggle.on" :disabled="!canEdit" />
            </div>
          </div>
        </div>
      </section>

      <section class="wc-card">
        <div class="wc-card__head">
          <h3>更新进度</h3>
          <span class="wc-card__sub">{{ progressSub }}</span>
        </div>
        <div class="wc-card__body">
          <ol class="up-steps">
            <li v-for="step in steps" :key="step.n" class="up-step up-step--todo">
              <span class="up-step__n">{{ step.n }}</span>
              <div class="up-step__body">
                <span class="up-step__label">{{ step.label }}</span>
                <span class="up-step__desc">{{ step.desc }}</span>
              </div>
            </li>
          </ol>
          <div style="margin-top: 12px">
            <div class="up-prog-head">
              <span class="wc-card__sub">总进度</span>
              <span class="wc-mono" data-testid="update-progress-text">—</span>
            </div>
            <div class="wc-bar">
              <div class="wc-bar__fill" :style="{ width: '0%' }" data-testid="update-progress-bar" />
            </div>
          </div>
          <div style="display: flex; gap: 8px; margin-top: 14px; flex-wrap: wrap">
            <button
              type="button"
              class="wc-btn wc-btn--primary"
              :disabled="!applyEnabled"
              :title="applyEnabled ? '' : applyDisabledReason"
              data-testid="update-start"
              @click="startUpdate"
            >
              开始更新
            </button>
            <button
              type="button"
              class="wc-btn"
              :disabled="!canEdit"
              data-testid="update-rollback"
              @click="openRollback(currentVersion)"
            >
              回滚到 {{ currentVersion }}
            </button>
          </div>
          <p v-if="!applyEnabled && applyDisabledReason" class="wc-hint" data-testid="update-apply-reason">
            {{ applyDisabledReason }}
          </p>
        </div>
      </section>
    </div>

    <!-- ══ 更新历史 + 离线更新要求 ══════════════════════════════════════ -->
    <div class="wc-grid wc-grid--2">
      <section class="wc-card">
        <div class="wc-card__head">
          <h3>更新历史</h3>
        </div>
        <EmptyState
          title="暂无更新记录"
          desc="网关暂未返回更新历史；执行一次更新后，这里会显示每次更新的结果。"
        >
          <template #actions>
            <button type="button" class="wc-btn wc-btn--sm" data-testid="update-check-empty" @click="checkUpdate">检查更新</button>
          </template>
        </EmptyState>
      </section>

      <section class="wc-card">
        <div class="wc-card__head">
          <h3>离线更新要求</h3>
        </div>
        <div class="wc-card__body">
          <ul class="up-impact">
            <li>工业现场常<b>无外网</b>：可导入离线更新包（.tar.zst + 签名清单），签名校验与在线一致。</li>
            <li>更新会重启服务，采集中断约 5–15 秒；已入队数据不丢失。</li>
            <li v-if="updateSupported">已配置更新源：可在线更新；现场无外网时也可改用离线包，签名校验一致。</li>
            <li v-else>在线更新当前不可用：{{ lastCheck?.reason || '请先点击「检查更新」获取后端声明的真实原因。' }}</li>
          </ul>
          <button type="button" class="wc-btn" :disabled="!canEdit" data-testid="update-offline-2" @click="openOffline">
            导入离线更新包
          </button>
        </div>
      </section>
    </div>
  </div>

  <!-- 执行更新（危险：下载 + 验签，重启后由宿主安装器替换运行版本） -->
  <DangerConfirmModal
    :open="applyOpen"
    :title="`执行系统更新到 ${applyTargetVersion || '最新版本'}`"
    :impacts="applyImpacts"
    :facts="applyFacts"
    :reasons="APPLY_REASONS"
    :min-note-length="10"
    confirm-mode="full"
    :confirm-value="gatewayId"
    confirm-label="风险二次确认（输入网关标识全名）"
    :confirm-placeholder="`输入网关标识 ${gatewayId || '（当前不可得）'} 以确认`"
    confirm-text="确认执行更新"
    @close="applyOpen = false"
    @submit="onApplySubmit"
  />

  <!-- 回滚危险二次确认 -->
  <DangerConfirmModal
    :open="rollbackOpen"
    :title="`回滚系统到 ${rollbackTarget}`"
    :impacts="rollbackImpacts"
    :facts="rollbackFacts"
    :reasons="ROLLBACK_REASONS"
    :min-note-length="10"
    :confirm-value="rollbackTarget"
    confirm-label="二次校验（输入目标版本号）"
    :confirm-text="`回滚到 ${rollbackTarget}`"
    @close="rollbackOpen = false"
    @submit="onRollbackSubmit"
  />

  <!-- 离线包导入（危险：上传固件） -->
  <DangerConfirmModal
    :open="offlineOpen"
    title="导入离线更新包"
    :impacts="offlineImpacts"
    :facts="offlineFacts"
    :reasons="OFFLINE_REASONS"
    :min-note-length="10"
    confirm-value="iot-daq-offline.tar.zst"
    confirm-label="二次校验（输入离线包文件名）"
    confirm-text="确认导入离线包"
    @close="offlineOpen = false"
    @submit="onOfflineSubmit"
  />
</template>

<script setup lang="ts">
/**
 * @file UpdatePage.vue
 * @module web-console/pages/UpdatePage
 * @description 系统更新页（当前版本取自网关；检查状态取自后端，能力未接入时诚实降级）。
 */
import { computed, onMounted, reactive, ref, watch } from 'vue';
import {
  UiRadio,
  UiSwitch,
  EmptyState,
  RoleGate,
  DangerConfirmModal,
  type RadioOption,
  type DangerFact,
} from '@ui-kit';
import { dataVersion, repo, type UpdateCheckInfo } from '@/api/repo';
import { session } from '../store/session';

/** 当前角色是否可执行更新（更新属高危，仅 admin）。 */
const canEdit = computed<boolean>(() => session.state.role === 'admin');

/** 当前版本（真实：`GET /api/overview` 的 version）。 */
const currentVersion = ref(repo.getGateway().version || '—');
watch(dataVersion, () => {
  currentVersion.value = repo.getGateway().version || '—';
});

/**
 * 网关标识（`GET /api/overview` 的 name 即当前 gateway_id）：执行更新的
 * 二次校验锚点，只透传真实值供用户对照输入，**禁止硬编码**。
 */
const gatewayName = ref(repo.getGateway().name || '');
watch(dataVersion, () => {
  gatewayName.value = repo.getGateway().name || '';
});

/** 规范化后的网关标识（空 / 占位符 → 空串，不可用于二次校验）。 */
const gatewayId = computed<string>(() => {
  const id = gatewayName.value.trim();
  return id && id !== '—' ? id : '';
});

/** 操作提示（结果区，真实结果原文）。 */
const actionMessage = ref('');

// ---------- 更新检查（真实：GET /api/updates/check） ----------
/** 是否正在检查（避免重复下发）。 */
const checking = ref(false);

/** 最近一次检查结果（未检查 = null）。 */
const lastCheck = ref<UpdateCheckInfo | null>(null);

/** 尚未检查时展示的中性说明（不假设能力可用）。 */
const NOT_CHECKED_HINT = '尚未检查更新，暂不能执行更新；请先点击「检查更新」。';

/** 后端是否声明支持更新检查（未配置 OTA / 能力未接线 → false）。 */
const updateSupported = computed<boolean>(() => lastCheck.value?.checkSupported === true);

/** 后端是否存在**待安装**的新版本（= 已有落盘 pending；该包已下载并通过验签）。 */
const updateAvailable = computed<boolean>(() => lastCheck.value?.updateAvailable === true);

/**
 * 「执行更新」是否可点：后端声明具备能力（`checkSupported`）+ 已有待安装的新版本
 * （`updateAvailable` = 存在落盘 pending）+ 当前角色有权限。
 *
 * 语义边界：`updateAvailable` 只表示「存在待重启生效的新版本」，是否最终被受理由
 * **后端在执行端点里判定**并给出真实原因，本页不自造判定。
 */
const applyEnabled = computed<boolean>(
  () => canEdit.value && updateSupported.value && updateAvailable.value,
);

/** 无法执行更新时的**真实原因**（优先取后端 `reason`；绝不假装可点）。 */
const applyDisabledReason = computed<string>(() => {
  if (!canEdit.value) {
    return '当前角色无权执行系统更新。';
  }
  if (!lastCheck.value) {
    return NOT_CHECKED_HINT;
  }
  if (!lastCheck.value.checkSupported) {
    return lastCheck.value.reason || '暂时无法执行更新。';
  }
  if (!lastCheck.value.updateAvailable) {
    return lastCheck.value.reason || '当前没有待安装的新版本。';
  }
  return '';
});

/**
 * 最新可用版本（未检查 / 无待安装版本 → 空串，页面显示 —）。
 *
 * ⚠️ 口径红线：`availableVersion` 是 OTA 的待安装版本号，与 `currentVersion`
 * （Cargo 包版本串）**不是同一口径**，因此无待安装版本时**不**回落展示包版本。
 */
const latestVersion = computed<string>(() => {
  const info = lastCheck.value;
  if (!info || !info.checkSupported || !info.updateAvailable) {
    return '';
  }
  return info.availableVersion || '';
});

/** 最新可用副文案（诚实：未检查 / 无待安装 / 有待安装各一句，不可判定时用后端原文）。 */
const latestVersionSub = computed<string>(() => {
  const info = lastCheck.value;
  if (!info) {
    return '尚未检查更新';
  }
  if (!info.checkSupported) {
    return info.reason || '暂时无法检查更新。';
  }
  return info.updateAvailable
    ? `来自 ${info.source || '配置的更新源'}`
    : info.reason || '当前没有待安装的新版本。';
});

/** 顶部条：真实状态（能力未接入时直接展示后端给出的原因）。 */
const updateBanner = computed<string>(() => {
  const info = lastCheck.value;
  if (checking.value) {
    return '正在检查更新…';
  }
  if (!info) {
    return '尚未检查更新';
  }
  if (!info.checkSupported) {
    return info.reason || '暂时无法检查更新。';
  }
  return info.updateAvailable
    ? `待安装新版本 ${info.availableVersion || ''}（重启后生效）`
    : '没有待安装的新版本';
});

/** 顶部条色调（有更新 = 提示色，无更新 = 正常色，未检查 = 中性）。 */
const updateBannerClass = computed<string>(() => {
  const info = lastCheck.value;
  if (!info || !info.checkSupported) {
    return 'wc-tag--neutral';
  }
  return info.updateAvailable ? 'wc-tag--info' : 'wc-tag--ok';
});

// ---------- 通道策略（本机界面状态） ----------
const channel = ref<'stable' | 'beta' | 'lock'>('stable');

/** 通道选项。 */
const channelOptions: readonly RadioOption[] = [
  { value: 'stable', label: '稳定版 stable（推荐）', desc: '仅接收经过完整回归的版本。工业现场默认。' },
  { value: 'beta', label: '测试版 beta', desc: '提前获取修复，可能引入回归。仅用于试产线。' },
  { value: 'lock', label: '锁定 · 不接收更新', desc: '完全关闭更新检查。适用于已验收冻结的生产线。' },
];

/** 通道中文名。 */
const channelLabel = computed<string>(() => {
  const map: Record<string, string> = { stable: '稳定版', beta: '测试版', lock: '已锁定' };
  return map[channel.value] ?? channel.value;
});

/** 策略开关（本机界面状态）。 */
const toggles = reactive([
  { label: '自动检查更新', on: true, desc: '每日检查一次，仅提示，不自动安装。' },
  { label: '自动安装', on: false, desc: '关闭时需人工点击安装；工业现场建议保持关闭。' },
  { label: '维护窗口内自动安装', on: true, desc: '仅在维护窗口内允许自动安装，避免占用产线时段。' },
  { label: '安装前自动备份配置', on: true, desc: '升级前自动导出配置快照，失败可回滚。' },
]);

// ---------- 进度（能力未接入 → 不做假进度） ----------

/** 更新进度区副文案（如实反映是否有待安装版本，不做假进度）。 */
const progressSub = computed<string>(() => {
  if (updateAvailable.value) {
    return '新版本已就绪 · 待重启生效';
  }
  return updateSupported.value ? '没有待安装的新版本' : '暂无进行中的更新';
});

/** 更新步骤（流程说明，不含具体包体数据）。 */
const steps = [
  { n: '1', label: '下载更新包', desc: '来自官方源或内网镜像' },
  { n: '2', label: '校验签名与完整性', desc: 'Ed25519 + SHA-256 清单' },
  { n: '3', label: '备份当前版本与配置', desc: '写入 rollback 目录' },
  { n: '4', label: '应用并重启服务', desc: '服务平滑重启，采集中断数秒后自动恢复' },
  { n: '5', label: '健康自检', desc: '采集 / 转发 / 授权 三项连通性' },
];

// ---------- 执行更新（危险：POST /api/updates/apply） ----------
/** 危险二次确认弹窗开关。 */
const applyOpen = ref(false);
/** 是否正在下发（防重复提交）。 */
const applying = ref(false);

/** 本次更新目标版本（来自最近一次检查的待安装版本；无待安装版本时为空）。 */
const applyTargetVersion = computed<string>(() =>
  lastCheck.value?.updateAvailable ? lastCheck.value.availableVersion || '' : '',
);

/** 执行更新影响清单（如实写清后果与恢复路径）。 */
const applyImpacts: readonly string[] = [
  '更新会替换当前运行版本并重启服务，采集中断约 5–15 秒；已入队数据不丢失。',
  '更新包必须通过签名与完整性校验（Ed25519 + SHA-256），校验失败即拒绝安装。',
  '后端当前只负责下载与验签：替换由宿主安装器在重启时完成，本页不承诺「已升级」，结果以重启后的实际版本号为准。',
  '操作不可撤销；原因与补充说明将写入审计日志。',
];

/** 执行更新对象摘要（真实字段）。 */
const applyFacts = computed<readonly DangerFact[]>(() => [
  { label: '当前版本', value: currentVersion.value },
  { label: '目标版本', value: applyTargetVersion.value || '—' },
  { label: '升级源', value: lastCheck.value?.source || '—' },
  { label: '网关标识', value: gatewayId.value || '—' },
]);

/** 执行更新原因枚举（必选，将随操作写入审计）。 */
const APPLY_REASONS: readonly string[] = [
  '安全漏洞修复',
  '功能升级',
  '缺陷修复',
  '现场验收要求升级',
  '其它（请在补充说明中描述）',
];

/**
 * 「开始更新」：能力就绪时打开**危险二次确认**弹窗（不直接下发指令）。
 *
 * 兜底说明：按钮不可点时给出后端声明的真实原因；取不到网关标识时拒绝打开
 * 弹窗（二次校验锚点缺失 → fail-closed），绝不伪造成功、绝不下发假指令。
 */
function startUpdate(): void {
  if (!applyEnabled.value) {
    actionMessage.value = applyDisabledReason.value;
    return;
  }
  if (!gatewayId.value) {
    actionMessage.value = '无法二次确认：未取到网关标识，请刷新页面后重试。';
    return;
  }
  applyOpen.value = true;
}

/**
 * 确认执行更新：真实 `POST /api/updates/apply`。
 *
 * body 恰好是 `{reason, note, confirm}` 三个**彼此独立**的字段
 * （`note` 绝不并入 `reason`）；后端 `accepted:true, applied:false` 时由 repo
 * 给出「已下载并验签、重启后由宿主安装器替换」的**如实**文案，绝不写「更新成功」；
 * `supported:false` / `accepted:false` 时把后端 `reason` 原文呈现给用户。
 */
async function onApplySubmit(payload: { reason: string; note: string; confirm: string }): Promise<void> {
  if (applying.value) {
    return;
  }
  applying.value = true;
  try {
    const outcome = await repo.ops.applyUpdate({
      reason: payload.reason,
      note: payload.note,
      confirm: payload.confirm,
    });
    applyOpen.value = false;
    actionMessage.value = outcome.message;
  } finally {
    applying.value = false;
  }
}

/**
 * 检查更新：真实 `GET /api/updates/check`。
 *
 * 后端未配置更新源时返回 `checkSupported:false` + `reason`，页面照实展示
 * （诚实降级，绝不伪造「已是最新」）。失败时展示真实 HTTP 原因。
 */
async function checkUpdate(): Promise<void> {
  if (checking.value) {
    return;
  }
  checking.value = true;
  actionMessage.value = '';
  try {
    const info = await repo.ops.checkUpdates();
    lastCheck.value = info;
    if (!info.checkSupported) {
      actionMessage.value = `未执行检查：${info.reason || '暂时无法检查更新。'}`;
    } else if (info.updateAvailable) {
      // 口径红线：`availableVersion`（OTA 待安装版本号）与 `currentVersion`（Cargo
      // 包版本串）不是一个口径，**不**并排展示；重启替换的口径以后端为准。
      actionMessage.value =
        `发现待安装的新版本 ${info.availableVersion || '（版本号未上报）'}（升级源 ${info.source || '—'}）；` +
        '重启网关后由宿主安装器完成替换。';
    } else {
      // 无待安装版本 ≠ 「已是最新」——直接展示后端原文，避免过度承诺。
      actionMessage.value = info.reason || '当前没有待安装的新版本。';
    }
  } catch (cause) {
    lastCheck.value = null;
    const raw = cause instanceof Error ? cause.message : String(cause);
    actionMessage.value = `检查未成功：${raw}`;
  } finally {
    checking.value = false;
  }
}

// ---------- 回滚危险确认 ----------
const rollbackOpen = ref(false);
const rollbackTarget = ref('');

/** 回滚影响清单。 */
const rollbackImpacts: readonly string[] = [
  '回滚会替换当前运行版本，服务将重启，采集中断约 5–15 秒。',
  '回滚不会回退配置与点位表；如需配置回退请到「备份与恢复」。',
  '回滚保留的上一版本仅 1 份，回滚后该版本被当前版本覆盖。',
];

/** 回滚对象摘要。 */
const rollbackFacts = computed<readonly DangerFact[]>(() => [
  { label: '当前版本', value: currentVersion.value },
  { label: '目标版本', value: rollbackTarget.value || '—' },
  { label: '签名校验', value: '强制（失败即拒绝）' },
]);

/** 回滚原因枚举（必选）。 */
const ROLLBACK_REASONS: readonly string[] = ['新版本引入回归', '兼容性问题', '现场验收需回退', '误升级修复'];

/** 打开回滚确认。 */
function openRollback(version: string): void {
  if (!canEdit.value) {
    return;
  }
  rollbackTarget.value = version;
  rollbackOpen.value = true;
}

/** 回滚提交：版本回滚能力尚未接入，如实告知（不伪造成功）。 */
function onRollbackSubmit(payload: { reason: string; note: string; tail: string; secondApprover: string }): void {
  rollbackOpen.value = false;
  actionMessage.value =
    `回滚未执行：当前版本暂不支持版本回滚（原因：${payload.reason}）。` +
    '如需回退配置，请到「备份与恢复」。';
}

// ---------- 离线包导入（危险） ----------
const offlineOpen = ref(false);

/** 离线包导入影响清单。 */
const offlineImpacts: readonly string[] = [
  '上传并安装离线更新包会替换当前运行版本，服务将重启，采集中断约 5–15 秒。',
  '离线包必须带签名清单（.tar.zst + 签名），校验逻辑与在线一致，校验失败即拒绝安装。',
  '安装前会自动备份当前版本与配置，失败可回滚。',
];

/** 离线包对象摘要。 */
const offlineFacts: readonly DangerFact[] = [
  { label: '离线包文件名', value: 'iot-daq-offline.tar.zst' },
  { label: '签名清单', value: 'iot-daq-offline.tar.zst.sig' },
];

/** 离线包导入原因枚举（必选）。 */
const OFFLINE_REASONS: readonly string[] = ['现场无外网', '内网安全策略', '指定版本部署', '灾备恢复'];

/** 打开离线包导入确认。 */
function openOffline(): void {
  if (!canEdit.value) {
    return;
  }
  offlineOpen.value = true;
}

/** 离线包提交：离线包导入能力尚未接入，如实告知（不伪造成功）。 */
function onOfflineSubmit(payload: { reason: string; note: string; tail: string; secondApprover: string }): void {
  offlineOpen.value = false;
  actionMessage.value = `导入未执行：当前版本暂不支持离线包导入（原因：${payload.reason}）。`;
}

// ---------- 首次进入：拉取真实能力状态 ----------
// 让顶部状态条与「执行更新」按钮的可用性在进入页面时即为**权威**（不依赖用户先点一次），
// 未配置升级源 / 能力未接线时直接展示后端 `reason`。
onMounted(() => {
  void checkUpdate();
});
</script>

<style scoped>
.pg-toolbar {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: 8px;
  min-height: 28px;
}
.up-toggle {
  display: flex;
  align-items: flex-start;
  gap: 12px;
  padding: 11px 0;
  border-bottom: 1px solid var(--divider);
}
.up-toggle:last-child {
  border-bottom: 0;
}
.up-toggle__main {
  display: flex;
  flex-direction: column;
  flex: 1;
  min-width: 0;
}
.up-toggle__label {
  font-size: var(--fs-table);
  font-weight: 500;
  color: var(--text-1);
}
.up-toggle__desc {
  font-size: var(--fs-caption);
  color: var(--text-3);
  line-height: 1.5;
}
.up-steps {
  margin: 0;
  padding: 0;
  list-style: none;
  display: flex;
  flex-direction: column;
  gap: 10px;
}
.up-step {
  display: flex;
  align-items: flex-start;
  gap: 10px;
}
.up-step__n {
  width: 20px;
  height: 20px;
  flex: 0 0 20px;
  border-radius: 50%;
  display: flex;
  align-items: center;
  justify-content: center;
  font-size: 11px;
  font-weight: 600;
  background: var(--divider);
  color: var(--text-3);
}
.up-step--done .up-step__n {
  background: var(--ok-bg);
  color: var(--ok-fg);
}
.up-step--run .up-step__n {
  background: var(--info-bg);
  color: var(--info-fg);
}
.up-step__body {
  display: flex;
  flex-direction: column;
}
.up-step__label {
  font-size: var(--fs-table);
  color: var(--text-1);
}
.up-step__desc {
  font-size: var(--fs-caption);
  color: var(--text-3);
}
.up-prog-head {
  display: flex;
  justify-content: space-between;
  margin-bottom: 6px;
}
.up-impact {
  margin: 0;
  padding-left: 18px;
  font-size: var(--fs-table);
  line-height: 1.8;
  color: var(--text-2);
}
</style>
