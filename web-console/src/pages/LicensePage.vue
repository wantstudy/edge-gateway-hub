<!--
  =============================================================================
  LicensePage —— 授权与激活（★ 客户端唯一授权触点）
  =============================================================================
  硬红线（`docs/design/ui-gateway-console.md` §3.6 / `activation-rules.md`）：
   客户端**只允许三个操作**：① 复制机器码 ② 输入激活码（必须联网）③ 申请换机。
   本页**绝不**提供任何「解绑 / 重置试用 / 废弃 / revoke / 吊销」入口
   （判定标准是**可点元素**；说明性文字允许出现，但不得有对应按钮或链接）。
   授权状态必须**可解释**：降级时给出「原因 + 恢复路径」，并展示机器码、激活码（脱敏）、
   授权到期时间、剩余天数、试用剩余天数（试用 3 天）。
   授权判定全部在 Rust 侧 —— 前端只做展示，改前端不影响授权结果。
-->
<template>
  <PageHeader
    crumb="系统 / 授权与激活"
    title="授权与激活"
    desc="机器码指纹、租约状态、激活与换机申请。客户端不具备任何解绑或重置能力。"
  >
    <template #actions>
      <button type="button" class="wc-btn wc-btn--sm" @click="refresh">刷新状态</button>
    </template>
  </PageHeader>

  <div class="wc-content">
    <!-- ===== 全局授权横幅（优先级见 §4.6；本页只呈现授权类横幅）===== -->
    <div v-if="banner" class="wc-banner" :class="`wc-banner--${banner.tone}`" data-testid="license-banner">
      <span aria-hidden="true">{{ banner.icon }}</span>
      <span>
        <b>{{ banner.title }}</b>
        <span v-if="banner.text"> · {{ banner.text }}</span>
      </span>
    </div>

    <!-- ===== KPI 区：授权状态一眼可见 ===== -->
    <div class="wc-grid wc-grid--4">
      <div class="wc-kpi">
        <span class="wc-kpi__label">授权状态</span>
        <span class="wc-kpi__value" data-testid="kpi-status">
          <StatusTag :status="license.status" :text="statusText" />
        </span>
        <span class="wc-kpi__sub">{{ license.tierName }} · 校验档位 {{ license.grade }} 档</span>
      </div>
      <div class="wc-kpi">
        <span class="wc-kpi__label">{{ license.status === 'trial' ? '试用剩余' : '授权剩余' }}</span>
        <span class="wc-kpi__value" data-testid="kpi-remaining">{{ remainingValue }}</span>
        <span class="wc-kpi__sub">{{ remainingSub }}</span>
      </div>
      <div class="wc-kpi">
        <span class="wc-kpi__label">授权到期时间</span>
        <span class="wc-kpi__value wc-kpi__value--sm" data-testid="kpi-validUntil">{{ license.validUntil }}</span>
        <span class="wc-kpi__sub">{{ license.status === 'active' ? `剩余 ${license.remainingDays} 天` : '到期后按下方规则降级' }}</span>
      </div>
      <div class="wc-kpi">
        <span class="wc-kpi__label">上次心跳</span>
        <span class="wc-kpi__value wc-kpi__value--sm">{{ license.lastHeartbeatAt }}</span>
        <span class="wc-kpi__sub">下次心跳 {{ license.nextHeartbeatAt }}</span>
      </div>
    </div>

    <!-- ===== 降级/异常可解释区（状态 ∈ 离线宽限 / 已降级 时显示：原因 + 恢复路径）===== -->
    <div
      v-if="license.degradeReason"
      class="wc-banner wc-banner--danger wc-banner--block"
      data-testid="degrade-explain"
    >
      <span aria-hidden="true">⚠</span>
      <span class="wc-banner__stack">
        <span class="wc-banner__line">
          <b>当前状态：{{ statusText }}</b>
        </span>
        <span class="wc-banner__line" data-testid="degrade-reason">
          原因：{{ license.degradeReason }}
        </span>
        <span class="wc-banner__line" data-testid="degrade-recovery">
          恢复路径：{{ recoveryPath }}（本地采集持续，数据留在本地可导出，恢复联网后自动恢复转发）
        </span>
      </span>
    </div>

    <!-- ===== 主体栅格：机器码指纹 + 授权操作 ===== -->
    <div class="wc-grid wc-grid--2-1">
      <!-- 机器码指纹 -->
      <section class="wc-card">
        <div class="wc-card__head">
          <h3>机器码指纹</h3>
          <span class="wc-card__sub">多源锚点 + HMAC · 一机一码</span>
        </div>
        <div class="wc-card__body">
          <MachineCodeDisplay
            :code="gateway.machineCode"
            :anchors="anchorList"
            data-testid="machine-code"
          />
          <p class="wc-note">
            <span class="wc-note__icon" aria-hidden="true">ⓘ</span>
            <span>
              容器部署时锚点取自宿主机，容器重建不改变机器码；
              N-of-M 容错：替换任一易变锚点仍判为同机，整机更换则判为异机。
            </span>
          </p>
          <dl class="wc-kv">
            <dt>设备名称</dt>
            <dd>{{ gateway.name }}</dd>
            <dt>部署形态</dt>
            <dd>{{ gateway.deployMode }}</dd>
            <dt>版本</dt>
            <dd class="wc-mono">{{ gateway.version }}</dd>
            <dt>锚点来源（只读）</dt>
            <dd data-testid="anchor-sources">{{ license.anchorSources }}</dd>
          </dl>
        </div>
      </section>

      <!-- 授权操作：客户端仅此三项 -->
      <section class="wc-card">
        <div class="wc-card__head">
          <h3>授权操作</h3>
          <span class="wc-card__sub">客户端仅此三项</span>
        </div>
        <div class="wc-card__body">
          <!-- 触点 1：复制机器码 -->
          <button
            type="button"
            class="wc-btn wc-btn--primary wc-btn--wide"
            data-testid="btn-copy-machine"
            @click="copyMachineCode"
          >
            {{ copied ? '已复制机器码' : '复制机器码' }}
          </button>

          <!-- 触点 2：输入激活码（联网激活） -->
          <button
            type="button"
            class="wc-btn wc-btn--wide"
            data-testid="btn-open-activate"
            @click="openActivate"
          >
            输入激活码
          </button>

          <!-- 触点 3：申请换机 -->
          <button
            type="button"
            class="wc-btn wc-btn--wide"
            data-testid="btn-open-transfer"
            @click="openTransfer"
          >
            申请换机
          </button>

          <!-- 能力边界说明（文字，无对应可点元素） -->
          <div class="wc-impact" data-testid="impact-note">
            <p class="wc-impact__title">客户端不提供的能力</p>
            <ul>
              <li>没有解绑入口 —— 换机必须由厂商在管理后台处理原码后重新发放。</li>
              <li>没有重置试用入口 —— 试用期由云端首次激活时间判定，本地不可改。</li>
              <li>授权判定全部在 Rust 侧完成，界面只做展示，改前端不影响授权结果。</li>
            </ul>
          </div>
        </div>
      </section>
    </div>

    <!-- ===== 授权详情：激活码（脱敏）+ 租约 + 到期后行为 ===== -->
    <div class="wc-grid wc-grid--2">
      <section class="wc-card">
        <div class="wc-card__head">
          <h3>授权详情</h3>
          <span class="wc-card__sub">激活码默认脱敏</span>
        </div>
        <div class="wc-card__body">
          <dl class="wc-kv">
            <dt>激活码</dt>
            <dd>
              <MaskedCode
                :code="activationCode"
                :can-reveal="canReveal"
                @reveal="onRevealCode"
                data-testid="masked-code"
              />
            </dd>
            <dt>授权版本</dt>
            <dd>{{ license.tierName }}</dd>
            <dt>校验档位</dt>
            <dd>{{ license.grade }} 档 · {{ gradeHint }}</dd>
            <dt>租约有效至</dt>
            <dd class="wc-mono">{{ license.validUntil }}</dd>
            <dt>剩余天数</dt>
            <dd data-testid="detail-remaining">{{ remainingDetail }}</dd>
            <dt>到期后</dt>
            <dd data-testid="on-expire">{{ license.onExpireText }}</dd>
          </dl>
          <button type="button" class="wc-btn wc-btn--sm" data-testid="btn-capabilities" @click="showCapabilities = true">
            了解档位差异
          </button>
        </div>
      </section>

      <!-- 换机申请：客户侧视角（仅提交申请，厂商后台处理） -->
      <section class="wc-card">
        <div class="wc-card__head">
          <h3>换机申请</h3>
          <span class="wc-card__sub">客户侧仅提交申请</span>
        </div>
        <div class="wc-card__body">
          <p class="wc-note">
            <span class="wc-note__icon" aria-hidden="true">ⓘ</span>
            <span>
              设备硬件更换或系统重装导致机器码变化时，需厂商在后台重新发放激活码。
              本页面仅提交申请，不具备任何自助处理能力。
            </span>
          </p>

          <!-- 已有申请：展示受理状态 -->
          <div v-if="transferTicket" class="wc-ticket" data-testid="transfer-ticket">
            <div class="wc-ticket__row">
              <span class="wc-ticket__key">状态</span>
              <span class="wc-ticket__val"><StatusTag status="pending" text="待厂商处理" /></span>
            </div>
            <div class="wc-ticket__row">
              <span class="wc-ticket__key">受理编号</span>
              <span class="wc-ticket__val wc-mono" data-testid="ticket-id">{{ transferTicket.id }}</span>
            </div>
            <div class="wc-ticket__row">
              <span class="wc-ticket__key">提交时间</span>
              <span class="wc-ticket__val wc-mono">{{ transferTicket.at }}</span>
            </div>
            <div class="wc-ticket__row">
              <span class="wc-ticket__key">原因</span>
              <span class="wc-ticket__val">{{ transferTicket.reason }}</span>
            </div>
            <p class="wc-ticket__hint" data-testid="ticket-hint">
              厂商将在 1 个工作日内处理：作废原激活码并重新发放新码；收到新码后在本页「输入激活码」完成激活。
            </p>
          </div>

          <EmptyState
            v-else
            title="尚未提交换机申请"
            desc="机器码已变化或计划更换主机时，提交申请后由厂商在总管理后台处理并重新发放激活码。"
          >
            <template #actions>
              <button type="button" class="wc-btn wc-btn--primary" data-testid="btn-open-transfer-2" @click="openTransfer">
                申请换机
              </button>
            </template>
          </EmptyState>
        </div>
      </section>
    </div>
  </div>

  <!-- ===== 换机申请弹窗（二次确认 + 原因必填 + 对象名二次校验）===== -->
  <DangerConfirmModal
    :open="transferOpen"
    :title="`申请换机 · ${gateway.name}`"
    :impacts="transferImpacts"
    :facts="transferFacts"
    :reasons="TRANSFER_REASONS"
    :min-note-length="10"
    :confirm-value="gateway.machineCode"
    confirm-label="机器码二次确认（输入本机机器码后 8 位）"
    confirm-placeholder="输入本机机器码去分隔符后的后 8 位"
    confirm-text="提交换机申请"
    @close="transferOpen = false"
    @submit="onTransferSubmit"
  />

  <!-- ===== 激活码输入弹窗（客户端触点 2，联网激活）===== -->
  <Teleport to="body">
    <div v-if="activateOpen" class="wc-modal__mask" @click.self="activateOpen = false">
      <div class="wc-modal" role="dialog" aria-modal="true" aria-label="输入激活码">
        <div class="wc-modal__head">
          <h3 class="wc-modal__title">输入激活码</h3>
        </div>
        <div class="wc-modal__body">
          <p class="wc-note">
            <span class="wc-note__icon" aria-hidden="true">ⓘ</span>
            <span>激活需联网（约 3 秒），一个激活码只能激活一台设备。若本机已更换硬件，请改点「申请换机」。</span>
          </p>
          <UiField
            label="激活码"
            required
            :hint="'格式形如 IOT-2026-XXXX-XXXX-XXXX-XX'"
            :error="activateError"
          >
            <UiInput
              v-model="activateCode"
              :placeholder="'IOT-2026-____-____-____-__'"
              :invalid="activateError.length > 0"
              data-testid="activate-input"
            />
          </UiField>
          <p v-if="activateResult" class="wc-modal__result" data-testid="activate-result">{{ activateResult }}</p>
          <p v-else class="wc-modal__hint" data-testid="activate-format">
            提交后由网关联网完成校验；真实授权判定在网关（Rust）侧执行。
          </p>
        </div>
        <div class="wc-modal__foot">
          <button type="button" class="wc-btn" @click="activateOpen = false">取消</button>
          <RoleGate :allowed="canOperateLicense" mode="disable" deny-text="当前角色无权提交激活码" fallback-label="无权操作">
            <button type="button" class="wc-btn wc-btn--primary" data-testid="btn-activate-submit" @click="submitActivate">
              激活
            </button>
          </RoleGate>
        </div>
      </div>
    </div>
  </Teleport>

  <!-- ===== 档位能力弹窗 ===== -->
  <Teleport to="body">
    <div v-if="showCapabilities" class="wc-modal__mask" @click.self="showCapabilities = false">
      <div class="wc-modal" role="dialog" aria-modal="true" aria-label="档位能力">
        <div class="wc-modal__head">
          <h3 class="wc-modal__title">档位能力对照 · {{ license.tierName }}</h3>
        </div>
        <div class="wc-modal__body">
          <table class="wc-cap-table">
            <thead>
              <tr><th>能力</th><th>当前档位</th><th>说明</th></tr>
            </thead>
            <tbody>
              <tr v-for="cap in license.capabilities" :key="cap.name">
                <td>{{ cap.name }}</td>
                <td>
                  <span v-if="cap.included" class="wc-cap__yes">含</span>
                  <span v-else class="wc-cap__no">不含</span>
                </td>
                <td class="wc-cap__note">{{ cap.note }}</td>
              </tr>
            </tbody>
          </table>
        </div>
        <div class="wc-modal__foot">
          <button type="button" class="wc-btn" @click="showCapabilities = false">关闭</button>
        </div>
      </div>
    </div>
  </Teleport>
