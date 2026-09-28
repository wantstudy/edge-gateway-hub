<template>
  <!--
    TransfersPage —— 换机工单（页面清单第 8 项，仅「授权运营」可见）。
    核心业务目标（ui-admin-console.md §4.2）：**≤3 次点击完成换机**
      1) 在列表看到工单
      2) 点「处理」
      3) 在弹窗点「废弃并重发」→ 二次确认
    支持「废弃 + 重发」合并执行（同一事务语义），并展示 reissued_from 溯源。
  -->
  <PageHeader
    crumb="风控 / 换机工单"
    title="换机工单"
    desc="客服处理客户换机的唯一入口。一键执行「废弃旧码 + 重发新码」，整体 ≤3 次点击完成；两步在同一事务内，接口幂等。"
  >
    <template #actions>
      <button type="button" class="ac-btn" @click="toggleOnlyPending">
        {{ onlyPending ? '显示全部工单' : '仅看待处理' }}
      </button>
    </template>
  </PageHeader>

  <div class="ac-content">
    <section class="ac-card">
      <div class="ac-card__head">
        <h3>{{ onlyPending ? '待处理工单' : '全部工单' }}</h3>
        <span class="ac-card__sub">目标 ≤3 次点击完成</span>
      </div>

      <EmptyState
        v-if="tickets.length === 0"
        title="没有待处理的换机工单"
        desc="当前没有客户提交换机申请。若需要直接换机，可到激活码列表对已绑定码执行「废弃 + 重发」。"
      >
        <template #actions>
          <button type="button" class="ac-btn" @click="onlyPending = false">显示全部工单</button>
          <button type="button" class="ac-btn ac-btn--primary" @click="goCodes">去激活码管理</button>
        </template>
      </EmptyState>

      <template v-else>
        <UiTable :columns="columns" :rows="tickets" row-key-field="id">
          <template #cell-id="{ row }">
            <span class="ac-mono">{{ row.id }}</span>
          </template>
          <template #cell-oldMachineSummary="{ row }">
            <span class="ac-mono">{{ row.oldMachineSummary }}</span>
          </template>
          <template #cell-newMachineCode="{ row }">
            <span class="ac-mono">{{ row.newMachineCode ? maskMachineCode(row.newMachineCode) : '（未提供）' }}</span>
          </template>
          <template #cell-submittedAt="{ row }">
            <span class="ac-mono">{{ formatDateTime(row.submittedAt) }}</span>
          </template>
          <template #cell-status="{ row }">
            <StatusTag :status="row.status" />
          </template>
          <template #cell-resolution="{ row }">
            {{ row.resolution || '—' }}
          </template>
          <template #actions="{ row }">
            <span v-if="row.status === 'processed'" class="ac-note">已处理</span>
            <button v-else type="button" class="ac-btn ac-btn--sm ac-btn--primary" @click="openProcess(row)">
              处理
            </button>
          </template>
        </UiTable>
      </template>
    </section>

    <!-- 处理说明（客服口径） -->
    <section class="ac-card">
      <div class="ac-card__head"><h3>处理说明</h3></div>
      <div class="ac-card__body">
        <div class="ac-list">
          <div class="ac-list__item">
            <div>
              <div class="ac-list__title">「废弃 + 重发」合并执行</div>
              <div class="ac-list__desc">
                两步在同一事务内完成；接口幂等 —— 重复提交同一请求不会产生第二个新码。
              </div>
            </div>
          </div>
          <div class="ac-list__item">
            <div>
              <div class="ac-list__title">重发不自动解绑原设备</div>
              <div class="ac-list__desc">
                原设备已随废弃失效（≤24h 停止北向转发），无需也无法在客户端解绑。
              </div>
            </div>
          </div>
          <div class="ac-list__item">
            <div>
              <div class="ac-list__title">容器重建不算换机</div>
              <div class="ac-list__desc">同一宿主上容器重建时宿主锚点指纹一致，无需换机重发。</div>
            </div>
          </div>
        </div>
      </div>
    </section>
  </div>

  <!-- 处理工单弹窗（草稿隔离） -->
  <Teleport to="body">
    <div v-if="processOpen" class="ac-modal-mask" @click.self="closeProcess">
      <div class="ac-modal" role="dialog" aria-modal="true" aria-label="处理换机工单">
        <div class="ac-modal__head">
          <h3>处理工单 {{ processTarget?.id ?? '' }}</h3>
        </div>
        <div class="ac-modal__body">
          <dl class="ac-kv">
            <dt>原激活码</dt>
            <dd class="ac-mono">{{ sourceCodeMasked }}（已绑定 {{ processTarget?.oldMachineSummary ?? '—' }}）</dd>
            <dt>原设备影响</dt>
            <dd>废弃后将在下次心跳（≤24h）后停止北向转发，本地采集继续</dd>
            <dt>新机器码</dt>
            <dd class="ac-mono">{{ newMachineStatus }}</dd>
          </dl>

          <UiField label="绑定模式" required>
            <UiRadio v-model="processForm.mode" :options="modeOptions" />
          </UiField>

          <div class="ac-grid ac-grid--2">
            <UiField label="继承 tier" required>
              <UiSelect v-model="processForm.tier" :options="tierOptions" />
            </UiField>
            <UiField label="有效期" required>
              <UiSelect v-model="processForm.validity" :options="validityOptions" />
            </UiField>
            <UiField class="ac-modal__full" label="处理备注" required :error="noteError" :hint="`必填，≥ ${MIN_NOTE} 字`" full>
              <UiTextarea
                v-model="processForm.note"
                :rows="3"
                :min-length="MIN_NOTE"
                show-counter
                :invalid="noteError.length > 0"
                placeholder="例如：客户系统重装导致机器码变化，已确认非换硬件"
              />
            </UiField>
          </div>
        </div>
        <div class="ac-modal__foot">
          <button type="button" class="ac-btn" @click="closeProcess">取消</button>
          <button
            type="button"
            class="ac-btn ac-btn--danger"
            :disabled="!canSubmitProcess"
            @click="submitOnlyRevoke"
          >
            仅废弃旧码
          </button>
          <button
            type="button"
            class="ac-btn ac-btn--primary"
            :disabled="!canSubmitProcess"
            @click="confirmReissueOpen = true"
          >
            废弃并重发
          </button>
        </div>
      </div>
    </div>
  </Teleport>

  <!-- 二次确认（废弃 + 重发，高危四要素） -->
  <DangerConfirmModal
    :open="confirmReissueOpen"
    :title="`确认废弃并重发（工单 ${processTarget?.id ?? ''}）`"
    :impacts="[
      `旧码 ${sourceCodeMasked} 立即废弃；设备 ${processTarget?.oldMachineSummary ?? '—'} 将在下次心跳后停止北向转发（本地采集继续）。`,
      processForm.mode === 'prebind'
        ? `生成新码并预绑定机器码 ${maskMachineCode(processTarget?.newMachineCode ?? '')}，建立 reissued_from 溯源链。`
        : '生成新码，留待客户在新机首次激活时绑定，建立 reissued_from 溯源链。',
      '两步在同一事务内完成，接口幂等：重复提交不会产生第二个结果。',
    ]"
    :facts="confirmFacts"
    :reasons="REVOKE_REASONS"
    :confirm-value="sourceCodeRaw"
    confirm-label="风险二次确认（请输入原激活码后 8 位）"
    confirm-placeholder="去分隔符后 8 位"
    confirm-text="确认废弃并重发"
    @close="confirmReissueOpen = false"
    @submit="submitProcess"
  />
