<!--
  =============================================================================
  UpdatePage —— 系统更新（设计 §3.7 / 原型 gateway-v2a-glacier「update」）
  =============================================================================
  更新通道 / 包校验 / 备份 / 回滚 / 离线包上传 / 维护窗口；更新历史列表分页。
  · 免费基础版无远程更新能力 —— 显示升级引导而非报错，**不提供任何绕过入口**。
  · 回滚为危险操作（DangerConfirmModal：影响清单 + 原因必填 + 对象二次校验）。
  · 离线包上传：.tar.zst + 签名清单，校验逻辑与在线一致；上传固件走危险确认。
-->
<template>
  <PageHeader
    crumb="运维 / 系统更新"
    title="系统更新"
    desc="远程更新：检查、下载、校验、应用与回滚。离线现场支持导入离线更新包（.tar.zst + 签名清单）。"
  >
    <template #actions>
      <span style="display: inline-flex; gap: 8px">
        <RoleGate :allowed="canEdit" mode="disable" deny-text="当前角色无权执行系统更新">
          <button type="button" class="wc-btn" data-testid="update-check" @click="checkUpdate">检查更新</button>
        </RoleGate>
        <RoleGate :allowed="canEdit" mode="disable" deny-text="当前角色无权导入离线包">
          <button type="button" class="wc-btn" data-testid="update-offline" @click="openOffline">导入离线包</button>
        </RoleGate>
      </span>
    </template>
  </PageHeader>

  <div class="wc-content">
    <!-- ══ 更新可用横幅 ════════════════════════════════════════════════ -->
    <div v-if="hasUpdate" class="wc-banner wc-banner--info" data-testid="update-banner">
      <span>有可用更新 {{ latestVersion }}</span>
      <span class="wc-banner__ops">
        <button type="button" class="wc-btn wc-btn--primary wc-btn--sm" :disabled="!canEdit" @click="startUpdate">
          立即更新
        </button>
      </span>
    </div>
    <div v-else class="wc-banner wc-banner--ok" data-testid="update-banner">
      <span>当前已是最新版本（{{ currentVersion }}），无需更新。</span>
    </div>

    <p v-if="actionMessage" class="wc-hint" data-testid="update-message">{{ actionMessage }}</p>

    <!-- ══ KPI ═════════════════════════════════════════════════════════ -->
    <div class="wc-grid wc-grid--4">
      <div class="wc-kpi">
        <span class="wc-kpi__label">当前版本</span>
        <span class="wc-kpi__value">{{ currentVersion }}</span>
        <span class="wc-kpi__sub">构建 a91f3c7</span>
      </div>
      <div class="wc-kpi">
        <span class="wc-kpi__label">最新可用</span>
        <span class="wc-kpi__value">{{ latestVersion }}</span>
        <span class="wc-kpi__sub wc-kpi__sub--ok">{{ hasUpdate ? '有更新' : '已最新' }}</span>
      </div>
      <div class="wc-kpi">
        <span class="wc-kpi__label">更新包签名</span>
        <span class="wc-kpi__value">已通过</span>
        <span class="wc-kpi__sub">Ed25519 + 代码签名</span>
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

          <dl class="wc-kv">
            <dt>维护窗口</dt>
            <dd class="wc-mono">{{ maintenanceWindow }}</dd>
            <dt>更新包来源</dt>
            <dd class="wc-mono">官方源 + 内网镜像</dd>
            <dt>签名校验</dt>
            <dd><StatusTag status="success" text="强制" /> 失败即拒绝安装</dd>
            <dt>回滚保留</dt>
            <dd class="wc-mono">保留上一版本（1 份）</dd>
          </dl>
        </div>
      </section>

      <section class="wc-card">
        <div class="wc-card__head">
          <h3>更新进度</h3>
        </div>
        <div class="wc-card__body">
          <ol class="up-steps">
            <li v-for="(step, i) in steps" :key="step.n" class="up-step" :class="`up-step--${stepState(i)}`">
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
              <span class="wc-mono" data-testid="update-progress-text">{{ progress }}%</span>
            </div>
            <div class="wc-bar">
              <div class="wc-bar__fill" :style="{ width: `${progress}%` }" data-testid="update-progress-bar" />
            </div>
          </div>
          <div style="display: flex; gap: 8px; margin-top: 14px; flex-wrap: wrap">
            <button
              type="button"
              class="wc-btn wc-btn--primary"
              :disabled="!canEdit"
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
        </div>
      </section>
    </div>

    <!-- ══ 更新历史（分页）+ 离线现场说明 ══════════════════════════════ -->
    <div class="wc-grid wc-grid--2">
      <section class="wc-card">
        <div class="wc-card__head">
          <h3>更新历史</h3>
          <span class="wc-card__sub">共 {{ historyTotal }} 条</span>
        </div>

        <EmptyState
          v-if="historyTotal === 0"
          title="还没有更新记录"
          desc="本机尚未执行过远程更新。首次更新前的自动备份会出现在「备份与恢复」。"
        >
          <template #actions>
            <button type="button" class="wc-btn wc-btn--primary" @click="startUpdate">开始更新</button>
          </template>
        </EmptyState>

        <template v-else>
          <UiTable :columns="historyColumns" :rows="pagedHistory" row-key-field="version">
            <template #cell-version="{ row }">
              <span class="wc-mono">{{ row.version }}</span>
            </template>
            <template #cell-time="{ row }">
              <span class="wc-mono">{{ row.time }}</span>
            </template>
            <template #cell-result="{ row }">
              <StatusTag :status="row.result" :text="resultLabel(row)" />
            </template>
            <template #cell-duration="{ row }">
              <span class="wc-mono">{{ row.duration }}</span>
            </template>
            <template #actions="{ row }">
              <button
                v-if="row.result === 'success'"
                type="button"
                class="wc-btn wc-btn--sm"
                :disabled="!canEdit"
                :data-testid="`history-rollback-${row.version}`"
                @click="openRollback(row.version)"
              >
                回滚
              </button>
              <button v-else type="button" class="wc-btn wc-btn--sm" @click="showDetail(row)">详情</button>
            </template>
          </UiTable>

          <UiPager :page="historyPage" :total="historyTotal" :page-size="HISTORY_PAGE_SIZE" @update:page="onHistoryPage" />
        </template>
      </section>

      <section class="wc-card">
        <div class="wc-card__head">
          <h3>离线现场与降级说明</h3>
        </div>
        <div class="wc-card__body">
          <div class="wc-banner wc-banner--warn">
            <span class="wc-banner__icon">!</span>
            <span>远程更新必须考虑的三件事</span>
          </div>
          <ul class="up-impact">
            <li>工业现场常<b>无外网</b>：必须支持导入离线更新包（.tar.zst + 签名清单），且校验逻辑与在线一致。</li>
            <li>更新会重启服务 → 必须<b>优雅停机</b>：队列 flush 完成后再退出，否则正在补发的数据会丢。</li>
            <li>免费基础版<b>无远程更新能力</b>，此处显示升级引导而非报错。</li>
          </ul>
          <p class="wc-note">
            <span class="wc-note__icon">i</span>
            <span>
              若当前为免费基础版，本页顶部横幅替换为「当前版本不含远程更新」并给出升级路径，
              <b>不提供任何绕过入口</b>。
            </span>
          </p>
          <button type="button" class="wc-btn" :disabled="!canEdit" data-testid="update-offline-2" @click="openOffline">
            导入离线更新包
          </button>
        </div>
      </section>
    </div>
  </div>

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

  <!-- 离线包上传（危险：上传固件） -->
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
 * @description 系统更新页（通道策略 + 进度 + 历史分页 + 回滚/离线包危险确认）。
 */
