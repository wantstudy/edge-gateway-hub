<!--
  =============================================================================
  SettingsPage —— 系统设置
  =============================================================================
  交付要点（`docs/design/ui-gateway-console.md` §3.7）：
    · 网关基础配置（名称 / 位置 / 时区 / NTP 时间源）；
    · 配置**回滚**（`POST /api/settings/rollback`）—— 危险操作，二次确认 + 原因必填 + 对象名二次校验；
    · 部分项需重启生效，需明确标注。
  边界：写操作最终由网关（Rust）侧判定并执行；前端只做展示与提交。
-->
<template>
  <PageHeader
    crumb="系统 / 系统设置"
    title="系统设置"
    desc="网关基础配置、时间与安全设置。部分项需重启生效；配置回滚为高危操作。"
  >
    <template #actions>
      <RoleGate :allowed="canEdit" mode="disable" deny-text="当前角色无权保存设置" fallback-label="无权保存">
        <button type="button" class="wc-btn wc-btn--primary wc-btn--sm" data-testid="btn-save-settings" @click="saveSettings">
          {{ saved ? '已保存' : '保存' }}
        </button>
      </RoleGate>
    </template>
  </PageHeader>

  <div class="wc-content">
    <!-- 未保存提示 -->
    <div v-if="dirty" class="wc-banner wc-banner--warn" data-testid="dirty-banner">
      <span aria-hidden="true">⚠</span>
      <span>有未保存的修改。保存后可在此页「配置回滚」中回退到历史版本。</span>
    </div>

    <div class="wc-grid wc-grid--2-1">
      <!-- 网关基础配置 -->
      <section class="wc-card">
        <div class="wc-card__head">
          <h3>网关标识</h3>
          <span class="wc-card__sub">保存后即时生效</span>
        </div>
        <div class="wc-card__body">
          <div class="wc-form">
            <UiField label="网关名称" required>
              <UiInput v-model="form.name" placeholder="如 线1-网关-01" data-testid="set-name" />
            </UiField>
            <UiField label="所属站点" required hint="位置区 / 线体，用于运维定位">
              <UiInput v-model="form.site" placeholder="如 长沙工厂 / 注塑一车间" data-testid="set-site" />
            </UiField>
            <UiField label="时区" required hint="影响审计时间与数据时间戳">
              <UiSelect v-model="form.timezone" :options="timezoneOptions" data-testid="set-timezone" />
            </UiField>
            <UiField label="界面语言" hint="首期仅提供简体中文">
              <UiSelect v-model="form.locale" :options="localeOptions" :disabled="true" />
            </UiField>
            <UiField label="NTP 时间源" required hint="可信时间是试用防改时间、±5min 验签窗口的共同依赖">
              <UiInput v-model="form.ntp" placeholder="如 ntp.aliyun.com" data-testid="set-ntp" />
            </UiField>
          </div>
          <p class="wc-hint" data-testid="clock-hint">
            当前时钟 {{ clockText }} · 校时正常，偏移 12 ms
          </p>
        </div>
      </section>

      <!-- 配置回滚 -->
      <section class="wc-card">
        <div class="wc-card__head">
          <h3>配置回滚</h3>
          <span class="wc-card__sub">危险操作</span>
        </div>
        <div class="wc-card__body">
          <dl class="wc-kv">
            <dt>当前版本</dt>
            <dd class="wc-mono" data-testid="current-version">{{ currentVersion }}</dd>
            <dt>可用历史版本</dt>
            <dd data-testid="history-count">{{ history.length }} 个</dd>
            <dt>回滚影响</dt>
            <dd>仅回滚配置，不回滚采集数据与设备连接</dd>
          </dl>

          <div class="wc-list">
            <div v-for="snap in history" :key="snap.id" class="wc-list__item">
              <div>
                <div class="wc-list__title wc-mono">{{ snap.id }}</div>
                <div class="wc-list__desc">{{ snap.at }} · {{ snap.note }} · 操作者 {{ snap.operator }}</div>
              </div>
              <div class="wc-list__ops">
                <RoleGate :allowed="canRollback" mode="disable" deny-text="当前角色无权执行配置回滚" fallback-label="回滚">
                  <button
                    type="button"
                    class="wc-btn wc-btn--sm wc-btn--danger"
                    data-testid="btn-rollback"
                    @click="openRollback(snap)"
                  >
                    回滚到此版本
                  </button>
                </RoleGate>
              </div>
            </div>
          </div>

          <p class="wc-note" data-testid="rollback-note">
            <span class="wc-note__icon" aria-hidden="true">ⓘ</span>
            <span>回滚会将配置恢复到所选快照；若该快照与当前版本不兼容，网关可能提示需重启。</span>
          </p>
        </div>
      </section>
    </div>

    <!-- ===== 其他设置（只读展示，首期不改）===== -->
    <div class="wc-grid wc-grid--3">
      <section class="wc-card">
        <div class="wc-card__head"><h3>网络与端口</h3></div>
        <div class="wc-card__body">
          <dl class="wc-kv">
            <dt>监听地址</dt>
            <dd class="wc-mono">0.0.0.0:8080</dd>
            <dt>HTTPS</dt>
            <dd>自签名证书</dd>
            <dt>会话超时</dt>
            <dd>30 分钟</dd>
          </dl>
          <p class="wc-note wc-note--warn">
            <span class="wc-note__icon" aria-hidden="true">⚠</span>
            <span>管理界面暴露在局域网时<b>必须启用 HTTPS</b>，否则口令与会话令牌可被嗅探。</span>
          </p>
        </div>
      </section>

      <section class="wc-card">
        <div class="wc-card__head"><h3>存储与保留</h3></div>
        <div class="wc-card__body">
          <dl class="wc-kv">
            <dt>数据目录</dt>
            <dd class="wc-mono">/var/lib/iot-daq</dd>
            <dt>队列上限</dt>
            <dd>10 GB / 7 天</dd>
            <dt>遥测保留</dt>
            <dd>30 天</dd>
            <dt>日志保留</dt>
            <dd>180 天</dd>
          </dl>
        </div>
      </section>

      <section class="wc-card">
        <div class="wc-card__head"><h3>安全</h3></div>
        <div class="wc-card__body">
          <dl class="wc-kv">
            <dt>北向 TLS 校验</dt>
            <dd><StatusTag status="user_enabled" text="强制" /></dd>
            <dt>管理界面 HTTPS</dt>
            <dd><StatusTag status="user_enabled" text="启用" /></dd>
            <dt>审计日志</dt>
            <dd>追加不可篡改</dd>
          </dl>
        </div>
      </section>
    </div>
  </div>

  <!-- ===== 配置回滚：危险操作二次确认（原因必填 + 对象名二次校验）===== -->
  <DangerConfirmModal
    :open="rollback.open"
    :title="rollback.title"
    :impacts="rollback.impacts"
    :facts="rollback.facts"
    :reasons="ROLLBACK_REASONS"
    :min-note-length="10"
    :confirm-value="rollback.snapshotId"
    confirm-label="快照二次确认（输入快照编号）"
    confirm-placeholder="输入目标快照编号"
    confirm-text="确认回滚"
    @close="closeRollback"
    @submit="submitRollback"
  />