</template>

<script setup lang="ts">
/**
 * @file LicensePage.vue
 * @module web-console/pages/LicensePage
 * @description 授权与激活页（客户端唯一授权触点）。
 *
 * 边界（红线）：
 *  · 只有三个合法动作：复制机器码 / 输入激活码 / 申请换机；
 *  · 无任何自助「解绑 / 重置试用 / 废弃 / revoke」入口（判定标准是**可点元素**）；
 *  · 授权判定在 Rust 侧，前端只做展示与「提交申请」。
 */
import { computed, ref } from 'vue';
import {
  PageHeader,
  StatusTag,
  EmptyState,
  MachineCodeDisplay,
  MaskedCode,
  RoleGate,
  DangerConfirmModal,
  UiField,
  UiInput,
  formatMachineCode,
  type DangerFact,
} from '@ui-kit';
import { repo, DEFAULT_ACTOR } from '@/api/repo';
import { session } from '../store/session';

// ---------------------------------------------------------------------------
// 展示态
// ---------------------------------------------------------------------------

/** 刷新触发器（重取授权快照）。 */
const reloadKey = ref(0);

/** 网关信息（本机视角）。 */
const gateway = computed(() => repo.getGateway());

/** 授权快照。
 *
 * 现状：默认取 `licenseSnapshot`（已授权）。若顶栏会话被切换到降级快照
 * （`session.state.license.status === 'grace'`），本页同步展示降级态，用于验证
 * 「降级必须可解释」。真实降级判定在 Rust 侧。
 */
