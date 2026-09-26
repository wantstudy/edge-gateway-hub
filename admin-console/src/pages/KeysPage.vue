<template>
  <!--
    KeysPage —— 签名密钥管理（页面清单第 11 项，仅「系统」角色可见）。
    硬约束（ui-admin-console.md §3.7 / task 45）：
      轮换界面**必须把风险讲明**——
      「客户端内置的是公钥集而非单钥，轮换后旧客户端仍可验签；
        但客户端内置公钥版本升级依赖客户端发布，须先评估存量覆盖率再决定退役时间。」
  -->
  <PageHeader
    crumb="风控 / 签名密钥管理"
    title="签名密钥管理"
    desc="kid 生命周期管理。轮换不等于全网失联：客户端内置公钥集，旧客户端仍可验签；退役须先评估存量版本覆盖率。"
  >
    <template #actions>
      <RoleGate :allowed="canRotate" mode="disable" deny-text="当前角色无「密钥轮换」权限">
        <button type="button" class="ac-btn ac-btn--danger" @click="openRotate">轮换密钥</button>
      </RoleGate>
    </template>
  </PageHeader>

  <div class="ac-content">
    <!-- 关键风险横幅：必须显著 -->
    <div class="ac-banner ac-banner--danger">
      <div>
        <b>轮换风险必须讲明</b><br />
        客户端内置的是<b>公钥集</b>而非单钥，因此轮换后旧客户端仍可验签（现有租约不失效）；
        但客户端内置公钥版本升级依赖客户端发布，须先评估存量版本覆盖率再决定旧 kid 的退役时间。
        一次错误退役 = 存量设备全网失联。
      </div>
    </div>

    <!-- 当前签发密钥 -->
    <section class="ac-card">
      <div class="ac-card__head">
        <h3>当前签发密钥</h3>
        <span class="ac-card__sub">签发 + 验签</span>
      </div>
      <div class="ac-card__body">
        <template v-if="activeKey">
          <dl class="ac-kv">
            <dt>kid</dt>
            <dd class="ac-mono">{{ activeKey.kid }}</dd>
            <dt>算法</dt>
            <dd>{{ activeKey.algorithm }}</dd>
            <dt>状态</dt>
            <dd><StatusTag :status="activeKey.status" /></dd>
            <dt>启用时间</dt>
            <dd class="ac-mono">{{ activeKey.activatedAt }}</dd>
            <dt>计划退役</dt>
            <dd class="ac-mono">{{ activeKey.plannedRetireAt === '—' ? '—（未设置）' : `${activeKey.plannedRetireAt}（14 天后）` }}</dd>
            <dt>公钥指纹</dt>
            <dd class="ac-mono">{{ activeKey.fingerprint }}</dd>
          </dl>

          <!-- 存量版本覆盖率：决定能否退役旧 kid 的唯一依据 -->
          <div>
            <div style="display: flex; gap: 10px; font-size: 12px; margin-bottom: 5px">
              <span style="color: var(--text-2)">客户端内置公钥版本覆盖率</span>
              <span style="margin-left: auto; color: var(--text-3)">{{ coverageNote }}</span>
            </div>
            <div class="ac-bar">
              <div class="ac-bar__fill ac-bar__fill--ok" :style="{ width: `${coveragePercent}%` }" />
            </div>
          </div>

          <p class="ac-note">
            <span class="ac-note__icon">ⓘ</span>
            <span>
              覆盖率未达 100% 前不建议退役旧 kid。客户端公钥集可通过应用更新通道追加新公钥
              （更新包本身经代码签名），且服务端提供公钥集查询端点兜底 —— <b>换密钥无需重发所有客户端</b>。
            </span>
          </p>
        </template>
        <EmptyState v-else title="没有处于启用态的密钥" desc="当前无 active 密钥，签发将不可用。请立即轮换生成新密钥。">
          <template #actions>
            <button type="button" class="ac-btn ac-btn--danger" @click="openRotate">轮换密钥</button>
          </template>
        </EmptyState>
      </div>
    </section>

    <!-- 历史密钥 -->
    <section class="ac-card">
      <div class="ac-card__head">
        <h3>历史密钥</h3>
        <span class="ac-card__sub">退役后仅验签，不再签发</span>
      </div>
      <UiTable :columns="columns" :rows="historyKeys" row-key-field="kid">
        <template #cell-kid="{ row }">
          <span class="ac-mono">{{ row.kid }}</span>
        </template>
        <template #cell-status="{ row }">
          <StatusTag :status="row.status" />
        </template>
        <template #cell-activatedAt="{ row }">
          <span class="ac-mono">{{ row.activatedAt }}</span>
        </template>
        <template #cell-retiredAt="{ row }">
          <span class="ac-mono">{{ row.retiredAt }}</span>
        </template>
        <template #cell-fingerprint="{ row }">
          <span class="ac-mono">{{ row.fingerprint }}</span>
        </template>
        <template #actions="{ row }">
          <!-- 已退役 / 已停用的 key 无「退役」动作，直接展示占位 -->
          <template v-if="canRotate && row.status !== 'key_retired' && row.status !== 'key_disabled'">
            <button type="button" class="ac-btn ac-btn--sm" @click="retire(row.kid)">退役</button>
          </template>
          <span v-else class="ac-note">—</span>
        </template>
      </UiTable>
    </section>
  </div>

  <!--
    轮换密钥（★ 危险，四要素）。
    二次确认口径与默认 tail8 模式一致：组件比对的是 confirmValue 去分隔符后的后 8 位，
    而 nextKid 形如 kid-2026Q4，实际期望 ID2026Q4。早期这里把口径写成「输入新 kid 全名」，
    占位符示例又直接写成 kid-2026Q4，用户照抄也过不了校验，确认按钮永久禁用。
  -->
  <DangerConfirmModal
    :open="rotateOpen"
    :title="`轮换签发密钥 → ${nextKid}`"
    :impacts="[
      '新 kid 生效后只用于签发新租约；旧客户端仍可验签（客户端内置公钥集，非单钥），现有租约不失效。',
      `客户端内置公钥版本升级依赖客户端发布，须先评估存量版本覆盖率（当前 ${coveragePercent}%）再决定旧 kid 退役时间。`,
      '建议：旧 kid 先置为「已退役（仅验签）」，观察 30 天后再停用。',
    ]"
    :facts="rotateFacts"
    :reasons="ROTATE_REASONS"
    :confirm-value="nextKid"
    confirm-label="风险二次确认（请输入新 kid 去分隔符后的后 8 位）"
    confirm-placeholder="如 kid-2026Q4 → ID2026Q4"
    confirm-text="确认轮换密钥"
    @close="rotateOpen = false"
    @submit="submitRotate"
  />