import { computed, reactive, ref } from 'vue';
import {
  PageHeader,
  UiTable,
  UiPager,
  UiRadio,
  UiSwitch,
  StatusTag,
  EmptyState,
  RoleGate,
  DangerConfirmModal,
  type TableColumn,
  type RadioOption,
  type DangerFact,
} from '@ui-kit';
import { session } from '../store/session';

/** 当前角色是否可执行更新（更新属高危，仅 admin）。 */
const canEdit = computed<boolean>(() => session.state.role === 'admin');

/** 每页条数（更新历史）。 */
const HISTORY_PAGE_SIZE = 5;

/** 当前版本 / 最新版本。 */
const currentVersion = 'v1.4.2';
const latestVersion = 'v1.5.0';

/** 是否有可用更新。 */
const hasUpdate = ref(true);

/** 操作提示。 */
const actionMessage = ref('');

// ---------- 通道策略 ----------
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

/** 策略开关（可切换）。 */
const toggles = reactive([
  { label: '自动检查更新', on: true, desc: '每日 08:00 检查一次，仅在界面提示，不自动安装。' },
  { label: '自动安装', on: false, desc: '关闭时需人工点击安装；工业现场建议保持关闭。' },
  { label: '维护窗口内自动安装', on: true, desc: '仅在下列窗口内允许自动安装，避免占用产线时段。' },
  { label: '安装前自动备份配置', on: true, desc: '升级前自动导出配置快照，失败可回滚。' },
]);