</template>

<script setup lang="ts">
/**
 * @file TransfersPage.vue
 * @module admin-console/pages/TransfersPage
 * @description 换机工单页（≤3 次点击完成换机）。
 */
import { computed, reactive, ref } from 'vue';
import { useRouter } from 'vue-router';
import {
  PageHeader,
  UiField,
  UiSelect,
  UiTextarea,
  UiRadio,
  UiTable,
  StatusTag,
  EmptyState,
  DangerConfirmModal,
  formatDateTime,
  maskMachineCode,
  maskCode,
  type SelectOption,
  type RadioOption,
  type TableColumn,
} from '@ui-kit';
import { repo, REVOKE_REASONS, TIER_NAMES, DEFAULT_ACTOR, type TransferTicket } from '../api/repo';

const router = useRouter();

/** 备注最小字数。 */
const MIN_NOTE = 10;

/** 刷新触发器。 */
const reloadKey = ref(0);

/** 是否仅看待处理。 */
const onlyPending = ref(true);

/** 工单列表。 */
const tickets = computed(() => {
  void reloadKey.value;
  const all = repo.allTransfers();
  return onlyPending.value ? all.filter((t) => t.status === 'pending') : all;
});

/** 列定义。 */
const columns: readonly TableColumn[] = [
  { key: 'id', label: '受理编号', mono: true },
  { key: 'tenant', label: '租户' },
  { key: 'oldMachineSummary', label: '原机器码', mono: true },
  { key: 'newMachineCode', label: '新机器码', mono: true },
  { key: 'reason', label: '原因' },
  { key: 'submittedAt', label: '提交时间', mono: true },
  { key: 'status', label: '状态' },
  { key: 'resolution', label: '处理结果' },
];

