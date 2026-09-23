<template>
  <!--
    CodeDetailPage —— 激活码详情（页面清单第 4 项）。
    实现要点：
      - 基本信息（码值默认掩码 + 按权限揭示，揭示动作落审计）
      - 生命周期时间线（发放 → 绑定 → 废弃 → 重发，含操作者/时间/原因）
      - reissued_chain 溯源链
      - 绑定设备信息与回执连续性摘要
  -->
  <PageHeader
    :crumb="`授权运营 / 激活码管理 / ${maskCode(code?.code ?? '')}`"
    title="激活码详情"
    desc="单个激活码的完整生命周期与溯源：它是谁、什么时候发的、绑到哪台机器、谁操作的、为什么废弃。"
  >
    <template #actions>
      <button type="button" class="ac-btn" @click="goBack">返回列表</button>
      <RoleGate :allowed="canRevoke && code?.status === 'bound'" mode="disable" deny-text="当前角色或状态不允许废弃">
        <button type="button" class="ac-btn ac-btn--danger" @click="revokeOpen = true">废弃激活码</button>
      </RoleGate>
    </template>
  </PageHeader>

  <div class="ac-content">
    <!-- 找不到：给回升路径 -->
    <EmptyState
      v-if="!code"
      title="激活码不存在或已被清理"
      desc="可能该链接已失效。请返回列表重新检索，或通过关键字搜索激活码。"
    >
      <template #actions>
        <button type="button" class="ac-btn ac-btn--primary" @click="goBack">返回激活码列表</button>
      </template>
    </EmptyState>

    <template v-else>
      <div class="ac-grid ac-grid--1-2">
        <!-- 基本信息 -->
        <section class="ac-card">
          <div class="ac-card__head"><h3>基本信息</h3></div>
          <div class="ac-card__body">
            <dl class="ac-kv">
              <dt>激活码</dt>
              <dd>
                <!-- 真实 reveal 交互：列表掩码，此处按权限揭示并记审计 -->
                <MaskedCode
                  :code="code.code"
                  :can-reveal="canReveal"
                  @reveal="onReveal"
                />
              </dd>
              <dt>状态</dt>
              <dd><StatusTag :status="code.status" /></dd>
              <dt>租户</dt>
              <dd>{{ code.tenant }}</dd>
              <dt>tier</dt>
              <dd>{{ code.tier }}</dd>
              <dt>有效期</dt>
              <dd class="ac-mono">{{ code.validFrom }} → {{ code.validUntil }}</dd>
              <dt>来源订单</dt>
              <dd class="ac-mono">{{ code.orderId }}</dd>
              <dt>预绑定机器码</dt>
              <dd class="ac-mono">{{ code.prebindMachineCode ?? '—（留待首次激活绑定）' }}</dd>
              <dt>明文查看</dt>
              <dd>
                <span v-if="!canReveal">当前角色无「查看明文」权限（需授权运营 / 系统角色）</span>
                <span v-else-if="revealed">已揭示（本次已记入审计日志）</span>
                <span v-else>已掩码 · 点击「显示明文」需权限且记审计</span>
              </dd>
            </dl>
          </div>
        </section>

        <!-- 生命周期时间线 -->
        <section class="ac-card">
          <div class="ac-card__head">
            <h3>生命周期时间线</h3>
            <span class="ac-card__sub">发放 → 绑定 → 废弃 → 重发</span>
          </div>
          <div class="ac-card__body">
            <CodeLifecycleTimeline :nodes="timelineNodes" />
          </div>
        </section>
      </div>

      <!-- 溯源链 -->
      <section class="ac-card">
        <div class="ac-card__head">
          <h3>溯源链 reissued_chain</h3>
          <span class="ac-card__sub">换机重发形成的码链</span>
        </div>
        <div class="ac-card__body">
          <div v-if="chain.length > 1" class="ac-chain">
            <template v-for="(node, i) in chain" :key="node.id">
              <button
                type="button"
                class="ac-chain__node"
                :class="{ 'is-current': node.id === code.id }"
                @click="goToCode(node.id)"
              >
                <span class="ac-mono">{{ maskCode(node.code) }}</span>
                <StatusTag :status="node.status" />
              </button>
              <span v-if="i < chain.length - 1" class="ac-chain__arrow">⟶ 重发于</span>
            </template>
          </div>
          <EmptyState
            v-else
            title="该激活码暂无重发记录"
            desc="溯源链在下一次「废弃 + 重发」后产生；如需换机，请到换机工单或列表页执行重发。"
          />
        </div>
      </section>

      <!-- 绑定设备信息 -->
      <section class="ac-card">
        <div class="ac-card__head"><h3>绑定设备</h3></div>
        <div class="ac-card__body">
          <template v-if="boundDevice">
            <dl class="ac-kv">
              <dt>机器码摘要</dt>
              <dd class="ac-mono">{{ maskMachineSummary(boundDevice.machineSummary) }}（sha256 前 16 位）</dd>
              <dt>锚点来源</dt>
              <dd>宿主板 UUID · 宿主物理网卡 MAC · 磁盘序列号（HMAC 已签名）</dd>
              <dt>部署形态</dt>
              <dd>{{ boundDevice.deployMode }}</dd>
              <dt>客户端版本</dt>
              <dd class="ac-mono">{{ boundDevice.clientVersion }}</dd>
              <dt>校验档位</dt>
              <dd>{{ boundDevice.grade }}</dd>
              <dt>最近心跳</dt>
              <dd class="ac-mono">{{ formatDateTime(boundDevice.lastHeartbeatAt) }}</dd>
              <dt>回执健康</dt>
              <dd><StatusTag :status="boundDevice.receiptStatus" :text="code.receiptContinuity" /></dd>
              <dt>首次激活</dt>
              <dd class="ac-mono">{{ formatDateTime(boundDevice.firstActivatedAt) }}</dd>
            </dl>
            <div class="ac-list">
              <div class="ac-list__item">
                <div>
                  <div class="ac-list__title">设备详情入口</div>
                  <div class="ac-list__desc">查看心跳时序与回执连续性覆盖图</div>
                </div>
                <div class="ac-list__ops">
                  <button type="button" class="ac-btn ac-btn--sm" @click="goToDevice(boundDevice.id)">查看设备</button>
                </div>
              </div>
            </div>
          </template>
          <EmptyState
            v-else
            title="尚未绑定设备"
            desc="该码处于「已发放」状态，等待客户在网关授权页输入激活码完成绑定。"
          >
            <template #actions>
              <button type="button" class="ac-btn" @click="goBack">返回列表</button>
            </template>
          </EmptyState>
        </div>
      </section>

      <p class="ac-note">
        <span class="ac-note__icon">ⓘ</span>
        <span>
          废弃与重发在同一事务内执行，接口幂等：重复提交同一请求不会产生第二个结果。
          前端不持有签名私钥、不直连授权数据库，所有写操作均走管理 API。
        </span>
      </p>
    </template>
  </div>

  <!-- 废弃（★ 高危，四要素）：与列表页一致 -->
  <DangerConfirmModal
    :open="revokeOpen"
    :title="`废弃激活码 ${maskCode(code?.code ?? '')}`"
    :impacts="[
      '该码绑定设备将在下次心跳（≤24h）后立即停止北向转发，本地采集继续。',
      '客户侧会显示授权停用提示，需重新发放新码才能恢复。',
      '本操作不可撤销。恢复路径：为该设备签发新激活码（或走换机重发）。',
    ]"
    :facts="revokeFacts"
    :reasons="REVOKE_REASONS"
    :confirm-value="code?.code ?? ''"
    confirm-label="风险二次确认（请输入激活码后 8 位）"
    confirm-placeholder="去分隔符后 8 位"
    confirm-text="废弃激活码"
    :require-second-approver="dualApproval"
    @close="revokeOpen = false"
    @submit="submitRevoke"
  />
