<!--
  =============================================================================
  BackupPage —— 备份与恢复（设计 §3.7 / 原型 gateway-v2a-glacier「backup」）
  =============================================================================
  配置与授权状态的备份、导出与恢复；**恢复不可撤销**，需二次确认
  （DangerConfirmModal：影响清单 + 原因必填 + 对象名二次校验 + 可选双人复核）。
  备份列表分页（UiPager，条数只此一个口径）。
  Docker 部署时备份位置须指向宿主机持久卷（提示）。
-->
<template>
  <PageHeader
    crumb="运维 / 备份与恢复"
    title="备份与恢复"
    desc="配置与授权状态的备份、导出与恢复；每日 02:00 自动备份，最多保留 30 份。恢复不可撤销。"
  >
    <template #actions>
      <RoleGate :allowed="canEdit" mode="disable" deny-text="当前角色为只读，不能立即备份">
        <button type="button" class="wc-btn wc-btn--primary" data-testid="backup-now" @click="backupNow">
          立即备份
        </button>
      </RoleGate>
    </template>
  </PageHeader>

  <div class="wc-content">
    <p v-if="actionMessage" class="wc-hint" data-testid="backup-message">{{ actionMessage }}</p>

    <!-- ══ 备份列表（分页）═════════════════════════════════════════════ -->
    <section class="wc-card">
      <div class="wc-card__head">
        <h3>备份列表</h3>
        <span class="wc-card__sub">每日 02:00 自动备份，最多保留 30 份 · 共 {{ backupTotal }} 条</span>
      </div>

      <EmptyState
        v-if="backupTotal === 0"
        title="还没有备份"
        desc="备份包含 config.toml、点位表、转发规则与授权状态（不含私钥）。可点击「立即备份」生成第一份。"
      >
        <template #actions>
          <button type="button" class="wc-btn wc-btn--primary" @click="backupNow">立即备份</button>
        </template>
      </EmptyState>

      <template v-else>
        <UiTable
          :columns="columns"
          :rows="pagedBackups"
          row-key-field="id"
          footer="备份内容：config.toml · 点位表 · 转发规则 · 授权状态（不含私钥）"
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
        </div>
        <div class="wc-card__body">
          <div class="bk-form-grid">
            <UiField label="自动备份">
              <UiSelect v-model="strategy.mode" :options="modeOptions" :disabled="!canEdit" />
            </UiField>
            <UiField label="执行时间">
              <UiInput v-model="strategy.time" :disabled="!canEdit" placeholder="02:00" />
            </UiField>
            <UiField label="保留份数" hint="超出后自动清理最旧的一份">
              <UiInput v-model="strategy.keep" :disabled="!canEdit" placeholder="7" />
            </UiField>
            <UiField label="备份位置" hint="Docker 部署时须指向宿主机持久卷，否则容器重建即丢">
              <UiInput v-model="strategy.location" :disabled="!canEdit" placeholder="./backup" />
            </UiField>
          </div>
          <button type="button" class="wc-btn" :disabled="!canEdit" data-testid="backup-save-strategy" @click="saveStrategy">
            保存策略
          </button>
        </div>
      </section>

      <section class="wc-card">
        <div class="wc-card__head">
          <h3>恢复</h3>
        </div>
        <div class="wc-card__body">
          <div class="wc-banner wc-banner--danger">
            <span class="wc-banner__icon">!</span>
            <span>
              恢复操作的影响：当前配置与点位表将被<b>完全覆盖</b>且无法撤销；恢复后服务自动重启（采集中断约 5–15 秒）；
              授权状态随备份回滚，若期间发生过换机或重发，恢复后需重新激活。
            </span>
          </div>
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

  <!-- 恢复危险二次确认 -->
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
 * @description 备份与恢复页（备份列表分页 + 危险恢复二次确认）。
 */
import { computed, reactive, ref } from 'vue';
import {
  PageHeader,
  UiTable,
  UiPager,
  UiInput,
  UiSelect,
  UiField,
  StatusTag,
  EmptyState,
  RoleGate,
  DangerConfirmModal,
  type TableColumn,
  type SelectOption,
  type DangerFact,
} from '@ui-kit';
import { session } from '../store/session';

/** 每页条数。 */
const PAGE_SIZE = 5;

/** 当前角色是否可编辑。 */
const canEdit = computed<boolean>(() => session.state.role === 'admin' || session.state.role === 'engineer');

