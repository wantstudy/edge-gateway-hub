<template>
  <!--
    DeviceDetailPage —— 设备详情（页面清单第 9 项）。
    重点（ui-admin-console.md §3.4）：
      - 心跳时序图（近 7 天，缺失点标红）
      - 回执连续性（B 档核心证据）：近 24h 序号区间覆盖图，跳空区间标红
      - 关联激活码与溯源链
      - 操作：废弃关联激活码（同危险确认）、标记设备异常（仅备注，不改授权）
    ⚠️ 本页**没有**「判定破解 / 一键封禁」动作（回执异常 ≠ 破解）。
  -->
  <PageHeader
    :crumb="`授权运营 / 设备管理 / ${device ? maskMachineSummary(device.machineSummary) : ''}`"
    title="设备详情"
    desc="设备授权证据与回执连续性。B 档下厂商不接收业务数据，回执是唯一在线证据；异常 ≠ 破解。"
  >
    <template #actions>
      <button type="button" class="ac-btn" @click="goBack">返回列表</button>
      <RoleGate :allowed="canMark" mode="disable" deny-text="当前角色无「标记异常」权限">
        <button type="button" class="ac-btn" @click="openMark">标记异常（仅备注）</button>
      </RoleGate>
      <RoleGate
        :allowed="canRevoke && device?.boundCodeId !== null"
        mode="disable"
        deny-text="当前角色无「废弃」权限，或该设备无关联激活码"
      >
        <button type="button" class="ac-btn ac-btn--danger" @click="revokeOpen = true">废弃关联激活码</button>
      </RoleGate>
    </template>
  </PageHeader>

  <div class="ac-content">
    <EmptyState
      v-if="!device"
      title="设备不存在"
      desc="可能该设备已被移除，或链接已失效。请返回列表重新检索。"
    >
      <template #actions>
        <button type="button" class="ac-btn ac-btn--primary" @click="goBack">返回设备列表</button>
      </template>
    </EmptyState>

    <template v-else>
      <div class="ac-grid ac-grid--1-2">
        <!-- 设备信息 -->
        <section class="ac-card">
          <div class="ac-card__head"><h3>设备信息</h3></div>
          <div class="ac-card__body">
            <dl class="ac-kv">
              <dt>机器码摘要</dt>
              <dd class="ac-mono">{{ maskMachineSummary(device.machineSummary) }}</dd>
              <dt>机器码（掩码）</dt>
              <dd><MachineCodeDisplay :code="device.machineCode" masked :anchors="anchors" /></dd>
              <dt>租户</dt>
              <dd>{{ device.tenant }}</dd>
              <dt>设备名</dt>
              <dd>{{ device.name }}</dd>
              <dt>tier</dt>
              <dd>{{ device.tier }}</dd>
              <dt>校验档位</dt>
              <dd>
                <StatusTag
                  :status="device.grade === 'A' ? 'ok' : device.grade === 'B' ? 'info' : 'unknown'"
                  :text="`档位 ${device.grade}`"
                />
              </dd>
              <dt>部署形态</dt>
              <dd><StatusTag :status="device.deployMode" /></dd>
              <dt>镜像 digest</dt>
              <dd class="ac-mono">{{ device.imageDigest ?? '—（native 部署）' }}</dd>
              <dt>关联激活码</dt>
              <dd>
                <button v-if="device.boundCodeId" type="button" class="ac-link ac-mono" @click="goToCode(device.boundCodeId)">
                  {{ device.boundCodeMasked }}
                </button>
                <span v-else>—</span>
              </dd>
              <dt>授权状态</dt>
              <dd><StatusTag :status="device.licenseStatus" /></dd>
              <dt>首次激活</dt>
              <dd class="ac-mono">{{ formatDateTime(device.firstActivatedAt) }}</dd>
              <dt>人工备注</dt>
              <dd>{{ device.anomalyNote || '—' }}</dd>
            </dl>
          </div>
        </section>

        <!-- 回执连续性 -->
        <section class="ac-card">
          <div class="ac-card__head">
            <h3>回执连续性（近 24 小时）</h3>
            <span class="ac-card__sub">B 档核心证据</span>
          </div>
          <div class="ac-card__body">
            <div class="ac-coverage">
              <div
                v-for="cell in coverage"
                :key="cell.hour"
                class="ac-coverage__cell"
                :class="{ 'ac-coverage__cell--gap': cell.kind === 'gap', 'ac-coverage__cell--miss': cell.kind === 'missing' }"
                :title="`第 ${cell.hour} 小时：${cell.kind === 'gap' ? '序号跳空' : cell.kind === 'missing' ? '未收到回执' : '回执正常'}`"
              />
            </div>
            <div class="ac-legend">
              <span><i style="background: #e8ffea; border: 1px solid #b5f0ce" />回执正常</span>
              <span><i style="background: #ffece8; border: 1px solid #ffcdc5" />序号跳空</span>
              <span><i style="background: #f2f3f5; border: 1px solid var(--border)" />缺失</span>
            </div>
            <dl class="ac-kv">
              <dt>最新区间号</dt>
              <dd class="ac-mono">#1216</dd>
              <dt>近 24h 条数</dt>
              <dd class="ac-mono">1,796,400</dd>
              <dt>摘要哈希</dt>
              <dd class="ac-mono">b7c1e9…44a0</dd>
              <dt>回执延迟</dt>
              <dd>平均 42s（允许延迟补报）</dd>
            </dl>
            <p class="ac-note">
              <span class="ac-note__icon">ⓘ</span>
              <span>
                每格为 1 小时的序号区间覆盖。<b>回执异常不等于破解</b>，
                须先核实是否检修 / 断电 / 更换硬件，再判定处置（运维手册第 4 章）。
              </span>
            </p>
          </div>
        </section>
      </div>

      <!-- 心跳时序 -->
      <section class="ac-card">
        <div class="ac-card__head">
          <h3>心跳时序（近 7 天）</h3>
          <span class="ac-card__sub">缺失点标红</span>
        </div>
        <div class="ac-card__body">
          <BarChart :labels="hbLabels" :series="hbSeries" :height="150" />
          <p class="ac-note">
            <span class="ac-note__icon">ⓘ</span>
            <span>09-21 缺失一次心跳：客户端在 7 天离线宽限期内仍正常转发（设计如此，属宽限而非故障）。</span>
          </p>
        </div>
      </section>

      <!-- 溯源链 -->
      <section class="ac-card">
        <div class="ac-card__head"><h3>关联激活码与溯源链</h3></div>
        <div class="ac-card__body">
          <div v-if="chain.length > 0" class="ac-chain">
            <template v-for="(node, i) in chain" :key="node.id">
              <button type="button" class="ac-chain__node" @click="goToCode(node.id)">
                <span class="ac-mono">{{ maskCode(node.code) }}</span>
                <StatusTag :status="node.status" />
              </button>
              <span v-if="i < chain.length - 1" class="ac-chain__arrow">⟶ 重发于</span>
            </template>
          </div>
          <EmptyState v-else title="无关联激活码" desc="该设备当前没有绑定激活码（可能仅处于心跳保活状态）。" />
        </div>
      </section>
    </template>
  </div>

  <!-- 标记异常（仅备注，不改授权） -->
  <Teleport to="body">
    <div v-if="markOpen" class="ac-modal-mask" @click.self="closeMark">
      <div class="ac-modal" role="dialog" aria-modal="true" aria-label="标记设备异常">
        <div class="ac-modal__head"><h3>标记设备异常（仅备注）</h3></div>
        <div class="ac-modal__body">
          <div class="ac-banner ac-banner--info">
            <div>
              本操作<b>只记录人工备注，不改变任何授权判定</b>。回执异常 ≠ 破解，
              请先联系客户核实（检修 / 断电 / 更换硬件）后再决定后续动作。
            </div>
          </div>
          <UiField label="备注" required :error="markError">
            <UiTextarea v-model="markNote" :rows="3" :min-length="10" show-counter :invalid="markError.length > 0" placeholder="必填，≥10 字，例如：客户反馈 09-21 车间检修断电" />
          </UiField>
        </div>
        <div class="ac-modal__foot">
          <button type="button" class="ac-btn" @click="closeMark">取消</button>
          <button type="button" class="ac-btn ac-btn--primary" :disabled="markNote.trim().length < 10" @click="submitMark">
            保存备注
          </button>
        </div>
      </div>
    </div>
  </Teleport>

  <!-- 废弃关联激活码（★ 高危，四要素） -->
  <DangerConfirmModal
    :open="revokeOpen"
    :title="`废弃关联激活码 ${device?.boundCodeMasked ?? ''}`"
    :impacts="[
      '该设备将在下次心跳（≤24h）后立即停止北向转发，本地采集继续。',
      '客户侧会显示授权停用提示，需重新发放新码才能恢复。',
      '本操作不可撤销。恢复路径：为该设备签发新激活码（或走换机重发）。',
    ]"
    :facts="revokeFacts"
    :reasons="REVOKE_REASONS"
    :confirm-value="boundCodeRaw"
    confirm-label="风险二次确认（请输入激活码后 8 位）"
    confirm-placeholder="去分隔符后 8 位"
    confirm-text="废弃关联激活码"
    :require-second-approver="dualApproval"
    @close="revokeOpen = false"
    @submit="submitRevoke"
  />
