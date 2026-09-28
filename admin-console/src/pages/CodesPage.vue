<template>
  <!--
    CodesPage —— 激活码管理（★ 核心页，页面清单第 3 项）。
    实现要点：
      - 筛选（状态 / 租户 / tier / 关键字）+ 分页 + 导出
      - 列显示码值掩码、绑定设备指纹摘要、生效时间
      - 行内操作按状态与权限动态变化：未绑定→作废、已绑定→废弃、已废弃→重发
      - 发放弹窗（草稿隔离）、废弃弹窗（四要素）、重发弹窗（预绑定 / 留待激活）
  -->
  <PageHeader
    crumb="授权运营 / 激活码管理"
    title="激活码管理"
    desc="一机一码的生命周期唯一执行侧：发放 → 绑定 → 废弃 → 重发。码值列表默认掩码，详情页按权限揭示并记审计。"
  >
    <template #actions>
      <button type="button" class="ac-btn" @click="exportCsv">导出 CSV</button>
      <!-- 发放按钮：按操作级权限门控（无权则隐藏） -->
      <RoleGate :allowed="canIssue">
        <button type="button" class="ac-btn ac-btn--primary" @click="openIssue">发放激活码</button>
      </RoleGate>
    </template>
  </PageHeader>

  <div class="ac-content">
    <!-- 筛选栏 -->
    <div class="ac-card">
      <div class="ac-card__body">
        <div class="ac-filters">
          <div class="ac-filters__item">
            <label for="f-status">状态</label>
            <UiSelect v-model="filters.status" :options="statusOptions" />
          </div>
          <div class="ac-filters__item">
            <label for="f-tenant">租户</label>
            <UiSelect v-model="filters.tenant" :options="tenantOptions" />
          </div>
          <div class="ac-filters__item">
            <label for="f-tier">tier</label>
            <UiSelect v-model="filters.tier" :options="tierOptions" />
          </div>
          <div class="ac-filters__item ac-filters__grow">
            <label for="f-keyword">搜索（激活码 / 机器码摘要 / 客户名称 / 订单号）</label>
            <UiInput v-model="filters.keyword" placeholder="输入关键字后自动筛选" />
          </div>
        </div>
      </div>
    </div>

    <!-- 列表 -->
    <section class="ac-card">
      <div class="ac-card__head">
        <h3>激活码列表</h3>
        <span class="ac-card__sub">共 {{ paged.total }} 条 · 码值默认掩码</span>
      </div>

      <!-- 空态：给下一步动作（验收点 4） -->
      <EmptyState
        v-if="paged.total === 0"
        title="没有符合条件的激活码"
        desc="可能是筛选条件过窄。你可以清空筛选，或直接为租户签发新的激活码。"
      >
        <template #actions>
          <button type="button" class="ac-btn" @click="resetFilters">清空筛选</button>
          <button v-if="canIssue" type="button" class="ac-btn ac-btn--primary" @click="openIssue">签发激活码</button>
        </template>
      </EmptyState>

      <template v-else>
        <UiTable
          :columns="columns"
          :rows="paged.items"
          row-key-field="id"
        >
          <!-- 激活码：列表掩码 -->
          <template #cell-code="{ row }">
            <span class="ac-mono">{{ maskCode(row.code) }}</span>
          </template>
          <!-- 状态 -->
          <template #cell-status="{ row }">
            <StatusTag :status="row.status" />
          </template>
          <!-- 绑定设备 -->
          <template #cell-boundDeviceSummary="{ row }">
            <span v-if="row.boundDeviceSummary" class="ac-mono">
              {{ maskMachineSummary(row.boundDeviceSummary) }}（{{ row.boundDeviceName }}）
            </span>
            <span v-else>—</span>
          </template>
          <!-- 有效期 -->
          <template #cell-validUntil="{ row }">
            <span class="ac-mono">{{ formatDate(row.validUntil) }}</span>
          </template>
          <!-- 溯源 -->
          <template #cell-trace="{ row }">
            <span v-if="row.reissuedTo" class="ac-mono">→ {{ maskCode(codeById(row.reissuedTo)) }}</span>
            <span v-else-if="row.reissuedFrom" class="ac-mono">← {{ maskCode(codeById(row.reissuedFrom)) }}</span>
            <span v-else>—</span>
          </template>
          <!-- 行内操作：按状态与权限动态渲染 -->
          <template #actions="{ row }">
            <button type="button" class="ac-btn ac-btn--sm" @click="openDetail(row.id)">详情</button>
            <!-- 未绑定 → 作废（不影响任何在网设备） -->
            <RoleGate v-if="row.status === 'issued'" :allowed="canRevoke" mode="disable" deny-text="当前角色无「作废」权限">
              <button type="button" class="ac-btn ac-btn--sm" @click="openVoid(row)">作废</button>
            </RoleGate>
            <!-- 已绑定 → 废弃（高危，立即停止北向转发） -->
            <RoleGate
              v-else-if="row.status === 'bound'"
              :allowed="canRevoke"
              mode="disable"
              deny-text="当前角色无「废弃」权限"
            >
              <button type="button" class="ac-btn ac-btn--sm ac-btn--danger" @click="openRevoke(row)">废弃</button>
            </RoleGate>
            <!-- 已废弃 → 重发 -->
            <RoleGate
              v-else-if="row.status === 'revoked'"
              :allowed="canReissue"
              mode="disable"
              deny-text="当前角色无「重发」权限"
            >
              <button type="button" class="ac-btn ac-btn--sm" @click="openReissue(row)">重发</button>
            </RoleGate>
          </template>
        </UiTable>

        <UiPager :page="paged.page" :total="paged.total" :page-size="PAGE_SIZE" @update:page="onPage" />
      </template>
    </section>

    <!-- 三种操作的区别（客服必读） -->
    <section class="ac-card">
      <div class="ac-card__head"><h3>三种操作的区别（客服必读）</h3></div>
      <div class="ac-card__body">
        <div class="ac-list">
          <div class="ac-list__item">
            <div>
              <div class="ac-list__title">作废（仅未绑定）</div>
              <div class="ac-list__desc">仅使该码不可用，<b>不影响任何在网设备</b>。</div>
            </div>
          </div>
          <div class="ac-list__item">
            <div>
              <div class="ac-list__title">废弃（已绑定）</div>
              <div class="ac-list__desc">
                原设备将在下次心跳（≤24h）后<b>立即停止北向转发</b>，本地采集继续；<b>不可撤销</b>。
              </div>
            </div>
          </div>
          <div class="ac-list__item">
            <div>
              <div class="ac-list__title">重发</div>
              <div class="ac-list__desc">
                生成新码并建立 <span class="ac-mono">reissued_from</span> 溯源链；可预绑定新机器码，或留待客户首次激活时绑定。
              </div>
            </div>
          </div>
        </div>
      </div>
    </section>
  </div>

  <!-- ============ 发放弹窗 ============ -->
  <Teleport to="body">
    <div v-if="issueOpen" class="ac-modal-mask" @click.self="closeIssue">
      <div class="ac-modal" role="dialog" aria-modal="true" aria-label="发放激活码">
        <div class="ac-modal__head"><h3>发放激活码</h3></div>
        <div class="ac-modal__body">
          <div class="ac-grid ac-grid--2">
            <UiField
              label="租户"
              required
              :error="tenantError"
              :hint="isReal ? '下拉为 licensing-server 中的真实租户；无目标租户请先到「租户与策略」页新增' : ''"
            >
              <UiSelect v-model="issueForm.tenant" :options="issueTenantOptions" />
            </UiField>
            <UiField label="授权档位" required>
              <UiSelect v-model="issueForm.tier" :options="issueTierOptions" />
            </UiField>
            <UiField label="有效期起" required>
              <UiInput v-model="issueForm.validFrom" placeholder="YYYY-MM-DD" />
            </UiField>
            <UiField label="有效期止" required>
              <UiInput v-model="issueForm.validUntil" placeholder="YYYY-MM-DD" />
            </UiField>
            <UiField label="数量" required hint="1 – 100">
              <UiInput v-model="issueForm.count" type="number" />
            </UiField>
            <UiField label="备注">
              <UiInput v-model="issueForm.note" placeholder="合同号 / 订单号 / 说明" />
            </UiField>
            <UiField
              class="ac-modal__full"
              label="机器码"
              required
              :hint="prebindHint"
              :error="prebindError"
              full
            >
              <UiInput v-model="issueForm.prebindMachineCode" :invalid="prebindError.length > 0" placeholder="a1b2c3d4e5f6..." />
            </UiField>
          </div>
          <p class="ac-note">
            <span class="ac-note__icon">ⓘ</span>
            <span>激活码明文仅在生成后展示一次；后续查看需权限并记录审计。</span>
          </p>
        </div>
        <div class="ac-modal__foot">
          <button type="button" class="ac-btn" @click="closeIssue">取消</button>
          <button type="button" class="ac-btn ac-btn--primary" :disabled="!canSubmitIssue" @click="submitIssue">
            生成激活码
          </button>
        </div>
      </div>
    </div>
  </Teleport>

  <!-- ============ 发放结果（一次复制全部） ============ -->
  <Teleport to="body">
    <div v-if="issuedCodes.length > 0" class="ac-modal-mask" @click.self="issuedCodes = []">
      <div class="ac-modal" role="dialog" aria-modal="true" aria-label="激活码生成结果">
        <div class="ac-modal__head"><h3>已生成 {{ issuedCodes.length }} 个激活码</h3></div>
        <div class="ac-modal__body">
          <p class="ac-note">
            <span class="ac-note__icon">ⓘ</span>
            <span>明文<b>仅在此处展示一次</b>，请立即复制并交付客户；关闭后列表将只显示掩码。</span>
          </p>
          <textarea class="ac-codes" readonly :value="issuedText" rows="6" />
        </div>
        <div class="ac-modal__foot">
          <button type="button" class="ac-btn ac-btn--primary" @click="copyIssued">一次复制全部</button>
          <button type="button" class="ac-btn" @click="issuedCodes = []">我已保存，关闭</button>
        </div>
      </div>
    </div>
  </Teleport>

  <!-- ============ 作废（未绑定，低危） ============ -->
  <DangerConfirmModal
    :open="voidOpen"
    :title="`作废激活码 ${maskCode(voidTarget?.code ?? '')}`"
    :impacts="[
      '该码将不可再用于激活，本次操作不影响任何在网设备。',
      '如客户已持有该码但尚未激活，需向其补发新码。',
    ]"
    :facts="voidFacts"
    :confirm-text="'作废激活码'"
    @close="closeVoid"
    @submit="submitVoid"
  />

  <!-- ============ 废弃（★ 高危，四要素） ============ -->
  <DangerConfirmModal
    :open="revokeOpen"
    :title="`废弃激活码 ${maskCode(revokeTarget?.code ?? '')}`"
    :impacts="[
      `该码绑定设备「${revokeTarget?.boundDeviceSummary ? maskMachineSummary(revokeTarget.boundDeviceSummary) : '—'}（${revokeTarget?.boundDeviceName ?? '—'}）」将在下次心跳（≤24h）后立即停止北向转发，本地采集继续。`,
      '客户侧「授权与激活」页会显示授权停用提示，需重新发放新码才能恢复。',
      '本操作不可撤销。恢复路径：为该设备签发新激活码（或走换机重发）。',
    ]"
    :facts="revokeFacts"
    :reasons="REVOKE_REASONS"
    :confirm-value="revokeTarget?.code ?? ''"
    confirm-label="风险二次确认（请输入激活码后 8 位）"
    confirm-placeholder="去分隔符后 8 位，如 90ABCD3K"
    :confirm-text="dualApproval ? '提交废弃（含双人复核）' : '废弃激活码'"
    :require-second-approver="dualApproval"
    @close="closeRevoke"
    @submit="submitRevoke"
  />

  <!-- ============ 重发（★ 高危） ============ -->
  <Teleport to="body">
    <div v-if="reissueOpen" class="ac-modal-mask" @click.self="closeReissue">
      <div class="ac-modal" role="dialog" aria-modal="true" aria-label="重发激活码">
        <div class="ac-modal__head">
          <h3>重发激活码（源：{{ maskCode(reissueTarget?.code ?? '') }}，已废弃）</h3>
        </div>
        <div class="ac-modal__body">
          <div class="ac-reissue__source">
            <span class="ac-mono">reissued_from = {{ reissueTarget?.id ?? '—' }}</span>
            <span>· 租户 {{ reissueTarget?.tenant ?? '—' }}</span>
            <span>· 原 tier {{ reissueTarget?.tier ?? '—' }}</span>
          </div>

          <UiField label="绑定模式" required>
            <UiRadio v-model="reissueForm.mode" :options="reissueModeOptions" />
          </UiField>

          <UiField
            v-if="reissueForm.mode === 'prebind'"
            label="新机器码"
            required
            :hint="prebindHint"
            :error="prebindError"
          >
            <UiInput v-model="reissueForm.machineCode" :invalid="prebindError.length > 0" placeholder="a1b2c3d4e5f6..." />
          </UiField>

          <div class="ac-grid ac-grid--2">
            <UiField label="继承 tier（可覆盖）" required>
              <UiSelect v-model="reissueForm.tier" :options="issueTierOptions" />
            </UiField>
            <UiField label="有效期" required hint="默认顺延 12 个月">
              <UiInput v-model="reissueForm.validUntil" placeholder="YYYY-MM-DD" />
            </UiField>
            <UiField class="ac-modal__full" label="备注" full>
              <UiInput v-model="reissueForm.note" placeholder="如：客户主板损坏，已寄修" />
            </UiField>
          </div>

          <p class="ac-note">
            <span class="ac-note__icon">ⓘ</span>
            <span>
              重发<b>不自动解绑原设备</b>（原设备已随废弃失效）；新码建立
              <span class="ac-mono">reissued_from</span> 溯源链，详情页时间线可见。
            </span>
          </p>
        </div>
        <div class="ac-modal__foot">
          <button type="button" class="ac-btn" @click="closeReissue">取消</button>
          <button type="button" class="ac-btn ac-btn--primary" :disabled="!canSubmitReissue" @click="submitReissue">
            确认重发
          </button>
        </div>
      </div>
    </div>
  </Teleport>