const license = computed(() => {
  void reloadKey.value;
  return session.state.license.status === 'grace' ? repo.getLicenseDegraded() : repo.getLicense();
});

/** 当前操作者角色是否可执行授权类操作（仅 admin）。 */
const canOperateLicense = computed<boolean>(() => session.state.role === 'admin');

/** 是否可查看激活码明文（仅 admin；前端仅控制可见性）。 */
const canReveal = computed<boolean>(() => session.state.role === 'admin');

// ---------------------------------------------------------------------------
// 状态文案 / 可解释字段
// ---------------------------------------------------------------------------

/** 状态展示文案（补充剩余时长上下文）。 */
const statusText = computed<string>(() => {
  const lic = license.value;
  if (lic.status === 'trial') {
    return `试用中（剩余 ${lic.remainingText}）`;
  }
  if (lic.status === 'grace') {
    return '已降级（离线宽限）';
  }
  if (lic.status === 'active') {
    return '已授权';
  }
  return '已停用';
});

/** KPI 剩余值。 */
const remainingValue = computed<string>(() => {
  const lic = license.value;
  if (lic.status === 'trial') {
    return lic.remainingText;
  }
  if (lic.status === 'active') {
    return `${lic.remainingDays} 天`;
  }
  return lic.remainingText;
});