</template>

<script setup lang="ts">
/**
 * @file DeviceDetailPage.vue
 * @module admin-console/pages/DeviceDetailPage
 * @description 设备详情页。
 */
import { computed, ref } from 'vue';
import { useRoute, useRouter } from 'vue-router';
import {
  PageHeader,
  StatusTag,
  EmptyState,
  RoleGate,
  MachineCodeDisplay,
  UiField,
  UiTextarea,
  DangerConfirmModal,
  maskCode,
  maskMachineSummary,
  formatDateTime,
  can,
} from '@ui-kit';
import BarChart, { type BarSeries } from '../components/BarChart.vue';
import { repo, REVOKE_REASONS, DEFAULT_ACTOR } from '../api/repo';
import { session } from '../store/session';

const route = useRoute();
const router = useRouter();

/** 设备 id。 */
const deviceId = computed(() => String(route.params.id ?? ''));

/** 刷新触发器。 */
const reloadKey = ref(0);
const device = computed(() => {
  void reloadKey.value;
  return repo.getDevice(deviceId.value);
});

/** 权限。 */
const canMark = computed(() => can(session.state.role, 'device.mark_anomaly'));
const canRevoke = computed(() => can(session.state.role, 'code.revoke'));
const dualApproval = computed(() => session.state.dualApproval);

