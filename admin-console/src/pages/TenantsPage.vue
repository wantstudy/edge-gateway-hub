<template>
  <!--
    TenantsPage —— 租户与策略（页面清单第 6 项，仅「系统」角色可见）。
    要点（ui-admin-console.md §3.6）：
      - 租户列表（设备数 / 已授权 / 默认档位 / 心跳 / 离线宽限 / 回执要求 / 联系人）
      - 策略编辑（校验档位 A/B/C 默认 B、心跳间隔、离线宽限、回执要求）
      - 修改档位属高影响操作 → 二次确认 + 原因必填 + 记审计
  -->
  <PageHeader
    crumb="授权运营 / 租户与策略"
    title="租户与策略"
    desc="租户默认策略与校验档位配置。档位变更属高影响操作：直接影响客户数据出网范围，须二次确认并记审计。"
  />

  <div class="ac-content">
    <!-- 全局横幅：讲清策略如何生效（可解释） -->
    <div class="ac-banner ac-banner--info">
      <div>
        策略随 <b>Lease Token</b> 下发，网关本地只读执行，客户端界面不可修改。
        「允许离线时长」即租约宽限期：超期后<b>停止北向转发、保留本地采集</b>，恢复联网并续租成功后自动恢复。
      </div>
    </div>

    <section class="ac-card">
      <div class="ac-card__head">
        <h3>租户列表</h3>
        <span class="ac-card__sub">共 {{ tenants.length }} 个租户</span>
      </div>
      <UiTable :columns="columns" :rows="tenants" row-key-field="id">
        <template #cell-defaultGrade="{ row }">
          <StatusTag
            :status="row.defaultGrade === 'A' ? 'ok' : row.defaultGrade === 'B' ? 'info' : 'unknown'"
            :text="`档位 ${row.defaultGrade}`"
          />
        </template>
        <template #cell-receiptRequired="{ row }">
          <StatusTag :status="row.receiptRequired ? 'ok' : 'unknown'" :text="row.receiptRequired ? '强制' : '不要求'" />
        </template>
        <template #cell-enabled="{ row }">
          <StatusTag :status="row.enabled ? 'user_enabled' : 'user_disabled'" />
        </template>
        <template #actions="{ row }">
          <button type="button" class="ac-btn ac-btn--sm" @click="openPolicy(row.id)">策略</button>
          <!-- 提交按钮文案按当前状态取反：启用中 → 显示「停用」 -->
          <button
            type="button"
            class="ac-btn ac-btn--sm"
            :class="{ 'ac-btn--danger': row.enabled }"
            @click="toggleEnabled(row)"
          >
            {{ row.enabled ? '停用' : '启用' }}
          </button>
        </template>
      </UiTable>
    </section>

    <!-- 校验档位说明 -->
    <section class="ac-card">
      <div class="ac-card__head">
        <h3>校验档位说明（A / B / C）</h3>
        <span class="ac-card__sub">档位即「厂商可见范围」的契约</span>
      </div>
      <UiTable :columns="gradeColumns" :rows="gradeRows" row-key-field="grade">
        <template #cell-grade="{ row }">
          <StatusTag
            :status="row.grade === 'A' ? 'ok' : row.grade === 'B' ? 'info' : 'unknown'"
            :text="`档位 ${row.grade}`"
          />
        </template>
      </UiTable>
      <div class="ac-card__body">
        <p class="ac-note">
          <span class="ac-note__icon">ⓘ</span>
          <span>
            B 档（默认）：业务数据直连客户 Broker，网关侧签名 + 单调序号 + 本地去重，上报审计回执；
            厂商<b>仅见回执（序号区间 / 条数 / 摘要哈希），无业务数值</b>。
          </span>
        </p>
      </div>
    </section>
  </div>

  <!-- 策略编辑弹窗（草稿隔离：仅在打开时初始化） -->
  <Teleport to="body">
    <div v-if="policyOpen" class="ac-modal-mask" @click.self="closePolicy">
      <div class="ac-modal" role="dialog" aria-modal="true" aria-label="租户策略">
        <div class="ac-modal__head">
          <h3>租户策略 · {{ policyTarget?.name ?? '' }}</h3>
        </div>
        <div class="ac-modal__body">
          <div class="ac-grid ac-grid--2">
            <UiField label="默认校验档位" required hint="A / B / C，默认 B；改动影响客户数据出网范围">
              <UiSelect v-model="policyForm.defaultGrade" :options="gradeOptions" />
            </UiField>
            <UiField label="默认 tier" required>
              <UiSelect v-model="policyForm.defaultTier" :options="tierOptions" />
            </UiField>
            <UiField label="心跳间隔" required hint="租约保活周期">
              <UiSelect v-model="policyForm.heartbeatInterval" :options="heartbeatOptions" />
            </UiField>
            <UiField label="离线宽限（允许离线时长）" required hint="超期后停北向转发、保留本地采集">
              <UiSelect v-model="policyForm.offlineGrace" :options="graceOptions" />
            </UiField>
            <UiField class="ac-modal__full" label="强制回执" hint="B 档核心证据；C 档通常不要求" full>
              <UiSwitch v-model="policyForm.receiptRequired" on-text="强制回执" off-text="不要求回执" />
            </UiField>
          </div>

          <!-- 高危变更的说明与原因（策略变更需二次确认 + 原因） -->
          <div class="ac-banner ac-banner--warn">
            <div>
              修改档位 / 宽限属<b>高影响操作</b>：将改变客户设备的出网通道与离线容忍度，
              请填写变更原因，提交后将记入审计日志。
            </div>
          </div>
          <UiField label="变更原因" required :error="policyError" :hint="`必填，≥ ${MIN_REASON} 字`">
            <UiTextarea
              v-model="policyForm.reason"
              :rows="3"
              :min-length="MIN_REASON"
              show-counter
              :invalid="policyError.length > 0"
              placeholder="例如：客户要求最小化数据外发，B → C"
            />
          </UiField>
        </div>
        <div class="ac-modal__foot">
          <button type="button" class="ac-btn" @click="closePolicy">取消</button>
          <button type="button" class="ac-btn ac-btn--primary" :disabled="!canSubmitPolicy" @click="submitPolicy">
            保存策略
          </button>
        </div>
      </div>
    </div>
  </Teleport>
