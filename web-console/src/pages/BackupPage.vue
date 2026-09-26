<!--
  =============================================================================
  BackupPage —— 备份与恢复（设计 §3.7 / 原型 gateway-v2a-glacier「backup」）
  =============================================================================
  · 备份清单来自真实 `GET /api/settings/backups`（网关 config 目录写前备份）；
  · 「恢复」为**不可撤销**危险操作，走 DangerConfirmModal 四要素确认
    （影响清单 + 原因必填 + **备份文件名后 8 位二次校验**），确认后调真实
    `POST /api/settings/rollback`，结果按后端返回原样呈现；
  · 无端点支撑的能力（立即备份 / 下载 / 备份策略写）如实告知，不伪造成功。
-->
<template>
  <div class="wc-content">
    <!-- 工具条：立即备份 + 刷新清单（原页头右侧按钮迁入） -->
    <div class="pg-toolbar">
      <span class="wc-tag wc-tag--info">数据源 GET /api/settings/backups</span>
      <span class="wc-spacer" />
      <button type="button" class="wc-btn wc-btn--sm" data-testid="backup-refresh" @click="loadBackups">刷新清单</button>
      <RoleGate :allowed="canEdit" mode="disable" deny-text="当前角色为只读，不能立即备份">
        <button
          type="button"
          class="wc-btn wc-btn--primary wc-btn--sm"
          :disabled="backupBusy"
          data-testid="backup-now"
          @click="backupNow"
        >
          {{ backupBusy ? '备份中…' : '立即备份' }}
        </button>
      </RoleGate>
    </div>

    <p v-if="actionMessage" class="wc-hint" data-testid="backup-message">{{ actionMessage }}</p>

    <!-- ══ 备份列表（分页）═════════════════════════════════════════════ -->
    <section class="wc-card">
      <div class="wc-card__head">
        <h3>备份列表</h3>
        <span class="wc-card__sub">共 {{ backupTotal }} 条</span>
      </div>

      <EmptyState
        v-if="backupTotal === 0"
        title="还没有备份"
        desc="网关未返回任何备份文件。"
      >
        <template #actions>
          <button type="button" class="wc-btn" data-testid="backup-refresh-empty" @click="loadBackups">刷新清单</button>
        </template>
      </EmptyState>

      <template v-else>
        <UiTable
          :columns="columns"
          :rows="pagedBackups"
          row-key-field="id"
          footer="来源：网关 config 目录写前备份（PUT /api/settings 或回滚前自动生成）"
        >
          <template #cell-time="{ row }">
            <span class="wc-mono">{{ row.time }}</span>
          </template>
          <template #cell-kind="{ row }">
            <StatusTag :status="row.kind" :text="kindLabel(row.kind)" />
          </template>
          <template #cell-size="{ row }">
            <span class="wc-mono">{{ row.size }}</span>
          </template>
          <template #cell-location="{ row }">
            <span class="wc-mono">{{ row.location }}</span>
          </template>
          <template #actions="{ row }">
            <button type="button" class="wc-btn wc-btn--sm" :data-testid="`backup-download-${row.id}`" @click="download(row)">
              下载
            </button>
            <button
              type="button"
              class="wc-btn wc-btn--sm"
              :disabled="!canEdit"
              :data-testid="`backup-restore-${row.id}`"
              @click="openRestore(row)"
            >
              恢复
            </button>
          </template>
        </UiTable>

        <UiPager :page="page" :total="backupTotal" :page-size="PAGE_SIZE" @update:page="onPage" />
      </template>
    </section>

    <!-- ══ 备份策略 + 恢复（危险）══════════════════════════════════════ -->
    <div class="wc-grid wc-grid--2">
      <section class="wc-card">
        <div class="wc-card__head">
          <h3>备份策略</h3>
          <span class="wc-tag wc-tag--info" data-testid="backup-policy-source">数据源 GET|PUT /api/settings/backup-policy</span>
        </div>
        <div class="wc-card__body">
          <div class="bk-form-grid">
            <UiField label="写前自动备份" hint="保存配置 / 回滚前自动生成快照">
              <UiSwitch v-model="policy.autoBeforeWrite" :disabled="!canEdit || policyLoading" on-text="开启" off-text="关闭" />
            </UiField>
            <UiField label="保留份数" hint="超出后自动清理最旧的自产备份">
              <UiInput v-model="policy.retentionCount" :disabled="!canEdit || policyLoading" placeholder="20" />
            </UiField>
            <UiField label="周期备份间隔（分钟）" hint="0 = 关闭周期备份">
              <UiInput v-model="policy.intervalMin" :disabled="!canEdit || policyLoading" placeholder="0" />
            </UiField>
          </div>
          <button
            type="button"
            class="wc-btn"
            :disabled="!canEdit || policyBusy"
            data-testid="backup-save-strategy"
            @click="saveStrategy"
          >
            {{ policyBusy ? '保存中…' : '保存策略' }}
          </button>
        </div>
      </section>

      <section class="wc-card">
        <div class="wc-card__head">
          <h3>恢复</h3>
        </div>
        <div class="wc-card__body">
          <ul class="bk-impact">
            <li>当前配置将被所选备份<b>完全覆盖</b>且无法撤销。</li>
            <li>恢复后服务自动重启（采集中断约 5–15 秒）；已入队数据不丢失。</li>
            <li>授权状态随备份回滚；若期间发生过换机或重发，恢复后需重新激活。</li>
          </ul>
          <button
            type="button"
            class="wc-btn wc-btn--danger"
            style="width: 100%"
            :disabled="!canEdit || backups.length === 0"
            data-testid="backup-restore-latest"
            @click="openRestore(backups[0])"
          >
            从备份恢复（危险）
          </button>
        </div>
      </section>
    </div>
  </div>

  <!-- 恢复危险二次确认（备份文件名后 8 位二次校验） -->
  <DangerConfirmModal
    :open="restoreOpen"
    :title="`从备份恢复：${restoreTarget?.time ?? ''}`"
    :impacts="restoreImpacts"
    :facts="restoreFacts"
    :reasons="RESTORE_REASONS"
    :min-note-length="10"
    :confirm-value="restoreTarget?.id ?? ''"
    confirm-label="二次校验（输入备份标识后 8 位）"
    confirm-text="确认恢复"
    @close="restoreOpen = false"
    @submit="onRestoreSubmit"
  />