</template>

<script setup lang="ts">
/**
 * @file CodesPage.vue
 * @module admin-console/pages/CodesPage
 * @description 激活码管理（核心页）。
 *
 * 草稿隔离约定（血泪教训）：
 *  - 发放 / 重发使用**独立草稿对象**，仅在打开弹窗时初始化、关闭时清空；
 *  - 废弃使用 `DangerConfirmModal`，该组件自带草稿隔离（测试已覆盖）。
 * 任何草稿都不得写入 `repo` 返回的记录对象。
 */
import { computed, reactive, ref, watch } from 'vue';
import { useRouter } from 'vue-router';
import {
  PageHeader,
  UiField,
  UiInput,
  UiSelect,
  UiRadio,
  UiTable,
  UiPager,
  StatusTag,
  EmptyState,
  RoleGate,
  DangerConfirmModal,
  maskCode,
  maskMachineSummary,
  isValidMachineCode,
  formatDateTime,
  formatDate,
  can,
  type SelectOption,
  type RadioOption,
  type TableColumn,
} from '@ui-kit';
import { repo, TENANT_NAMES, TIER_NAMES, REVOKE_REASONS, DEFAULT_ACTOR, API_MODE, type CodeRecord } from '../api/repo';
import { session } from '../store/session';

const router = useRouter();