/** KPI 剩余副文案。 */
const remainingSub = computed<string>(() => {
  const lic = license.value;
  if (lic.status === 'trial') {
    return '试用期 3 天（云端首次激活时间判定）';
  }
  if (lic.status === 'active') {
    return '24h 自动心跳续期';
  }
  return '恢复联网/重新激活后自动恢复';
});

/** 详情区剩余天数文本。 */
const remainingDetail = computed<string>(() => {
  const lic = license.value;
  if (lic.status === 'active') {
    return `${lic.remainingDays} 天`;
  }
  if (lic.status === 'trial') {
    return `试用剩余 ${lic.remainingText}（共 3 天）`;
  }
  return `宽限期剩余 ${lic.remainingText}`;
});

/** 校验档位说明。 */
const gradeHint = computed<string>(() => {
  const map: Record<string, string> = {
    A: '本地验签（离线可运行）',
    B: '直连客户 Broker + 审计回执',
    C: '云端心跳校验',
  };
  return map[license.value.grade] ?? '云端心跳校验';
});

/** 锚点来源（拆分 `anchorSources` 供 `MachineCodeDisplay` 逐行展示）。 */
const anchorList = computed<readonly string[]>(() =>
  license.value.anchorSources
    .split('·')
    .map((s) => s.trim())
    .filter((s) => s.length > 0),
);