/** 维护窗口。 */
const maintenanceWindow = '每日 02:00 – 04:00';

// ---------- 进度 ----------
/** 更新步骤。 */
const steps = [
  { n: '1', label: '下载更新包', desc: '42.6 MB' },
  { n: '2', label: '校验签名与完整性', desc: 'Ed25519 + SHA-256 清单' },
  { n: '3', label: '备份当前版本与配置', desc: '→ rollback/v1.4.2' },
  { n: '4', label: '应用并重启服务', desc: '优雅停机，队列 flush 完成后重启' },
  { n: '5', label: '健康自检', desc: '采集 / 转发 / 授权 三项连通性' },
];

/** 进度百分比。 */
const progress = ref(0);

/** 已完成步骤索引（-1 未开始）。 */
const activeStep = ref(-1);

/** 步骤状态。 */
function stepState(index: number): string {
  if (index < activeStep.value) {
    return 'done';
  }
  if (index === activeStep.value) {
    return 'run';
  }
  return 'todo';
}

/** 开始更新（演示：立即置为进行中并给出提示）。 */
function startUpdate(): void {
  activeStep.value = 2;
  progress.value = 45;
  actionMessage.value = `已开始更新到 ${latestVersion}：正在备份当前版本并校验签名（Ed25519 + SHA-256）。`;
}

/** 检查更新。 */
function checkUpdate(): void {
  hasUpdate.value = true;
  actionMessage.value = `检查完成：发现新版本 ${latestVersion}（发布于 2h 前），签名主体 IoT-DAQ Release Signing。`;
}

/** 显示历史详情。 */
function showDetail(row: UpdateHistoryRow): void {
  actionMessage.value = `历史记录 ${row.version}（${row.time}）：结果 ${resultLabel(row)}，操作人 ${row.actor}，耗时 ${row.duration}。`;
}

/** 结果文案。 */
function resultLabel(row: UpdateHistoryRow): string {
  if (row.result === 'success') {
    return '成功';
  }
  if (row.result === 'warn') {
    return '回滚';
  }
  return '失败';
}

// ---------- 更新历史 ----------
/** 更新历史行。 */
interface UpdateHistoryRow {
  /** 版本 */
  version: string;
  /** 时间 */
  time: string;
  /** 操作人 */
  actor: string;
  /** 结果 */
  result: string;
  /** 耗时 */
  duration: string;
}