</template>

<script setup lang="ts">
/**
 * @file CodeDetailPage.vue
 * @module admin-console/pages/CodeDetailPage
 * @description 激活码详情页。
 */
import { computed, ref } from 'vue';
import { useRoute, useRouter } from 'vue-router';
import {
  PageHeader,
  StatusTag,
  EmptyState,
  RoleGate,
  MaskedCode,
  CodeLifecycleTimeline,
  DangerConfirmModal,
  maskCode,
  maskMachineSummary,
  formatDateTime,
  can,
  type TimelineNode,
} from '@ui-kit';
import { repo, REVOKE_REASONS, DEFAULT_ACTOR } from '../mock/mock-data';
import { session } from '../store/session';

const route = useRoute();
const router = useRouter();

/** 当前激活码 id。 */
const codeId = computed(() => String(route.params.id ?? ''));

/** 详情数据（响应式快照：写操作后 `reload` 会重新取）。 */
const reloadKey = ref(0);
const code = computed(() => {
  void reloadKey.value;
  return repo.getCode(codeId.value);
});

/** 权限。 */
const canReveal = computed(() => can(session.state.role, 'code.reveal'));
const canRevoke = computed(() => can(session.state.role, 'code.revoke'));
const dualApproval = computed(() => session.state.dualApproval);