/**
 * 全局横幅（授权类，优先级最高；见设计 §4.6）。
 * 文案与色调与降级原因严格对应，禁止「只说降级不说原因」。
 */
const banner = computed<{ tone: 'ok' | 'warn' | 'danger'; icon: string; title: string; text: string } | null>(() => {
  const lic = license.value;
  if (lic.status === 'grace' || lic.status === 'stopped') {
    return {
      tone: 'danger',
      icon: '⚠',
      title: lic.status === 'grace' ? '授权已降级（离线宽限）' : '授权已停用',
      text: lic.degradeReason || '北向转发已停用，本地采集继续。',
    };
  }
  if (lic.status === 'trial') {
    return {
      tone: 'warn',
      icon: '⚠',
      title: `试用剩余 ${lic.remainingText}`,
      text: '到期后降级为免费基础版（8 设备 / ≥1s / 无北向转发 / 无 OTA）。',
    };
  }
  return {
    tone: 'ok',
    icon: '●',
    title: `授权正常 · ${lic.tierName}`,
    text: `租约有效期至 ${lic.validUntil}，上次心跳 ${lic.lastHeartbeatAt}（正常）。`,
  };
});

/** 降级恢复路径（由状态推导，禁止空泛「请联系管理员」）。 */
const recoveryPath = computed<string>(() => {
  const lic = license.value;
  if (lic.status === 'trial') {
    return '输入激活码完成联网激活';
  }
  if (lic.status === 'grace') {
    return '恢复网络以完成心跳，或输入新的激活码';
  }
  if (lic.status === 'stopped') {
    return '使用厂商重新发放的激活码完成激活';
  }
  return '无需操作';
});

// ---------------------------------------------------------------------------
// 触点 1：复制机器码
// ---------------------------------------------------------------------------

/** 复制反馈（1.6s 自动复原）。 */
const copied = ref(false);
let copyTimer: ReturnType<typeof setTimeout> | null = null;