/** 是否 real 模式（构建期常量；real 下发放弹窗的「租户」为后端租户 ID 直填）。 */
const isReal = API_MODE === 'real';

/** 每页条数。 */
const PAGE_SIZE = 8;

/** 操作级权限（真实门控：无权时按钮隐藏或禁用）。 */
const canIssue = computed(() => can(session.state.role, 'code.issue'));
const canRevoke = computed(() => can(session.state.role, 'code.revoke'));
const canReissue = computed(() => can(session.state.role, 'code.reissue'));

/** 双人复核开关（会话级，影响废弃弹窗是否要求第二审批人）。 */
const dualApproval = computed(() => session.state.dualApproval);

/** 筛选草稿（页面级，非持久）。 */
const filters = reactive({
  status: '',
  tenant: '',
  tier: '',
  keyword: '',
});

/** 页码。 */
const page = ref(1);

/** 筛选条件变化时回到第 1 页（避免停留在空页）。 */
watch(
  () => [filters.status, filters.tenant, filters.tier, filters.keyword],
  () => {
    page.value = 1;
  },
);

/** 查询结果（响应式：随筛选与页码变化）。 */
const paged = computed(() =>
  repo.queryCodes({
    status: filters.status,
    tenant: filters.tenant,
    tier: filters.tier,
    keyword: filters.keyword,
    page: page.value,
    pageSize: PAGE_SIZE,
  }),
);