/** 是否已揭示明文（用于展示审计提示）。 */
const revealed = ref(false);

/** 揭示回调：落审计（真实系统为服务端记录，此处前端显式留痕）。 */
function onReveal(): void {
  revealed.value = true;
  repo.logReveal({ entityId: codeId.value, actor: DEFAULT_ACTOR });
}

/** 生命周期节点 → 时间线组件所需结构。 */
const timelineNodes = computed<TimelineNode[]>(
  () =>
    code.value?.timeline.map((n) => ({
      time: n.time,
      action: n.action,
      tone: n.tone,
      target: n.target,
      operator: n.operator,
      reason: n.reason,
      detail: n.detail,
    })) ?? [],
);

/** 绑定设备。 */
const boundDevice = computed(() => {
  const summary = code.value?.boundDeviceSummary;
  if (!summary) {
    return null;
  }
  return repo.allDevices().find((d) => d.machineSummary === summary) ?? null;
});

/**
 * 溯源链：从最源头码走到最末端码。
 * 先沿 reissuedFrom 上溯到根，再沿 reissuedTo 下溯到叶。
 */
const chain = computed(() => {
  const current = code.value;
  if (!current) {
    return [];
  }
  // 上溯到根
  let root = current;
  const guard = new Set<string>();
  while (root.reissuedFrom && !guard.has(root.id)) {
    guard.add(root.id);
    const parent = repo.getCode(root.reissuedFrom);
    if (!parent) {
      break;
    }
    root = parent;
  }
  // 从根下溯
  const out: { id: string; code: string; status: string }[] = [];
  let cursor: ReturnType<typeof repo.getCode> = root;
  const seen = new Set<string>();
  while (cursor && !seen.has(cursor.id)) {
    seen.add(cursor.id);
    out.push({ id: cursor.id, code: cursor.code, status: cursor.status });
    cursor = cursor.reissuedTo ? repo.getCode(cursor.reissuedTo) : null;
  }
  return out;
});

/** 废弃弹窗的对象摘要。 */
const revokeFacts = computed(() => [
  { label: '租户', value: code.value?.tenant ?? '—' },
  {
    label: '绑定设备',
    value: boundDevice.value
      ? `${maskMachineSummary(boundDevice.value.machineSummary)}（${boundDevice.value.name}，${boundDevice.value.deployMode}，档位 ${boundDevice.value.grade}）`
      : '—',
  },
  { label: '最近心跳', value: formatDateTime(boundDevice.value?.lastHeartbeatAt) },
  { label: '双人复核', value: dualApproval.value ? '已开启（需第二审批人）' : '未开启' },
]);

/** 废弃弹窗开关。 */
const revokeOpen = ref(false);

/** 提交废弃：立即失效 + 落审计 + 刷新。 */
function submitRevoke(payload: { reason: string; note: string; secondApprover: string }): void {
  repo.revokeCode({
    id: codeId.value,
    reason: payload.reason,
    note: payload.secondApprover ? `${payload.note}（第二审批人：${payload.secondApprover}）` : payload.note,
    actor: DEFAULT_ACTOR,
  });
  revokeOpen.value = false;
  reloadKey.value += 1;
}

/** 返回列表。 */
function goBack(): void {
  void router.push({ name: 'codes' });
}

/** 跳转到链上其他码。 */
function goToCode(id: string): void {
  if (id === codeId.value) {
    return;
  }
  void router.push({ name: 'code-detail', params: { id } });
}

/** 跳设备详情。 */
function goToDevice(id: string): void {
  void router.push({ name: 'device-detail', params: { id } });
}
</script>

<style scoped>
.ac-chain {
  display: flex;
  align-items: center;
  gap: 10px;
  flex-wrap: wrap;
}
.ac-chain__node {
  display: inline-flex;
  align-items: center;
  gap: 8px;
  font-family: var(--font-mono);
  font-size: var(--fs-table);
  padding: 6px 10px;
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  background: #fff;
  cursor: pointer;
}
.ac-chain__node:hover {
  border-color: var(--brand);
}
.ac-chain__node.is-current {
  border-color: var(--brand);
  background: var(--brand-subtle);
}
.ac-chain__arrow {
  font-size: var(--fs-caption);
  color: var(--text-3);
}
</style>