/** 复制机器码到剪贴板，并记录前端侧审计。 */
async function copyMachineCode(): Promise<void> {
  const text = formatMachineCode(gateway.value.machineCode);
  try {
    if (navigator.clipboard && window.isSecureContext) {
      await navigator.clipboard.writeText(text);
    } else {
      const el = document.createElement('textarea');
      el.value = text;
      el.style.position = 'fixed';
      el.style.opacity = '0';
      document.body.appendChild(el);
      el.select();
      document.execCommand('copy');
      document.body.removeChild(el);
    }
    copied.value = true;
    if (copyTimer) {
      clearTimeout(copyTimer);
    }
    copyTimer = setTimeout(() => {
      copied.value = false;
    }, 1600);
  } catch {
    copied.value = false;
  }
  repo.logCopyMachineCode({ actor: DEFAULT_ACTOR });
}

// ---------------------------------------------------------------------------
// 触点 2：输入激活码（联网激活）
// ---------------------------------------------------------------------------

/** 激活弹窗开关。 */
const activateOpen = ref(false);

/** 激活码草稿。 */
const activateCode = ref('');

/** 激活行内错误。 */
const activateError = ref('');

/** 激活结果文案（成功提示）。 */
const activateResult = ref('');

/** 打开激活弹窗（重置草稿）。 */
function openActivate(): void {
  if (!canOperateLicense.value) {
    return;
  }
  activateOpen.value = true;
  activateCode.value = '';
  activateError.value = '';
  activateResult.value = '';
}

/** 提交激活码：先本地格式校验，再调用仓库（不做授权判定）。 */
function submitActivate(): void {
  activateError.value = '';
  activateResult.value = '';
  const result = repo.activate({ code: activateCode.value, actor: DEFAULT_ACTOR });
  if (!result.ok) {
    activateError.value = result.message;
    return;
  }
  activateResult.value = result.message;
  activateCode.value = '';
}

// ---------------------------------------------------------------------------
// 触点 3：申请换机（厂商后台处理）
// ---------------------------------------------------------------------------

/** 换机弹窗开关。 */
const transferOpen = ref(false);

/** 已提交的换机受理单（本会话内存态，演示用）。 */
const transferTicket = ref<{ id: string; at: string; reason: string } | null>(null);

/** 换机原因枚举（必选；**不含**任何「废弃/解绑」字样，避免误导客户以为客户端可执行）。 */
const TRANSFER_REASONS: readonly string[] = [
  '硬件故障更换主机',
  '系统重装导致机器码变化',
  '产线搬迁更换设备',
  '其它（请在补充说明中描述）',
];

/** 换机影响清单（把后果与恢复路径写清）。 */
const transferImpacts: readonly string[] = [
  '本操作仅提交换机申请，不会立即改变本机授权状态。',
  '受理后由厂商在总管理后台作废原激活码并重新发放新码（客户端不具备该能力）。',
  '收到新码前，本机维持当前授权状态；本地采集持续，数据不丢失。',
];

/** 打开换机弹窗（仅 admin）。 */
function openTransfer(): void {
  if (!canOperateLicense.value) {
    return;
  }
  transferOpen.value = true;
}

/** 换机对象摘要（让操作者确认「在操作谁」）。 */
const transferFacts = computed<readonly DangerFact[]>(() => [
  { label: '设备名称', value: gateway.value.name },
  { label: '当前机器码', value: formatMachineCode(gateway.value.machineCode) },
  { label: '当前授权', value: `${license.value.tierName} · ${statusText.value}` },
]);

/** 提交换机申请，成功后展示受理编号与「待厂商处理」状态。 */
function onTransferSubmit(payload: { reason: string; note: string; tail: string }): void {
  const result = repo.submitTransferRequest({
    oldMachineCode: formatMachineCode(gateway.value.machineCode),
    newMachineCode: '',
    reason: `${payload.reason}；${payload.note}`,
    contact: DEFAULT_ACTOR,
    actor: DEFAULT_ACTOR,
  });
  transferOpen.value = false;
  if (!result.ok) {
    return;
  }
  const now = new Date();
  const p = (n: number): string => String(n).padStart(2, '0');
  transferTicket.value = {
    id: result.ticketId,
    at: `${now.getFullYear()}-${p(now.getMonth() + 1)}-${p(now.getDate())} ${p(now.getHours())}:${p(now.getMinutes())}:${p(now.getSeconds())}`,
    reason: payload.reason,
  };
}

// ---------------------------------------------------------------------------
// 其他
// ---------------------------------------------------------------------------