/** 筛选下拉选项。real 模式租户选项来自 GET /admin/tenants（后端租户 ID），mock 用演示租户名。 */
const statusOptions: readonly SelectOption[] = [
  { value: '', label: '全部状态' },
  { value: 'issued', label: '已发放' },
  { value: 'bound', label: '已绑定' },
  { value: 'revoked', label: '已废弃' },
  { value: 'reissued', label: '已重发' },
];
const tenantOptions = computed<readonly SelectOption[]>(() => {
  if (!isReal) {
    return [{ value: '', label: '全部租户' }, ...TENANT_NAMES.map((t) => ({ value: t, label: t }))];
  }
  return [
    { value: '', label: '全部租户' },
    ...repo.allTenants().map((t) => ({ value: t.id, label: t.name && t.name !== t.id ? `${t.name}（${t.id}）` : t.id })),
  ];
});
const tierOptions: readonly SelectOption[] = [
  { value: '', label: '全部 tier' },
  ...TIER_NAMES.map((t) => ({ value: t, label: t })),
];
/** 发放下拉的租户选项：real 模式来自 GET /admin/tenants（真实租户），mock 用演示租户名。 */
const issueTenantOptions = computed<readonly SelectOption[]>(() => {
  if (!isReal) {
    return TENANT_NAMES.map((t) => ({ value: t, label: t }));
  }
  return repo
    .allTenants()
    .map((t) => ({ value: t.id, label: t.name && t.name !== t.id ? `${t.name}（${t.id}）` : t.id }));
});
const issueTierOptions: readonly SelectOption[] = TIER_NAMES.map((t) => ({ value: t, label: t }));

