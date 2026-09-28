<template>
  <!--
    TransfersPage —— 换机工单（页面清单第 8 项，仅「授权运营」可见）。
    核心业务目标（ui-admin-console.md §4.2）：**≤3 次点击完成换机**
      1) 在列表看到工单
      2) 点「处理」
      3) 在弹窗点「废弃并重发」→ 二次确认（四要素齐备，confirm = 工单编号原文）
    处理在服务端**同一事务**内完成「废弃 + 重发」，并展示 reissued_from 溯源。
    数据全部来自 `repo`（real = `GET/POST /admin/transfers*`；失败记真实原因、绝不回落 mock）。
  -->
  <PageHeader
    crumb="风控 / 换机工单"
    title="换机工单"
    desc="客服处理客户换机的唯一入口。一键执行「废弃旧码 + 重发新码」，整体 ≤3 次点击完成；两步在服务端同一事务内，接口幂等。"
  >
    <template #actions>
      <button type="button" class="ac-btn" @click="toggleOnlyPending">
        {{ onlyPending ? '显示全部工单' : '仅看待处理' }}
      </button>
      <button type="button" class="ac-btn ac-btn--primary" @click="openCreate">新建工单</button>
    </template>
  </PageHeader>

  <div class="ac-content">
    <section class="ac-card">
      <div class="ac-card__head">
        <h3>{{ onlyPending ? '待处理工单' : '全部工单' }}</h3>
        <span class="ac-card__sub">目标 ≤3 次点击完成</span>
      </div>

      <!-- 加载失败：诚实空态（给出后端返回的真实原因，绝不回落 mock） -->
      <EmptyState
        v-if="tickets.length === 0 && loadError"
        title="换机工单加载失败"
        :desc="`无法获取工单列表，真实原因：${loadError}。请确认后端服务与登录态后刷新页面重试。`"
      />

      <EmptyState
        v-else-if="tickets.length === 0"
        title="没有待处理的换机工单"
        desc="当前没有客户提交换机申请。可直接受理新工单，或到激活码列表对已绑定码执行「废弃 + 重发」。"
      >
        <template #actions>
          <button type="button" class="ac-btn" @click="onlyPending = false">显示全部工单</button>
          <button type="button" class="ac-btn" @click="openCreate">新建工单</button>
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
            <span class="ac-mono">{{ row.submittedAt }}</span>
          </template>
          <template #cell-status="{ row }">
            <StatusTag :status="row.status" />
          </template>
          <template #cell-resolution="{ row }">
            {{ row.resolution || '—' }}
          </template>
          <template #actions="{ row }">
            <template v-if="row.status === 'pending'">
              <button type="button" class="ac-btn ac-btn--sm ac-btn--primary" @click="openProcess(row)">
                处理
              </button>
              <button type="button" class="ac-btn ac-btn--sm" @click="openReject(row)">驳回</button>
            </template>
            <span v-else-if="row.status === 'processed'" class="ac-note">已处理</span>
            <span v-else class="ac-note">已驳回</span>
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
                两步在服务端**同一事务**内完成；接口幂等 —— 重复提交同一请求不会产生第二个新码。
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
              <div class="ac-list__title">驳回不改动任何码 / 设备</div>
              <div class="ac-list__desc">
                驳回仅为工单终态（`rejected`），不触及原激活码状态与设备绑定关系。
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
            <dt>租户</dt>
            <dd>{{ processTarget?.tenant ?? '—' }}</dd>
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
            <UiField
              class="ac-modal__full"
              label="处理结果说明"
              required
              hint="必填，将作为工单处理结果（resolution）随操作记入审计"
              full
            >
              <UiTextarea
                v-model="processForm.resolution"
                :rows="3"
                placeholder="例如：已废弃旧码并重发新码，客户确认收到"
              />
            </UiField>
          </div>

          <p class="ac-note">
            <span class="ac-note__icon">ⓘ</span>
            <span>点击下方按钮后将进入二次确认：需选择原因、填写补充说明（≥ 10 字）并复述**工单编号全名**。</span>
          </p>
        </div>
        <div class="ac-modal__foot">
          <button type="button" class="ac-btn" @click="closeProcess">取消</button>
          <button
            type="button"
            class="ac-btn ac-btn--danger"
            :disabled="!canSubmitProcess"
            @click="openConfirm('revoke')"
          >
            仅废弃旧码
          </button>
          <button
            type="button"
            class="ac-btn ac-btn--primary"
            :disabled="!canSubmitProcess"
            @click="openConfirm('reissue')"
          >
            废弃并重发
          </button>
        </div>
      </div>
    </div>
  </Teleport>

  <!-- 新建工单弹窗（客服代客户录入；对应补齐端点 POST /admin/transfers） -->
  <Teleport to="body">
    <div v-if="createOpen" class="ac-modal-mask" @click.self="closeCreate">
      <div class="ac-modal" role="dialog" aria-modal="true" aria-label="新建换机工单">
        <div class="ac-modal__head">
          <h3>新建换机工单</h3>
        </div>
        <div class="ac-modal__body">
          <p class="ac-note">
            <span class="ac-note__icon">ⓘ</span>
            <span>受理入口用于把客户提交换机申请登记为工单；机器码可粘贴展示态（带分隔符），提交时会自动归一到匹配态。</span>
          </p>
          <div class="ac-grid ac-grid--2">
            <UiField label="租户" required :error="createError">
              <UiSelect v-model="createForm.tenant" :options="tenantOptions" />
            </UiField>
            <UiField label="原激活码 ID" required hint="须属于所选租户（可在激活码详情查看 code_id）">
              <UiInput v-model="createForm.sourceCodeId" placeholder="例如 c-001" />
            </UiField>
            <UiField label="原机器码" hint="选填；可粘贴带分隔符的展示态">
              <UiInput v-model="createForm.oldMachineCode" placeholder="例如 A19C-4E72-18B3-F65D" />
            </UiField>
            <UiField label="新机器码" hint="选填；留空则处理时留待新机首次激活绑定">
              <UiInput v-model="createForm.newMachineCode" placeholder="例如 B83D-5F90-A2C4-1D77" />
            </UiField>
          </div>
          <UiField label="换机原因" required full>
            <UiTextarea
              v-model="createForm.reason"
              :rows="2"
              :invalid="createError.length > 0"
              placeholder="例如：主板损坏返修，客户提供新机机器码"
            />
          </UiField>
        </div>
        <div class="ac-modal__foot">
          <button type="button" class="ac-btn" @click="closeCreate">取消</button>
          <button type="button" class="ac-btn ac-btn--primary" :disabled="!canSubmitCreate" @click="submitCreate">
            受理工单
          </button>
        </div>
      </div>
    </div>
  </Teleport>

  <!-- 二次确认（处理 / 仅废弃 / 驳回 共用；confirm 恒为工单编号原文） -->
  <DangerConfirmModal
    :open="confirmOpen"
    :title="dangerTitle"
    :impacts="confirmImpacts"
    :facts="confirmFacts"
    :reasons="dangerReasons"
    :confirm-value="processTarget?.id ?? ''"
    confirm-mode="full"
    confirm-label="风险二次确认（请输入工单编号全名）"
    confirm-placeholder="输入工单编号全名"
    :confirm-text="dangerConfirmText"
    @close="confirmOpen = false"
    @submit="submitDanger"
  />