</template>

<script setup lang="ts">
/**
 * @file KeysPage.vue
 * @module admin-console/pages/KeysPage
 * @description 签名密钥管理页（仅 system 角色）。
 */
import { computed, ref } from 'vue';
import { PageHeader, StatusTag, UiTable, EmptyState, RoleGate, DangerConfirmModal, can, type TableColumn } from '@ui-kit';
import { repo, DEFAULT_ACTOR } from '../api/repo';
import { session } from '../store/session';

/** 权限。 */
const canRotate = computed(() => can(session.state.role, 'key.rotate'));

/** 刷新触发器。 */
const reloadKey = ref(0);

/** 全部密钥。 */
const allKeys = computed(() => {
  void reloadKey.value;
  return repo.allKeys();
});

/** 当前启用密钥。 */
const activeKey = computed(() => allKeys.value.find((k) => k.status === 'key_active') ?? null);

/** 历史密钥。 */
const historyKeys = computed(() => allKeys.value.filter((k) => k.status !== 'key_active'));

/** 列定义。 */
const columns: readonly TableColumn[] = [
  { key: 'kid', label: '密钥编号', mono: true },
  { key: 'status', label: '状态' },
  { key: 'activatedAt', label: '启用', mono: true },
  { key: 'retiredAt', label: '退役', mono: true },
  { key: 'usage', label: '用途' },
  { key: 'fingerprint', label: '公钥指纹', mono: true },
];

/** 存量覆盖率（演示：v1.0.0 386 台 / 共 412 台）。 */
const coveragePercent = 94;
/** 覆盖率说明。 */
const coverageNote = 'v1.0.0 386 台 · v0.9.x 26 台（内置旧公钥集）';

/** 轮换可选原因。 */
const ROTATE_REASONS: readonly string[] = ['计划性周期轮换', '密钥疑似泄露', '合规要求', '算法升级'];

/** 下一个 kid（按季度递增；Q4 之后进入下一年 Q1）。 */
const nextKid = computed(() => {
  const now = new Date();
  const quarter = Math.floor(now.getMonth() / 3) + 1;
  if (quarter >= 4) {
    return `kid-${now.getFullYear() + 1}Q1`;
  }
  return `kid-${now.getFullYear()}Q${quarter + 1}`;
});

/** 轮换弹窗开关。 */
const rotateOpen = ref(false);

/** 轮换弹窗对象摘要。 */
const rotateFacts = computed(() => [
  { label: '当前 kid', value: activeKey.value?.kid ?? '—' },
  { label: '新 kid', value: nextKid.value },
  { label: '存量覆盖', value: coverageNote },
  { label: '建议', value: '旧 kid 先置「已退役（仅验签）」，观察 30 天后再停用' },
]);

/** 打开轮换弹窗。 */
function openRotate(): void {
  rotateOpen.value = true;
}

/** 提交轮换（四要素已校验）。 */
function submitRotate(): void {
  void repo.rotateKey({ newKid: nextKid.value, actor: DEFAULT_ACTOR });
  rotateOpen.value = false;
  reloadKey.value += 1;
}

/** 退役指定 kid。 */
function retire(kid: string): void {
  const ok = window.confirm(`确认退役 ${kid}？退役后该 kid 不再签发，仅保留验签宽限期。此操作将记入审计。`);
  if (!ok) {
    return;
  }
  void repo.retireKey({ kid, actor: DEFAULT_ACTOR });
  reloadKey.value += 1;
}
</script>