</template>

<script setup lang="ts">
/**
 * @file BackupPage.vue
 * @module web-console/pages/BackupPage
 * @description 备份与恢复页（真实备份清单 + 真实定向回滚 + 危险操作二次确认）。
 */
import { computed, onMounted, reactive, ref, watch } from 'vue';
import {
  UiTable,
  UiPager,
  UiInput,
  UiSwitch,
  UiField,
  StatusTag,
  EmptyState,
  RoleGate,
  DangerConfirmModal,
  type TableColumn,
  type DangerFact,
} from '@ui-kit';
import { dataVersion, repo, type SettingsBackupRow } from '@/api/repo';
import { apiRequest } from '@/api/client';
import { formatTimestampText } from '@/utils/time';
import { session } from '../store/session';

/** 每页条数。 */
const PAGE_SIZE = 5;

/** 当前角色是否可编辑。 */
const canEdit = computed<boolean>(() => session.state.role === 'admin' || session.state.role === 'engineer');

/** 备份记录（视图模型）。 */
interface BackupRecord {
  /** 主键 = 备份文件名（用于二次校验） */
  id: string;
  /** 备份时间 */
  time: string;
  /** 类型：config（配置写前备份） */
  kind: string;
  /** 大小 */
  size: string;
  /** 位置 */
  location: string;
}

/** 备份清单（真实：`GET /api/settings/backups`）。 */
const backups = ref<BackupRecord[]>([]);

/**
 * 备份大小（字节字符串 → 人类可读；解析失败原样展示）。
 *
 * 大数红线：`size_bytes` 为 JSON **字符串**编码的 uint64，绝不可 `Number()` /
 * `parseInt`（> 2^53−1 静默丢精度）——全程 `BigInt` 整数除法，末位才转小数。
 */
function formatBytes(sizeBytes: string): string {
  const text = sizeBytes.trim();
  if (!/^\d+$/.test(text)) {
    return sizeBytes || '—';
  }
  const bytes = BigInt(text);
  if (bytes < 1024n) {
    return `${bytes} B`;
  }
  const kb = bytes / 1024n;
  if (kb < 1024n) {
    return `${kb} KB`;
  }
  const mb = kb / 1024n;
  return mb < 1024n
    ? `${(Number(mb) / 1024).toFixed(1)} MB`
    : `${(Number(mb / 1024n) / 1024).toFixed(1)} GB`;
}