</template>

<script setup lang="ts">
/**
 * @file TransfersPage.vue
 * @module admin-console/pages/TransfersPage
 * @description 换机工单页（≤3 次点击完成换机）。
 *
 * 数据经 `repo` 统一入口：real = `GET /admin/transfers`（列表）、
 * `POST /admin/transfers`（受理，补齐端点）、`POST /admin/transfers/{id}/process`
 * （废弃 + 重发单事务）、`POST /admin/transfers/{id}/reject`（驳回，补齐端点）；
 * 列表加载失败时展示后端真实原因，**绝不回落 mock**。
 */
import { computed, reactive, ref } from 'vue';
import { useRouter } from 'vue-router';
import {
  PageHeader,
  UiField,
  UiInput,
  UiSelect,
  UiTextarea,
  UiRadio,
  UiTable,
  StatusTag,
  EmptyState,
  DangerConfirmModal,
  maskMachineCode,
  maskCode,
  type SelectOption,
  type RadioOption,
  type TableColumn,
} from '@ui-kit';
import {
  repo,
  REVOKE_REASONS,
  TIER_NAMES,
  DEFAULT_ACTOR,
  transfersLoadError,
  type TransferTicket,
} from '../api/repo';

const router = useRouter();

/** 驳回原因枚举（页面级选项，非状态文案）。 */
const REJECT_REASONS: readonly string[] = ['工单信息不实', '非硬件更换', '重复提交', '客户撤回', '其他'];

/** 刷新触发器（写操作成功后自增，驱动列表重算）。 */
const reloadKey = ref(0);

/** 是否仅看待处理。 */
const onlyPending = ref(true);

/** 工单列表（real 读缓存；reactive 缓存更新自动驱动视图）。 */
const tickets = computed(() => {
  void reloadKey.value;
  const all = repo.allTransfers();
  return onlyPending.value ? all.filter((t) => t.status === 'pending') : all;
});