</template>

<script setup lang="ts">
/**
 * @file TenantsPage.vue
 * @module admin-console/pages/TenantsPage
 * @description 租户与策略页（仅 system 角色）。
 */
import { computed, reactive, ref } from 'vue';
import {
  PageHeader,
  UiField,
  UiSelect,
  UiSwitch,
  UiTextarea,
  UiTable,
  StatusTag,
  type SelectOption,
  type TableColumn,
} from '@ui-kit';
import { repo, TIER_NAMES, DEFAULT_ACTOR, type TenantRecord } from '../api/repo';

/** 变更原因最小字数。 */
const MIN_REASON = 10;

/** 刷新触发器。 */
const reloadKey = ref(0);

/** 租户列表。 */
const tenants = computed(() => {
  void reloadKey.value;
  return repo.allTenants();
});

/** 列定义。 */
const columns: readonly TableColumn[] = [
  { key: 'name', label: '租户' },
  { key: 'deviceCount', label: '设备', align: 'right' },
  { key: 'licensedCount', label: '已授权', align: 'right' },
  { key: 'defaultGrade', label: '默认档位' },
  { key: 'defaultTier', label: '默认 tier' },
  { key: 'heartbeatInterval', label: '心跳间隔' },
  { key: 'offlineGrace', label: '离线宽限' },
  { key: 'receiptRequired', label: '回执要求' },
  { key: 'contact', label: '联系人' },
  { key: 'enabled', label: '状态' },
];

/** 档位说明表。 */
interface GradeRow {
  grade: string;
  how: string;
  see: string;
  risk: string;
}

/** 档位说明数据。 */
const gradeRows: readonly GradeRow[] = [
  {
    grade: 'A',
    how: '数据经厂商云 /verify：验签 + ±5min 窗口 + 全局 Nonce + 白名单 + 计量',
    see: '全量业务数据',
    risk: '客户对数据出网敏感，需单独签约',
  },
  {
    grade: 'B',
    how: '业务数据直连客户 Broker；网关侧签名 + 单调序号 + 本地去重；上报审计回执',
    see: '仅回执（序号区间 / 条数 / 摘要哈希，无业务数值）',
    risk: '无法阻止破解后完全断网静默自用',
  },
  {
    grade: 'C',
    how: '仅租约心跳',
    see: '仅心跳',
    risk: '证据最少，仅适合信任关系场景',
  },
];

/** 档位列定义。 */
const gradeColumns: readonly TableColumn[] = [
  { key: 'grade', label: '档位' },
  { key: 'how', label: '校验方式' },
  { key: 'see', label: '厂商可见范围' },
  { key: 'risk', label: '风险' },
];