/** 列定义。 */
const columns: readonly TableColumn[] = [
  { key: 'code', label: '激活码', mono: true },
  { key: 'status', label: '状态' },
  { key: 'tenant', label: '租户' },
  { key: 'boundDeviceSummary', label: '绑定设备', mono: true },
  { key: 'tier', label: '授权档位' },
  { key: 'validUntil', label: '有效期', mono: true },
  { key: 'trace', label: '溯源', mono: true },
];

/** 按 id 查掩码码值（溯源列使用）。 */
function codeById(id: string): string {
  return repo.getCode(id)?.code ?? '';
}

/** 换页。 */
function onPage(next: number): void {
  page.value = next;
}

/** 清空筛选（空态动作）。 */
function resetFilters(): void {
  filters.status = '';
  filters.tenant = '';
  filters.tier = '';
  filters.keyword = '';
}

/** 跳详情。 */
function openDetail(id: string): void {
  void router.push({ name: 'code-detail', params: { id } });
}

/** 导出 CSV（明文列默认掩码 + 需二次确认，见审计页口径）。 */
function exportCsv(): void {
  // 导出属敏感动作：必须二次确认，且明文列保持掩码（与审计页口径一致）
  const ok = window.confirm('导出将包含全部筛选结果；明文激活码默认掩码，导出操作会记入审计。确认导出？');
  if (!ok) {
    return;
  }
  const rows = repo.queryCodes({
    status: filters.status,
    tenant: filters.tenant,
    tier: filters.tier,
    keyword: filters.keyword,
    page: 1,
    pageSize: 100000,
  }).items;
  const header = '激活码,状态,租户,tier,有效期,绑定设备摘要,创建时间';
  const body = rows
    .map((r) =>
      [maskCode(r.code), r.status, r.tenant, r.tier, formatDate(r.validUntil), r.boundDeviceSummary ? maskMachineSummary(r.boundDeviceSummary) : '-', formatDateTime(r.createdAt)].join(','),
    )
    .join('\n');
  const blob = new Blob([`\uFEFF${header}\n${body}`], { type: 'text/csv;charset=utf-8' });
  const url = URL.createObjectURL(blob);
  const a = document.createElement('a');
  a.href = url;
  a.download = `activation-codes-${Date.now()}.csv`;
  a.click();
  URL.revokeObjectURL(url);
}