/** 切换筛选。 */
function toggleOnlyPending(): void {
  onlyPending.value = !onlyPending.value;
}

/** 跳激活码管理。 */
function goCodes(): void {
  void router.push({ name: 'codes' });
}

// ---------------- 处理弹窗 ----------------
/** 处理弹窗开关。 */
const processOpen = ref(false);
/** 二次确认开关。 */
const confirmReissueOpen = ref(false);
/** 目标工单 id。 */
const processTargetId = ref('');

/** 目标工单快照。 */
const processTarget = computed<TransferTicket | null>(() =>
  processTargetId.value ? repo.allTransfers().find((t) => t.id === processTargetId.value) ?? null : null,
);

/** 原激活码原文（用于后 8 位校验）。 */
const sourceCodeRaw = computed(() => {
  const id = processTarget.value?.sourceCodeId;
  return id ? repo.getCode(id)?.code ?? '' : '';
});

/** 原激活码掩码显示。 */
const sourceCodeMasked = computed(() => maskCode(sourceCodeRaw.value));

/** 处理草稿（独立对象）。 */
const processForm = reactive({
  mode: 'prebind' as 'prebind' | 'later',
  tier: TIER_NAMES[0],
  validity: '12m',
  note: '',
});

/** 新机器码状态文案（校验占用情况）。 */
const newMachineStatus = computed(() => {
  const code = processTarget.value?.newMachineCode;
  if (!code) {
    return '（客户未提供，将留待首次激活时绑定）';
  }
  const occupied = repo.allDevices().some((d) => d.machineCode.toUpperCase() === code.replace(/[^A-Za-z0-9]/g, '').toUpperCase());
  return occupied ? `${maskMachineCode(code)}（已被占用 ✗）` : `${maskMachineCode(code)}（未被占用 ✓）`;
});

/** 备注错误。 */
const noteError = computed(() =>
  processForm.note.trim().length > 0 && processForm.note.trim().length < MIN_NOTE
    ? `还差 ${MIN_NOTE - processForm.note.trim().length} 字`
    : '',
);

/** 可提交（备注达标 + 预绑定模式需有机器码）。 */
const canSubmitProcess = computed(() => {
  if (processForm.note.trim().length < MIN_NOTE) {
    return false;
  }
  if (processForm.mode === 'prebind' && !processTarget.value?.newMachineCode) {
    return false;
  }
  return true;
});