/** 下拉选项。 */
const gradeOptions: readonly SelectOption[] = [
  { value: 'A', label: 'A（/verify 全链校验）' },
  { value: 'B', label: 'B（默认 · 心跳 + 回执）' },
  { value: 'C', label: 'C（仅心跳）' },
];
const tierOptions: readonly SelectOption[] = TIER_NAMES.map((t) => ({ value: t, label: t }));
const heartbeatOptions: readonly SelectOption[] = [
  { value: '1 小时', label: '1 小时' },
  { value: '12 小时', label: '12 小时' },
  { value: '24 小时', label: '24 小时（默认）' },
];
const graceOptions: readonly SelectOption[] = [
  { value: '24 小时', label: '24 小时' },
  { value: '3 天', label: '3 天' },
  { value: '7 天（默认）', label: '7 天（默认）' },
  { value: '14 天', label: '14 天' },
];

// ---------------- 策略弹窗 ----------------
/** 策略弹窗开关。 */
const policyOpen = ref(false);
/** 策略目标 id。 */
const policyTargetId = ref('');

/** 策略目标快照。 */
const policyTarget = computed<TenantRecord | null>(() => (policyTargetId.value ? repo.getTenant(policyTargetId.value) : null));

/** 策略草稿（独立对象）。 */
const policyForm = reactive({
  defaultGrade: 'B',
  defaultTier: TIER_NAMES[0],
  heartbeatInterval: '24 小时',
  offlineGrace: '7 天（默认）',
  receiptRequired: true,
  reason: '',
});

/** 原因校验。 */
const policyError = computed(() =>
  policyForm.reason.trim().length > 0 && policyForm.reason.trim().length < MIN_REASON ? `还差 ${MIN_REASON - policyForm.reason.trim().length} 字` : '',
);

/** 可提交。 */
const canSubmitPolicy = computed(() => policyForm.reason.trim().length >= MIN_REASON);

/** 打开策略弹窗：从当前对象**拷贝**到草稿（绝不直接引用）。 */
function openPolicy(id: string): void {
  const tenant = repo.getTenant(id);
  if (!tenant) {
    return;
  }
  policyTargetId.value = id;
  policyForm.defaultGrade = tenant.defaultGrade;
  policyForm.defaultTier = tenant.defaultTier;
  policyForm.heartbeatInterval = tenant.heartbeatInterval;
  policyForm.offlineGrace = tenant.offlineGrace;
  policyForm.receiptRequired = tenant.receiptRequired;
  policyForm.reason = '';
  policyOpen.value = true;
}

/** 关闭策略弹窗：清草稿（避免下次打开草稿「复活」）。 */
function closePolicy(): void {
  policyOpen.value = false;
  policyTargetId.value = '';
  policyForm.reason = '';
}

/** 保存策略。 */
function submitPolicy(): void {
  if (!canSubmitPolicy.value || !policyTargetId.value) {
    return;
  }
  void repo.updateTenant({
    id: policyTargetId.value,
    defaultGrade: policyForm.defaultGrade as 'A' | 'B' | 'C',
    defaultTier: policyForm.defaultTier,
    heartbeatInterval: policyForm.heartbeatInterval,
    offlineGrace: policyForm.offlineGrace,
    receiptRequired: policyForm.receiptRequired,
    reason: policyForm.reason.trim(),
    actor: DEFAULT_ACTOR,
  });
  closePolicy();
  reloadKey.value += 1;
}

/** 启用 / 停用租户（按钮文案随状态取反，见模板）。 */
function toggleEnabled(row: TenantRecord): void {
  void repo.setTenantEnabled({ id: row.id, enabled: !row.enabled, actor: DEFAULT_ACTOR });
  reloadKey.value += 1;
}
</script>

<style scoped>
.ac-modal-mask {
  position: fixed;
  inset: 0;
  background: rgba(29, 33, 41, 0.45);
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 2000;
  padding: 24px;
}
.ac-modal {
  background: #fff;
  border-radius: var(--radius);
  box-shadow: var(--shadow);
  width: 660px;
  max-width: 100%;
  max-height: 88vh;
  display: flex;
  flex-direction: column;
}
.ac-modal__head {
  padding: 16px 20px;
  border-bottom: 1px solid var(--divider);
}
.ac-modal__head h3 {
  margin: 0;
  font-size: 16px;
  font-weight: 600;
}
.ac-modal__body {
  padding: 20px;
  overflow: auto;
  display: flex;
  flex-direction: column;
  gap: 14px;
}
.ac-modal__foot {
  padding: 14px 20px;
  border-top: 1px solid var(--divider);
  display: flex;
  justify-content: flex-end;
  gap: 8px;
}
.ac-modal__full {
  grid-column: 1 / -1;
}
</style>