/**
 * 备份时刻（epoch 秒 / 毫秒字符串 → 本地时间文本）。
 *
 * 大数红线：绝不 `Number()` 未判定位数的整串；统一走共享展示工具，
 * 只认 10 位秒 / 13 位毫秒，其余（uint64、纳秒、非法值）一律回 `—`。
 */
function formatMtime(mtimeMs: string): string {
  return formatTimestampText(mtimeMs);
}

/** 拉取真实备份清单。 */
async function loadBackups(): Promise<void> {
  const result = await repo.settings.backups();
  if (!result.ok) {
    backups.value = [];
    actionMessage.value = `备份清单读取失败：${result.message}`;
    return;
  }
  backups.value = result.rows.map((row: SettingsBackupRow): BackupRecord => ({
    id: row.file,
    time: formatMtime(row.mtimeMs),
    kind: 'config',
    size: formatBytes(row.sizeBytes),
    location: row.file,
  }));
}

watch(dataVersion, () => {
  void loadBackups();
});

onMounted(() => {
  void loadBackups();
  void loadPolicy();
});

/** 备份总数（分页条唯一口径）。 */
const backupTotal = computed<number>(() => backups.value.length);

const page = ref(1);

/** 当前页备份。 */
const pagedBackups = computed<BackupRecord[]>(() => {
  const start = (page.value - 1) * PAGE_SIZE;
  return backups.value.slice(start, start + PAGE_SIZE);
});

/** 类型文案。 */
function kindLabel(kind: string): string {
  const map: Record<string, string> = { config: '配置写前备份' };
  return map[kind] ?? kind;
}

/** 列定义。 */
const columns: readonly TableColumn[] = [
  { key: 'time', label: '备份时间', mono: true },
  { key: 'kind', label: '类型' },
  { key: 'size', label: '大小', align: 'right' },
  { key: 'location', label: '位置', mono: true },
];

/** 换页。 */
function onPage(next: number): void {
  page.value = next;
}

/** 操作提示（真实结果原文）。 */
const actionMessage = ref('');

/** 备份写入中（防重复下发）。 */
const backupBusy = ref(false);

/**
 * 立即备份：真实 `POST /api/settings/backups`（无 body，`device.write` 守卫）。
 *
 * 后端按可读命名 `config.toml.YYYYMMDD-HHmmss-NNN.bak`（内嵌 UTC+8 时刻）落盘，
 * 清单接口自动可见，成功后重拉清单（不伪造条目）。
 */
async function backupNow(): Promise<void> {
  if (backupBusy.value) {
    return;
  }
  backupBusy.value = true;
  actionMessage.value = '';
  try {
    await apiRequest<unknown>('/api/settings/backups', { method: 'POST' });
    await loadBackups();
    actionMessage.value = `已创建备份（${backups.value[0]?.id ?? '最新一份'}）。`;
  } catch (cause) {
    const raw = cause instanceof Error ? cause.message : String(cause);
    actionMessage.value = `备份未创建：${raw}`;
  } finally {
    backupBusy.value = false;
  }
}

/** 下载备份：网关无下载端点，如实告知。 */
function download(row: BackupRecord): void {
  actionMessage.value = `下载未执行：网关未提供备份下载接口（${row.id}）。`;
}

// ---------- 备份策略（真实：GET|PUT /api/settings/backup-policy） ----------
/** 策略草稿（数字一律字符串，禁 parseInt）。 */
const policy = reactive({
  autoBeforeWrite: true,
  retentionCount: '20',
  intervalMin: '0',
});

/** 策略读取中 / 写入中。 */
const policyLoading = ref(false);
const policyBusy = ref(false);

/** 读取真实策略（失败时保留草稿并如实提示，不回退假值）。 */
async function loadPolicy(): Promise<void> {
  policyLoading.value = true;
  try {
    const result = await repo.settings.getBackupPolicy();
    if (result.ok && result.policy) {
      policy.autoBeforeWrite = result.policy.autoBeforeWrite;
      policy.retentionCount = result.policy.retentionCount;
      policy.intervalMin = result.policy.intervalMin;
    } else {
      actionMessage.value = `备份策略读取：${result.message}`;
    }
  } catch (cause) {
    const raw = cause instanceof Error ? cause.message : String(cause);
    actionMessage.value = `备份策略读取失败：${raw}`;
  } finally {
    policyLoading.value = false;
  }
}