</template>

<script setup lang="ts">
/**
 * @file SettingsPage.vue
 * @module web-console/pages/SettingsPage
 * @description 系统设置页（网关基础配置 + 配置回滚）。
 */
import { computed, reactive, ref } from 'vue';
import {
  PageHeader,
  UiField,
  UiInput,
  UiSelect,
  StatusTag,
  RoleGate,
  DangerConfirmModal,
  type SelectOption,
  type DangerFact,
} from '@ui-kit';
import { session } from '../store/session';
import { repo } from '../mock/mock-data';

// ---------------------------------------------------------------------------
// 网关基础配置
// ---------------------------------------------------------------------------

/** 当前网关信息（本机视角）。 */
const gateway = computed(() => repo.getGateway());

/** 表单草稿。 */
const form = reactive({
  name: gateway.value.name,
  site: '长沙工厂 / 注塑一车间',
  timezone: 'Asia/Shanghai',
  locale: 'zh-CN',
  ntp: 'ntp.aliyun.com',
});

/** 是否已保存（显示反馈）。 */
const saved = ref(false);

/** 是否有未保存修改。 */
const dirty = ref(false);

/** 时区选项。 */
const timezoneOptions: readonly SelectOption[] = [
  { value: 'Asia/Shanghai', label: 'Asia/Shanghai（UTC+08:00）' },
  { value: 'UTC', label: 'UTC（协调世界时）' },
];