/** 档位能力弹窗开关。 */
const showCapabilities = ref(false);

/** 激活码展示值：已激活态由厂商发放（演示用固定值），否则空。 */
const activationCode = computed<string>(() => (license.value.status === 'active' || license.value.status === 'trial' ? 'IOT-2026-8C3F-1234-ABCD-A1' : ''));

/** 揭示激活码明文（记录审计；前端仅控制可见性）。 */
function onRevealCode(): void {
  repo.logCopyMachineCode({ actor: DEFAULT_ACTOR });
}

/** 刷新授权快照。 */
function refresh(): void {
  reloadKey.value += 1;
}
</script>

<style scoped>
.wc-btn--wide {
  width: 100%;
}
.wc-kpi__value--sm {
  font-size: 15px;
}
.wc-banner--block {
  align-items: flex-start;
}
.wc-banner__stack {
  display: flex;
  flex-direction: column;
  gap: 4px;
}
.wc-banner__line {
  line-height: 1.6;
}
/* 能力边界说明：文字性说明，**不含**任何可点元素 */
.wc-impact {
  background: var(--warn-bg);
  border: 1px solid var(--warn-border);
  border-radius: var(--radius-sm);
  padding: 10px 12px;
  color: var(--warn-fg);
}
.wc-impact__title {
  margin: 0;
  font-size: var(--fs-table);
  font-weight: 600;
}
.wc-impact ul {
  margin: 6px 0 0;
  padding-left: 18px;
  font-size: var(--fs-caption);
  line-height: 1.75;
}
/* 受理单卡片 */
.wc-ticket {
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  padding: 12px;
  display: flex;
  flex-direction: column;
  gap: 8px;
}
.wc-ticket__row {
  display: flex;
  gap: 12px;
  font-size: var(--fs-table);
}
.wc-ticket__key {
  flex: 0 0 80px;
  color: var(--text-3);
  font-size: var(--fs-caption);
}
.wc-ticket__val {
  color: var(--text-1);
}
.wc-ticket__hint {
  margin: 4px 0 0;
  font-size: var(--fs-caption);
  color: var(--text-3);
  line-height: 1.6;
}
/* 档位能力表 */
.wc-cap-table {
  width: 100%;
  border-collapse: collapse;
  font-size: var(--fs-table);
}
.wc-cap-table th,
.wc-cap-table td {
  text-align: left;
  padding: 8px 10px;
  border-bottom: 1px solid var(--divider);
}
.wc-cap-table th {
  font-size: var(--fs-caption);
  color: var(--text-3);
  font-weight: 600;
}
.wc-cap__yes {
  color: var(--ok-fg);
  font-weight: 600;
}
.wc-cap__no {
  color: var(--danger-fg);
  font-weight: 600;
}
.wc-cap__note {
  color: var(--text-2);
  font-size: var(--fs-caption);
}
/* 通用弹窗（激活 / 档位） */
.wc-modal__mask {
  position: fixed;
  inset: 0;
  background: rgba(29, 33, 41, 0.45);
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 2000;
  padding: 24px;
}
.wc-modal {
  background: #fff;
  border-radius: var(--radius);
  box-shadow: var(--shadow);
  width: 520px;
  max-width: 100%;
  display: flex;
  flex-direction: column;
}
.wc-modal__head {
  padding: 16px 20px;
  border-bottom: 1px solid var(--divider);
}
.wc-modal__title {
  margin: 0;
  font-size: 16px;
  font-weight: 600;
}
.wc-modal__body {
  padding: 20px;
  display: flex;
  flex-direction: column;
  gap: 12px;
}
.wc-modal__hint {
  margin: 0;
  font-size: var(--fs-caption);
  color: var(--text-3);
  line-height: 1.6;
}
.wc-modal__result {
  margin: 0;
  font-size: var(--fs-table);
  color: var(--ok-fg);
  background: var(--ok-bg);
  border: 1px solid var(--ok-border);
  border-radius: var(--radius-sm);
  padding: 8px 10px;
  line-height: 1.6;
}
.wc-modal__foot {
  padding: 14px 20px;
  border-top: 1px solid var(--divider);
  display: flex;
  justify-content: flex-end;
  gap: 8px;
}
</style>