/** 机器码锚点来源。 */
const anchors: readonly string[] = ['宿主板 UUID（HMAC-SHA256 已签名）', '宿主物理网卡 MAC', '磁盘序列号'];

/** 回执覆盖图（24 格；本设备有 2 处跳空 + 1 处缺失）。 */
const coverage = computed(() =>
  Array.from({ length: 24 }, (_, i) => {
    const hour = i + 1;
    const kind: 'ok' | 'gap' | 'missing' = hour === 9 || hour === 10 ? 'gap' : hour === 23 ? 'missing' : 'ok';
    return { hour, kind };
  }),
);

/** 心跳时序标签。 */
const hbLabels: readonly string[] = ['09-17', '09-18', '09-19', '09-20', '09-21', '09-22', '09-23'];

/** 心跳序列（09-21 缺失点标红）。 */
const hbSeries: BarSeries[] = [
  {
    name: '心跳次数',
    color: '#1F6FEB',
    data: [1, 1, 1, 1, 0, 1, 1],
    missFlag: [false, false, false, false, true, false, false],
  },
];

/** 关联激活码原文（用于废弃确认的后 8 位校验）。 */
const boundCodeRaw = computed(() => {
  const id = device.value?.boundCodeId;
  return id ? repo.getCode(id)?.code ?? '' : '';
});

/** 溯源链（基于绑定码）。 */
const chain = computed(() => {
  const id = device.value?.boundCodeId;
  if (!id) {
    return [];
  }
  const current = repo.getCode(id);
  if (!current) {
    return [];
  }
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

// ---------------- 标记异常 ----------------
/** 标记弹窗开关。 */
const markOpen = ref(false);
/** 备注草稿。 */
const markNote = ref('');

/** 备注校验（≥10 字）。 */
const markError = computed(() => (markNote.value.trim().length > 0 && markNote.value.trim().length < 10 ? '还差若干字，至少 10 字' : ''));

/** 打开标记弹窗：重置草稿。 */
function openMark(): void {
  markNote.value = device.value?.anomalyNote ?? '';
  markOpen.value = true;
}

/** 关闭标记弹窗：清草稿。 */
function closeMark(): void {
  markOpen.value = false;
  markNote.value = '';
}

/** 提交备注（不改授权）。 */
function submitMark(): void {
  if (markNote.value.trim().length < 10 || !device.value) {
    return;
  }
  void repo.markDeviceAnomaly({ id: device.value.id, note: markNote.value.trim(), actor: DEFAULT_ACTOR });
  closeMark();
  reloadKey.value += 1;
}

// ---------------- 废弃关联激活码 ----------------
/** 废弃弹窗开关。 */
const revokeOpen = ref(false);

/** 废弃摘要。 */
const revokeFacts = computed(() => [
  { label: '租户', value: device.value?.tenant ?? '—' },
  { label: '设备', value: device.value ? `${maskMachineSummary(device.value.machineSummary)}（${device.value.name}）` : '—' },
  { label: '关联激活码', value: device.value?.boundCodeMasked ?? '—' },
  { label: '双人复核', value: dualApproval.value ? '已开启（需第二审批人）' : '未开启' },
]);

/** 提交废弃。 */
function submitRevoke(payload: { reason: string; note: string; secondApprover: string }): void {
  const id = device.value?.boundCodeId;
  if (!id) {
    revokeOpen.value = false;
    return;
  }
  repo.revokeCode({
    id,
    reason: payload.reason,
    // 三字段独立：第二审批人走 secondApprover 字段原样下发，不得折进 note（审计可追溯前提）
    note: payload.note,
    secondApprover: payload.secondApprover,
    actor: DEFAULT_ACTOR,
  });
  revokeOpen.value = false;
  reloadKey.value += 1;
}

/** 返回列表。 */
function goBack(): void {
  void router.push({ name: 'devices' });
}

/** 跳激活码详情。 */
function goToCode(id: string): void {
  void router.push({ name: 'code-detail', params: { id } });
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
  width: 560px;
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
.ac-link {
  background: none;
  border: 0;
  padding: 0;
  color: var(--brand);
  cursor: pointer;
  font-size: var(--fs-table);
}
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
.ac-chain__arrow {
  font-size: var(--fs-caption);
  color: var(--text-3);
}
</style>