// ---------------------------------------------------------------------------
// 发放：草稿 + 校验
// ---------------------------------------------------------------------------
/** 发放弹窗开关。 */
const issueOpen = ref(false);
/** 发放草稿。 */
const issueForm = reactive({
  tenant: TENANT_NAMES[0],
  tier: TIER_NAMES[0],
  validFrom: '2026-09-23',
  validUntil: '2027-09-23',
  count: '1',
  prebindMachineCode: '',
  note: '',
});
/** 已生成码（结果弹窗）。 */
const issuedCodes = ref<CodeRecord[]>([]);

/** 预绑定机器码校验错误（2026-09-27 契约：机器码必填，空即报错）。 */
const prebindError = computed(() => {
  const value = issueForm.prebindMachineCode.trim();
  if (value.length === 0) {
    return '机器码为必填项：请在客户设备上获取后填入';
  }
  return isValidMachineCode(value) ? '' : '机器码格式不正确（应为 8–64 位十六进制）';
});

/** 预绑定提示（写清语义，避免误用）。 */
const prebindHint = computed(() => {
  const value = issueForm.prebindMachineCode.trim();
  if (value.length === 0) {
    return '一机一码：该码只能在填入的这台机器上激活';
  }
  return isValidMachineCode(value) ? '格式校验通过 · 该码将只能在此机器激活' : '格式校验未通过';
});

/** 租户校验：real 模式所选租户必须存在于真实租户列表（引导先建租户）。 */
const tenantError = computed(() => {
  if (!isReal || issueForm.tenant === '') {
    return '';
  }
  return issueTenantOptions.value.some((o) => o.value === issueForm.tenant)
    ? ''
    : '租户不存在，请先在「租户与策略」页新增租户';
});

/** 发放可提交条件。 */
const canSubmitIssue = computed(() => {
  const count = Number(issueForm.count);
  return (
    issueForm.tenant.length > 0 &&
    issueForm.tier.length > 0 &&
    issueForm.validFrom.length > 0 &&
    issueForm.validUntil.length > 0 &&
    Number.isFinite(count) &&
    count >= 1 &&
    count <= 100 &&
    issueForm.validUntil >= issueForm.validFrom &&
    issueForm.prebindMachineCode.trim().length > 0 &&
    prebindError.value === '' &&
    tenantError.value === ''
  );
});

/** 打开发放弹窗：重置草稿（草稿隔离）。real 模式默认租户取会话租户（须在真实列表内），否则取首个真实租户。 */
function openIssue(): void {
  if (isReal) {
    const options = issueTenantOptions.value;
    const preferred = session.state.tenantId;
    issueForm.tenant =
      preferred && options.some((o) => o.value === preferred)
        ? preferred
        : (options[0]?.value ?? '');
  } else {
    issueForm.tenant = TENANT_NAMES[0];
  }
  issueForm.tier = TIER_NAMES[0];
  issueForm.validFrom = '2026-09-23';
  issueForm.validUntil = '2027-09-23';
  issueForm.count = '1';
  issueForm.prebindMachineCode = '';
  issueForm.note = '';
  issueOpen.value = true;
}

/** 关闭发放弹窗：清草稿。 */
function closeIssue(): void {
  issueOpen.value = false;
  issueForm.prebindMachineCode = '';
  issueForm.note = '';
}