/** 绑定模式选项。 */
const modeOptions: readonly RadioOption[] = [
  { value: 'prebind', label: '预绑定新机器码', desc: '该码只能在新机器上激活（需客户提供新机器码）' },
  { value: 'later', label: '留待首次激活时绑定', desc: '推荐。客户在新机上自助激活，无需提前收集机器码' },
];

/** 有效期选项。 */
const validityOptions: readonly SelectOption[] = [
  { value: '12m', label: '顺延 12 个月' },
  { value: 'keep', label: '沿用源码到期日' },
];

/** tier 选项。 */
const tierOptions: readonly SelectOption[] = TIER_NAMES.map((t) => ({ value: t, label: t }));

/** 打开处理弹窗：从工单初始化草稿（绝不写回真实数据）。 */
function openProcess(row: TransferTicket): void {
  processTargetId.value = row.id;
  processForm.mode = row.newMachineCode ? 'prebind' : 'later';
  processForm.tier = '专业版';
  processForm.validity = '12m';
  processForm.note = '';
  processOpen.value = true;
}

/** 关闭：清草稿。 */
function closeProcess(): void {
  processOpen.value = false;
  confirmReissueOpen.value = false;
  processTargetId.value = '';
  processForm.note = '';
}

/** 二次确认弹窗的对象摘要。 */
const confirmFacts = computed(() => [
  { label: '工单', value: processTarget.value?.id ?? '—' },
  { label: '原码', value: sourceCodeMasked.value },
  { label: '新机器码', value: processTarget.value?.newMachineCode ? maskMachineCode(processTarget.value.newMachineCode) : '（留待激活）' },
  { label: '绑定模式', value: processForm.mode === 'prebind' ? '预绑定新机器码' : '留待首次激活绑定' },
]);

/** 计算重发的有效期日期。 */
function resolveValidUntil(): string {
  // 「沿用源码到期日」：源码记录在缓存中时用其真实到期日；取不到时不写死固定日期，
  // 回退到「当前日期顺延 12 个月」（与另一分支同口径，绝不伪造固定日期）。
  if (processForm.validity === 'keep') {
    const source = repo.getCode(processTarget.value?.sourceCodeId ?? '')?.validUntil;
    if (source) {
      return source;
    }
  }
  const next = new Date();
  next.setFullYear(next.getFullYear() + 1);
  return `${next.getFullYear()}-${String(next.getMonth() + 1).padStart(2, '0')}-${String(next.getDate()).padStart(2, '0')}`;
}

/** 仅废弃旧码（real：等待后端确认后才关弹窗刷新）。 */
async function submitOnlyRevoke(): Promise<void> {
  if (!canSubmitProcess.value || !processTarget.value) {
    return;
  }
  await repo.processTransfer({
    ticketId: processTarget.value.id,
    prebindNew: false,
    inheritTier: processForm.tier,
    validUntil: resolveValidUntil(),
    note: processForm.note.trim(),
    actor: DEFAULT_ACTOR,
    reissue: false,
  });
  closeProcess();
  reloadKey.value += 1;
}

/** 废弃并重发（二次确认后执行，同一事务语义）。 */
async function submitProcess(payload: { note: string }): Promise<void> {
  if (!processTarget.value) {
    return;
  }
  const result = await repo.processTransfer({
    ticketId: processTarget.value.id,
    prebindNew: processForm.mode === 'prebind',
    inheritTier: processForm.tier,
    validUntil: resolveValidUntil(),
    note: payload.note,
    actor: DEFAULT_ACTOR,
    reissue: true,
  });
  if (!result.ok) {
    return; // real 模式失败原因见全局横幅；保留弹窗便于调整后重试
  }
  closeProcess();
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
  width: 640px;
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
  flex-wrap: wrap;
}
.ac-modal__full {
  grid-column: 1 / -1;
}
</style>