/** 更新历史（照搬原型）。 */
const history = ref<UpdateHistoryRow[]>([
  { version: 'v1.4.2', time: '2026-09-05 06:12', actor: 'admin', result: 'success', duration: '3m 12s' },
  { version: 'v1.4.1', time: '2026-08-11 06:08', actor: 'admin', result: 'success', duration: '2m 58s' },
  { version: 'v1.4.0', time: '2026-07-20 02:30', actor: 'auto', result: 'warn', duration: '—' },
  { version: 'v1.3.9', time: '2026-06-28 02:30', actor: 'auto', result: 'success', duration: '3m 01s' },
  { version: 'v1.3.8', time: '2026-05-30 02:30', actor: 'auto', result: 'success', duration: '2m 47s' },
  { version: 'v1.3.7', time: '2026-05-02 06:05', actor: 'admin', result: 'success', duration: '3m 20s' },
  { version: 'v1.3.6', time: '2026-04-11 02:30', actor: 'auto', result: 'success', duration: '2m 39s' },
  { version: 'v1.3.5', time: '2026-03-22 02:30', actor: 'auto', result: 'failed', duration: '0m 48s' },
  { version: 'v1.3.4', time: '2026-02-28 06:10', actor: 'admin', result: 'success', duration: '3m 05s' },
  { version: 'v1.3.3', time: '2026-02-01 02:30', actor: 'auto', result: 'success', duration: '2m 51s' },
]);

/** 历史总数（分页条唯一口径）。 */
const historyTotal = computed<number>(() => history.value.length);

const historyPage = ref(1);

/** 当前页历史。 */
const pagedHistory = computed<UpdateHistoryRow[]>(() => {
  const start = (historyPage.value - 1) * HISTORY_PAGE_SIZE;
  return history.value.slice(start, start + HISTORY_PAGE_SIZE);
});

/** 列定义。 */
const historyColumns: readonly TableColumn[] = [
  { key: 'version', label: '版本' },
  { key: 'time', label: '时间', mono: true },
  { key: 'actor', label: '操作人' },
  { key: 'result', label: '结果' },
  { key: 'duration', label: '耗时' },
];

/** 换页。 */
function onHistoryPage(next: number): void {
  historyPage.value = next;
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
  { label: '当前版本', value: currentVersion },
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

/** 回滚提交。 */
function onRollbackSubmit(payload: { reason: string; note: string; tail: string; secondApprover: string }): void {
  rollbackOpen.value = false;
  actionMessage.value = `已回滚到 ${rollbackTarget.value}（原因：${payload.reason}），服务将重启，操作已写入审计。`;
}

// ---------- 离线包上传（危险） ----------
const offlineOpen = ref(false);

/** 离线包上传影响清单。 */
const offlineImpacts: readonly string[] = [
  '上传并安装离线更新包会替换当前运行版本，服务将重启，采集中断约 5–15 秒。',
  '离线包必须带签名清单（.tar.zst + 签名），校验逻辑与在线一致，校验失败即拒绝安装。',
  '安装前会自动备份当前版本与配置，失败可回滚。',
];

/** 离线包对象摘要。 */
const offlineFacts: readonly DangerFact[] = [
  { label: '离线包文件名', value: 'iot-daq-offline.tar.zst' },
  { label: '包大小', value: '42.6 MB' },
  { label: '签名清单', value: 'iot-daq-offline.tar.zst.sig' },
];

/** 离线包导入原因枚举（必选）。 */
const OFFLINE_REASONS: readonly string[] = ['现场无外网', '内网安全策略', '指定版本部署', '灾备恢复'];

/** 打开离线包上传确认。 */
function openOffline(): void {
  if (!canEdit.value) {
    return;
  }
  offlineOpen.value = true;
}

/** 离线包提交。 */
function onOfflineSubmit(payload: { reason: string; note: string; tail: string; secondApprover: string }): void {
  offlineOpen.value = false;
  actionMessage.value = `离线包已导入（原因：${payload.reason}）：签名校验通过，正在按在线一致的流程应用。`;
}
</script>

<style scoped>
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
.wc-banner__icon {
  flex: 0 0 auto;
  font-weight: 700;
}
</style>