/** 提交发放（real：等待后端确认后才展示明文——明文仅展示一次，绝不能是本地伪造值）。 */
async function submitIssue(): Promise<void> {
  if (!canSubmitIssue.value) {
    return;
  }
  const created = await repo.issueCode({
    tenant: issueForm.tenant,
    tier: issueForm.tier,
    validFrom: issueForm.validFrom,
    validUntil: issueForm.validUntil,
    count: Number(issueForm.count),
    prebindMachineCode: issueForm.prebindMachineCode.trim(),
    note: issueForm.note.trim(),
    actor: DEFAULT_ACTOR,
  });
  if (created.length === 0) {
    return; // real 模式发放失败：全局横幅已给出原因（含业务码 / trace_id），不关弹窗以便重试
  }
  issuedCodes.value = created;
  closeIssue();
  resetFilters();
}

/** 已生成码的纯文本（供一次复制全部）。 */
const issuedText = computed(() => issuedCodes.value.map((c) => c.code).join('\n'));

/** 复制全部码。 */
async function copyIssued(): Promise<void> {
  try {
    await navigator.clipboard.writeText(issuedText.value);
  } catch {
    window.prompt('请手动复制以下激活码：', issuedText.value);
  }
}

// ---------------------------------------------------------------------------
// 作废（未绑定，低危）+ 废弃（已绑定，高危）
// ---------------------------------------------------------------------------
/** 作废弹窗。 */
const voidOpen = ref(false);
/** 作废目标 id（只存 id，不持有对象引用，避免脏写）。 */
const voidTargetId = ref('');
/** 废弃弹窗。 */
const revokeOpen = ref(false);
/** 废弃目标 id。 */
const revokeTargetId = ref('');

/** 当前作废目标（打开时按 id 取快照）。 */
const voidTarget = computed<CodeRecord | null>(() => (voidTargetId.value ? repo.getCode(voidTargetId.value) : null));
/** 当前废弃目标。 */
const revokeTarget = computed<CodeRecord | null>(() => (revokeTargetId.value ? repo.getCode(revokeTargetId.value) : null));

/** 作废弹窗的对象摘要。 */
const voidFacts = computed(() => [
  { label: '租户', value: voidTarget.value?.tenant ?? '—' },
  { label: '授权档位', value: voidTarget.value?.tier ?? '—' },
  { label: '发放时间', value: formatDateTime(voidTarget.value?.createdAt) },
  { label: '影响设备', value: '无（该码尚未绑定任何设备）' },
]);

/** 废弃弹窗的对象摘要。 */
const revokeFacts = computed(() => [
  { label: '租户', value: revokeTarget.value?.tenant ?? '—' },
  {
    label: '绑定设备',
    value: revokeTarget.value?.boundDeviceSummary
      ? `${maskMachineSummary(revokeTarget.value.boundDeviceSummary)}（${revokeTarget.value.boundDeviceName}）`
      : '—',
  },
  { label: '最近心跳', value: '2026-09-23 12:25:11（6 分钟前）' },
  { label: '双人复核', value: dualApproval.value ? '已开启（需第二审批人）' : '未开启' },
]);

/** 打开作废。 */
function openVoid(row: CodeRecord): void {
  voidTargetId.value = row.id;
  voidOpen.value = true;
}
/** 关闭作废。 */
function closeVoid(): void {
  voidOpen.value = false;
  voidTargetId.value = '';
}
/** 提交作废（原因由弹窗提供，语义上等同于「误发放」类回收）。 */
async function submitVoid(payload: { note: string }): Promise<void> {
  if (!voidTargetId.value) {
    return;
  }
  await repo.revokeCode({ id: voidTargetId.value, reason: '误发放（未绑定作废）', note: payload.note, actor: DEFAULT_ACTOR });
  closeVoid();
  resetFilters();
}

/** 打开废弃。 */
function openRevoke(row: CodeRecord): void {
  revokeTargetId.value = row.id;
  revokeOpen.value = true;
}
/** 关闭废弃。 */
function closeRevoke(): void {
  revokeOpen.value = false;
  revokeTargetId.value = '';
}
/** 提交废弃（四要素已由弹窗校验通过）。 */
async function submitRevoke(payload: { reason: string; note: string; secondApprover: string }): Promise<void> {
  if (!revokeTargetId.value) {
    return;
  }
  await repo.revokeCode({
    id: revokeTargetId.value,
    reason: payload.reason,
    // 三字段独立：第二审批人走 secondApprover 字段原样下发，不得折进 note（审计可追溯前提）
    note: payload.note,
    secondApprover: payload.secondApprover,
    actor: DEFAULT_ACTOR,
  });
  closeRevoke();
  resetFilters();
}