/** 列表加载失败的真实原因（供诚实空态展示）。 */
const loadError = transfersLoadError;

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
const confirmOpen = ref(false);
/** 二次确认动作。 */
const confirmAction = ref<'reissue' | 'revoke' | 'reject'>('reissue');
/** 目标工单 id。 */
const processTargetId = ref('');

/** 目标工单快照。 */
const processTarget = computed<TransferTicket | null>(() =>
  processTargetId.value ? repo.allTransfers().find((t) => t.id === processTargetId.value) ?? null : null,
);

/** 原激活码原文（仅用于展示掩码）。 */
const sourceCodeRaw = computed(() => {
  const id = processTarget.value?.sourceCodeId;
  return id ? repo.getCode(id)?.code ?? '' : '';
});

/** 原激活码掩码显示。 */
const sourceCodeMasked = computed(() => (sourceCodeRaw.value ? maskCode(sourceCodeRaw.value) : '—'));

/** 处理草稿（独立对象）。 */
const processForm = reactive({
  mode: 'prebind' as 'prebind' | 'later',
  tier: TIER_NAMES[0],
  validity: '12m',
  resolution: '',
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

/** 可提交（结果说明非空 + 预绑定模式需有机器码）。 */
const canSubmitProcess = computed(() => {
  if (processForm.resolution.trim().length === 0) {
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
  processForm.tier = TIER_NAMES[0];
  processForm.validity = '12m';
  processForm.resolution = '';
  processOpen.value = true;
}

/** 打开驳回：复用同一二次确认（动作 = reject）。 */
function openReject(row: TransferTicket): void {
  processTargetId.value = row.id;
  processForm.resolution = '';
  confirmAction.value = 'reject';
  confirmOpen.value = true;
}

/** 打开二次确认（处理 / 仅废弃）。 */
function openConfirm(action: 'reissue' | 'revoke'): void {
  confirmAction.value = action;
  confirmOpen.value = true;
}

/** 关闭：清草稿。 */
function closeProcess(): void {
  processOpen.value = false;
  confirmOpen.value = false;
  processTargetId.value = '';
  processForm.resolution = '';
}

/** 二次确认对象摘要。 */
const confirmFacts = computed(() => {
  const target = processTarget.value;
  const facts = [
    { label: '工单', value: target?.id ?? '—' },
    { label: '租户', value: target?.tenant ?? '—' },
    { label: '原码', value: sourceCodeMasked.value },
  ];
  if (confirmAction.value === 'reject') {
    facts.push({ label: '操作', value: '驳回工单（不改动码 / 设备）' });
  } else {
    facts.push({
      label: '新机器码',
      value: target?.newMachineCode ? maskMachineCode(target.newMachineCode) : '（留待激活）',
    });
  }
  return facts;
});

/** 二次确认影响清单（动作相关）。 */
const confirmImpacts = computed<readonly string[]>(() => {
  const target = processTarget.value;
  if (confirmAction.value === 'reject') {
    return [
      `工单 ${target?.id ?? ''} 将被驳回（终态，不可再处理）。`,
      '原激活码状态与设备绑定关系**保持不变**（驳回不触及任何码 / 设备）。',
      '驳回说明与原因、补充说明将一并写入审计日志。',
    ];
  }
  const lines = [
    `旧码 ${sourceCodeMasked.value} 立即废弃；设备 ${target?.oldMachineSummary ?? '—'} 将在下次心跳后停止北向转发（本地采集继续）。`,
  ];
  if (confirmAction.value === 'reissue') {
    lines.push(
      processForm.mode === 'prebind'
        ? `生成新码并预绑定机器码 ${maskMachineCode(target?.newMachineCode ?? '')}，建立 reissued_from 溯源链。`
        : '生成新码，留待客户在新机首次激活时绑定，建立 reissued_from 溯源链。',
    );
    lines.push('「废弃 + 重发」在服务端**同一事务**内完成，接口幂等：重复提交不会产生第二个结果。');
  } else {
    lines.push('仅废弃旧码，**不重发**新码（客户将获得新码后自行激活）。');
    lines.push('废弃在服务端单事务内完成；重复提交幂等。');
  }
  return lines;
});

/** 二次确认标题。 */
const dangerTitle = computed(() => {
  const id = processTarget.value?.id ?? '';
  if (confirmAction.value === 'reject') {
    return `确认驳回（工单 ${id}）`;
  }
  return confirmAction.value === 'reissue' ? `确认废弃并重发（工单 ${id}）` : `确认仅废弃旧码（工单 ${id}）`;
});

/** 二次确认按钮文案（动词短语）。 */
const dangerConfirmText = computed(() => {
  if (confirmAction.value === 'reject') {
    return '确认驳回工单';
  }
  return confirmAction.value === 'reissue' ? '确认废弃并重发' : '确认仅废弃旧码';
});

/** 二次确认原因枚举。 */
const dangerReasons = computed<readonly string[]>(() =>
  confirmAction.value === 'reject' ? REJECT_REASONS : REVOKE_REASONS,
);

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

/** 组装工单处理结果说明（resolution；worker 不允许空）。 */
function buildResolution(reissue: boolean): string {
  if (!reissue) {
    return '仅废弃旧码（客户将获得新码后自行激活）';
  }
  const mode = processForm.mode === 'prebind' ? '预绑定新机器码' : '留待首次激活绑定';
  return `已废弃旧码并重发新码（${mode}，tier ${processForm.tier}，有效期至 ${resolveValidUntil()}）`;
}

/** 二次确认提交：按动作分派到处理 / 仅废弃 / 驳回（四要素均为独立字段）。 */
async function submitDanger(payload: { reason: string; note: string; confirm: string }): Promise<void> {
  const target = processTarget.value;
  if (!target) {
    return;
  }
  if (confirmAction.value === 'reject') {
    const rejected = await repo.rejectTransfer({
      ticketId: target.id,
      tenant: target.tenant,
      reason: payload.reason,
      note: payload.note,
      confirm: payload.confirm,
      // 驳回说明即补充说明（两字段在数据层仍独立下发）
      resolution: payload.note,
      actor: DEFAULT_ACTOR,
    });
    if (!rejected) {
      return; // real 模式失败原因见全局横幅；保留弹窗便于调整后重试
    }
    closeProcess();
    reloadKey.value += 1;
    return;
  }

  const reissue = confirmAction.value === 'reissue';
  const result = await repo.processTransfer({
    ticketId: target.id,
    tenant: target.tenant,
    reason: payload.reason,
    note: payload.note,
    confirm: payload.confirm,
    resolution: buildResolution(reissue),
    prebindNew: reissue && processForm.mode === 'prebind',
    inheritTier: processForm.tier,
    validUntil: resolveValidUntil(),
    actor: DEFAULT_ACTOR,
    reissue,
  });
  if (!result.ok) {
    return; // real 模式失败原因见全局横幅；保留弹窗便于调整后重试
  }
  closeProcess();
  reloadKey.value += 1;
}

// ---------------- 新建工单弹窗 ----------------
/** 新建弹窗开关。 */
const createOpen = ref(false);

/** 新建草稿（独立对象，绝不写回真实数据）。 */
const createForm = reactive({
  tenant: '',
  sourceCodeId: '',
  oldMachineCode: '',
  newMachineCode: '',
  reason: '',
});

/** 租户下拉（值 = 租户 id，与真实端点 tenant_id 同口径；mock 内部映射为名称展示）。 */
const tenantOptions = computed<readonly SelectOption[]>(() =>
  repo.allTenants().map((t) => ({ value: t.id, label: `${t.name}（${t.id}）` })),
);

/** 新建表单错误（首个缺失项）。 */
const createError = computed(() => (createForm.tenant.trim() === '' ? '请选择租户' : ''));

/** 可受理（租户 + 原激活码 ID + 原因均非空）。 */
const canSubmitCreate = computed(
  () =>
    createForm.tenant.trim() !== '' &&
    createForm.sourceCodeId.trim() !== '' &&
    createForm.reason.trim() !== '',
);

/** 打开新建弹窗。 */
function openCreate(): void {
  createForm.tenant = tenantOptions.value[0]?.value ?? '';
  createForm.sourceCodeId = '';
  createForm.oldMachineCode = '';
  createForm.newMachineCode = '';
  createForm.reason = '';
  createOpen.value = true;
}

/** 关闭新建弹窗。 */
function closeCreate(): void {
  createOpen.value = false;
}

/** 提交受理（real 等待后端确认后才关弹窗刷新）。 */
async function submitCreate(): Promise<void> {
  if (!canSubmitCreate.value) {
    return;
  }
  const created = await repo.createTransfer({
    tenant: createForm.tenant,
    sourceCodeId: createForm.sourceCodeId,
    oldMachineCode: createForm.oldMachineCode,
    newMachineCode: createForm.newMachineCode,
    reason: createForm.reason,
    actor: DEFAULT_ACTOR,
  });
  if (!created) {
    return; // 失败原因见全局横幅
  }
  closeCreate();
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