/** 备份记录。 */
interface BackupRecord {
  /** 主键（用于二次校验） */
  id: string;
  /** 备份时间 */
  time: string;
  /** 类型：auto / manual / pre_upgrade */
  kind: string;
  /** 大小 */
  size: string;
  /** 位置 */
  location: string;
}

/** 备份清单（照搬原型）。 */
const backups = ref<BackupRecord[]>([
  { id: 'bk-20260923-0200', time: '2026-09-23 02:00', kind: 'auto', size: '1.2 MB', location: './backup/20260923-0200' },
  { id: 'bk-20260922-0200', time: '2026-09-22 02:00', kind: 'auto', size: '1.2 MB', location: './backup/20260922-0200' },
  { id: 'bk-20260921-1540', time: '2026-09-21 15:40', kind: 'manual', size: '1.3 MB', location: './backup/20260921-1540' },
  { id: 'bk-20260921-0200', time: '2026-09-21 02:00', kind: 'auto', size: '1.2 MB', location: './backup/20260921-0200' },
  { id: 'bk-20260920-0200', time: '2026-09-20 02:00', kind: 'auto', size: '1.1 MB', location: './backup/20260920-0200' },
  { id: 'bk-rollback-v1.4.1', time: '2026-09-19 18:22', kind: 'pre_upgrade', size: '1.1 MB', location: './backup/rollback-v1.4.1' },
  { id: 'bk-20260919-0200', time: '2026-09-19 02:00', kind: 'auto', size: '1.1 MB', location: './backup/20260919-0200' },
  { id: 'bk-20260918-0200', time: '2026-09-18 02:00', kind: 'auto', size: '1.1 MB', location: './backup/20260918-0200' },
  { id: 'bk-20260917-0200', time: '2026-09-17 02:00', kind: 'auto', size: '1.0 MB', location: './backup/20260917-0200' },
]);

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
  const map: Record<string, string> = { auto: '自动', manual: '手动', pre_upgrade: '升级前' };
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

/** 操作提示。 */
const actionMessage = ref('');

/** 立即备份。 */
function backupNow(): void {
  const stamp = compactStamp();
  const id = `bk-${stamp}`;
  backups.value.unshift({
    id,
    time: displayStamp(),
    kind: 'manual',
    size: '1.3 MB',
    location: `./backup/${stamp}`,
  });
  page.value = 1;
  actionMessage.value = `已生成手动备份 ${id}（${displayStamp()}，1.3 MB）。`;
}

/** 下载备份。 */
function download(row: BackupRecord): void {
  actionMessage.value = `开始下载备份 ${row.id}（${row.size}）。`;
}

/** 紧凑时间戳（YYYYMMDD-HHmm）。 */
function compactStamp(): string {
  const d = new Date();
  const p = (n: number): string => String(n).padStart(2, '0');
  return `${d.getFullYear()}${p(d.getMonth() + 1)}${p(d.getDate())}-${p(d.getHours())}${p(d.getMinutes())}`;
}

/** 展示时间戳。 */
function displayStamp(): string {
  const d = new Date();
  const p = (n: number): string => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

// ---------- 备份策略 ----------
/** 策略草稿。 */
const strategy = reactive({
  mode: 'daily',
  time: '02:00',
  keep: '7',
  location: './backup',
});

/** 策略模式选项。 */
const modeOptions: readonly SelectOption[] = [
  { value: 'daily', label: '每日' },
  { value: 'weekly', label: '每周' },
  { value: 'off', label: '关闭' },
];

/** 保存策略。 */
function saveStrategy(): void {
  actionMessage.value = `备份策略已保存：${modeOptions.find((o) => o.value === strategy.mode)?.label ?? strategy.mode} / ${strategy.time} / 保留 ${strategy.keep} 份。`;
}

// ---------- 恢复危险确认 ----------
const restoreOpen = ref(false);
const restoreTarget = ref<BackupRecord | null>(null);

/** 恢复影响清单。 */
const restoreImpacts: readonly string[] = [
  '当前配置与点位表将被所选备份**完全覆盖**，且无法撤销。',
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

/** 恢复提交。 */
function onRestoreSubmit(payload: { reason: string; note: string; tail: string; secondApprover: string }): void {
  restoreOpen.value = false;
  actionMessage.value = `已从备份 ${restoreTarget.value?.id ?? ''} 恢复（原因：${payload.reason}），服务将自动重启，操作已写入审计。`;
}
</script>

<style scoped>
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
.wc-banner__icon {
  flex: 0 0 auto;
  font-weight: 700;
}
</style>