// ---------------------------------------------------------------------------
// 重发（高危）
// ---------------------------------------------------------------------------
/** 重发弹窗开关。 */
const reissueOpen = ref(false);
/** 重发目标 id。 */
const reissueTargetId = ref('');
/** 重发草稿（独立对象）。 */
const reissueForm = reactive({
  mode: 'later' as 'prebind' | 'later',
  machineCode: '',
  tier: TIER_NAMES[0],
  validUntil: '2027-09-23',
  note: '',
});

/** 当前重发目标。 */
const reissueTarget = computed<CodeRecord | null>(() =>
  reissueTargetId.value ? repo.getCode(reissueTargetId.value) : null,
);

/** 绑定模式选项（说明后果，符合「可解释」）。 */
const reissueModeOptions: readonly RadioOption[] = [
  { value: 'prebind', label: '预绑定新机器码', desc: '该码只能在新机器上激活；粘贴机器码后校验格式与占用情况' },
  { value: 'later', label: '留待首次激活时绑定', desc: '推荐。客户在新机上自助激活，无需提前收集机器码' },
];

/** 重发预绑定机器码校验错误。 */
const reissuePrebindError = computed(() => {
  const value = reissueForm.machineCode.trim();
  if (reissueForm.mode !== 'prebind' || value.length === 0) {
    return '';
  }
  return isValidMachineCode(value) ? '' : '机器码格式不正确（应为 8–64 位十六进制）';
});

/** 重发可提交条件。 */
const canSubmitReissue = computed(() => {
  if (reissueForm.mode === 'prebind' && reissueForm.machineCode.trim().length === 0) {
    return false;
  }
  return reissuePrebindError.value === '' && reissueForm.tier.length > 0 && reissueForm.validUntil.length > 0;
});

/** 打开重发：从源码继承 tier 与有效期（草稿隔离，仅写入草稿）。 */
function openReissue(row: CodeRecord): void {
  reissueTargetId.value = row.id;
  reissueForm.mode = 'later';
  reissueForm.machineCode = '';
  reissueForm.tier = row.tier;
  const next = new Date();
  next.setFullYear(next.getFullYear() + 1);
  reissueForm.validUntil = `${next.getFullYear()}-${String(next.getMonth() + 1).padStart(2, '0')}-${String(next.getDate()).padStart(2, '0')}`;
  reissueForm.note = '';
  reissueOpen.value = true;
}

/** 关闭重发：清草稿。 */
function closeReissue(): void {
  reissueOpen.value = false;
  reissueTargetId.value = '';
  reissueForm.machineCode = '';
  reissueForm.note = '';
}

/** 提交重发（real：等待后端返回新码后才展示明文）。 */
async function submitReissue(): Promise<void> {
  if (!canSubmitReissue.value || !reissueTargetId.value) {
    return;
  }
  const created = await repo.reissueCode({
    sourceId: reissueTargetId.value,
    inheritTier: reissueForm.tier,
    inheritValidUntil: reissueForm.validUntil,
    prebindMachineCode: reissueForm.mode === 'prebind' ? reissueForm.machineCode.trim() : '',
    note: reissueForm.note.trim(),
    actor: DEFAULT_ACTOR,
  });
  closeReissue();
  resetFilters();
  if (created) {
    issuedCodes.value = [created];
  }
}
</script>

<style scoped>
/* 弹窗基础样式（本页自用，避免全局污染） */
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
}
.ac-modal__full {
  grid-column: 1 / -1;
}
.ac-codes {
  width: 100%;
  box-sizing: border-box;
  font-family: var(--font-mono);
  font-size: var(--fs-table);
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  padding: 10px;
  background: var(--bg-app);
  resize: vertical;
}
.ac-reissue__source {
  display: flex;
  gap: 10px;
  flex-wrap: wrap;
  font-size: var(--fs-caption);
  color: var(--text-2);
  background: var(--bg-app);
  border-radius: var(--radius-sm);
  padding: 8px 12px;
}
</style>