/** 语言选项（首期仅中文）。 */
const localeOptions: readonly SelectOption[] = [{ value: 'zh-CN', label: '简体中文' }];

/** 当前时钟文本。 */
const clockText = '2026-09-23 13:45:12 (+08:00)';

/** 当前角色是否可编辑设置（仅 admin / engineer）。 */
const canEdit = computed<boolean>(() => session.state.role === 'admin' || session.state.role === 'engineer');

/** 保存设置（演示：仅本地反馈；真实系统由网关侧执行并审计）。 */
function saveSettings(): void {
  saved.value = true;
  dirty.value = false;
  setTimeout(() => {
    saved.value = false;
  }, 1600);
}

// ---------------------------------------------------------------------------
// 配置回滚
// ---------------------------------------------------------------------------

/** 历史快照。 */
interface Snapshot {
  /** 快照编号（版本号） */
  readonly id: string;
  /** 生成时间 */
  readonly at: string;
  /** 说明 */
  readonly note: string;
  /** 操作者 */
  readonly operator: string;
}

/** 当前配置版本。 */
const currentVersion = 'v20260923-1341';

/** 历史快照列表。 */
const history: readonly Snapshot[] = [
  { id: 'v20260923-0200', at: '2026-09-23 02:00', note: '自动备份', operator: 'system' },
  { id: 'v20260922-1745', at: '2026-09-22 17:45', note: '北向出口参数调整', operator: 'admin' },
  { id: 'v20260921-1030', at: '2026-09-21 10:30', note: '点位死区批量更新', operator: 'eng01' },
];

/** 当前角色是否可执行回滚（仅 admin）。 */
const canRollback = computed<boolean>(() => session.state.role === 'admin');

/** 回滚确认弹窗状态。 */
const rollback = reactive<{
  open: boolean;
  snapshotId: string;
  title: string;
  impacts: readonly string[];
  facts: readonly DangerFact[];
}>({
  open: false,
  snapshotId: '',
  title: '',
  impacts: [],
  facts: [],
});

/** 回滚原因枚举。 */
const ROLLBACK_REASONS: readonly string[] = [
  '配置变更后出现异常',
  '误操作需恢复',
  '现场参数需要还原',
  '其它（请在补充说明中描述）',
];

/** 打开回滚二次确认。 */
function openRollback(snap: Snapshot): void {
  rollback.open = true;
  rollback.snapshotId = snap.id;
  rollback.title = `配置回滚 · ${snap.id}`;
  rollback.impacts = [
    `将把当前配置（${currentVersion}）回滚到快照 ${snap.id}。`,
    '仅回滚配置，不回滚采集数据与设备连接状态。',
    '回滚后网关可能提示需重启才能完全生效。',
    '回滚前的当前配置会自动留存一份快照，可再次回滚回来。',
  ];
  rollback.facts = [
    { label: '目标快照', value: snap.id },
    { label: '快照时间', value: snap.at },
    { label: '快照说明', value: snap.note },
    { label: '当前版本', value: currentVersion },
  ];
}

/** 关闭回滚确认。 */
function closeRollback(): void {
  rollback.open = false;
}

/** 提交回滚。 */
function submitRollback(_payload: { reason: string; note: string; tail: string }): void {
  rollback.open = false;
}
</script>

<style scoped>
.wc-form {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: 14px 16px;
}
.wc-note--warn .wc-note__icon {
  color: var(--warn);
}
</style>