/** 保存策略：真实 `PUT /api/settings/backup-policy`（reason 必填进审计）。 */
async function saveStrategy(): Promise<void> {
  if (policyBusy.value) {
    return;
  }
  policyBusy.value = true;
  actionMessage.value = '';
  try {
    const result = await repo.settings.putBackupPolicy({
      autoBeforeWrite: policy.autoBeforeWrite,
      retentionCount: policy.retentionCount,
      intervalMin: policy.intervalMin,
      reason: '备份策略调整',
      note: `写前自动备份=${policy.autoBeforeWrite ? '开启' : '关闭'}；保留份数=${policy.retentionCount}；周期间隔=${policy.intervalMin} 分钟`,
    });
    if (result.ok) {
      actionMessage.value = `备份策略已保存（保留 ${result.policy?.retentionCount ?? policy.retentionCount} 份）。`;
      await loadPolicy();
      await loadBackups();
    } else {
      actionMessage.value = `策略未保存：${result.message}`;
    }
  } catch (cause) {
    const raw = cause instanceof Error ? cause.message : String(cause);
    actionMessage.value = `策略未保存：${raw}`;
  } finally {
    policyBusy.value = false;
  }
}

// ---------- 恢复危险确认 ----------
const restoreOpen = ref(false);
const restoreTarget = ref<BackupRecord | null>(null);

/** 恢复影响清单。 */
const restoreImpacts: readonly string[] = [
  '当前配置将被所选备份**完全覆盖**，且无法撤销。',
  '恢复后服务自动重启，采集中断约 5–15 秒；已入队数据不丢失。',
  '授权状态随备份回滚：若期间发生过换机或重发，恢复后需重新激活。',
];

/** 恢复对象摘要。 */
const restoreFacts = computed<readonly DangerFact[]>(() => [
  { label: '备份标识', value: restoreTarget.value?.id ?? '—' },
  { label: '备份时间', value: restoreTarget.value?.time ?? '—' },
  { label: '大小', value: restoreTarget.value?.size ?? '—' },
  { label: '位置', value: restoreTarget.value?.location ?? '—' },
]);

/** 恢复原因枚举（必选）。 */
const RESTORE_REASONS: readonly string[] = ['配置错乱需回退', '误删设备/点位', '升级后异常回滚', '误操作修复'];

/** 打开恢复确认。 */
function openRestore(row: BackupRecord | undefined): void {
  if (!row || !canEdit.value) {
    return;
  }
  restoreTarget.value = row;
  restoreOpen.value = true;
}

/** 恢复提交：真实 `POST /api/settings/rollback`（定向到所选备份文件）。 */
async function onRestoreSubmit(payload: { reason: string; note: string; tail: string; secondApprover: string }): Promise<void> {
  restoreOpen.value = false;
  const target = restoreTarget.value;
  if (!target) {
    return;
  }
  // 危险三要素：reason（原因枚举）与 note（补充说明）**各自独立下发**，禁止拼接进同一字段。
  const result = await repo.settings.rollback({
    actor: session.state.displayName,
    reason: payload.reason,
    note: payload.note,
    backup: target.id,
  });
  actionMessage.value = result.ok
    ? `已从备份 ${target.id} 恢复${result.version ? `（配置版本 ${result.version}）` : ''}，服务将自动重启。`
    : `恢复未执行：${result.message}`;
  if (result.ok) {
    await loadBackups();
  }
}
</script>

<style scoped>
.pg-toolbar {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: 8px;
  min-height: 28px;
}
.bk-form-grid {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: 14px 16px;
}
@media (max-width: 1280px) {
  .bk-form-grid {
    grid-template-columns: 1fr;
  }
}
.bk-impact {
  margin: 0 0 12px;
  padding: 12px 14px 12px 32px;
  border: 1px solid var(--warn-border);
  background: var(--warn-bg);
  border-radius: var(--radius);
  font-size: var(--fs-caption);
  color: var(--warn-fg);
  line-height: 1.75;
}
</style>
