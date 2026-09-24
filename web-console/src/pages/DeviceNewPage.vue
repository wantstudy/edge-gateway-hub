<template>
  <!--
    DeviceNewPage —— 新增设备（接入分组第 2 页，路由 `/device-new`）。

    硬性约定遵守情况：
      · 四步向导（基础信息 / 连接参数 / 采集策略 / 确认提交），步骤间不可跳步；
      · **连接参数字段随协议整块替换**（PROTO_FIELDS），不用万能表单硬塞；
      · 每一步的「下一步」按本步校验实时禁用（不是提交后才报错）；
      · 写操作受 RoleGate 控制（工程师及以上）；
      · 提交只把配置交给 `repo.createDevice`（内存态 + 写审计），
        联网连通性与授权判定一律在网关 Rust 侧，前端不触碰。
  -->
  <PageHeader
    crumb="接入 / 新增设备"
    title="新增设备"
    desc="按四步向导录入设备连接配置。连接参数字段随协议整块替换；提交后由网关侧完成连通性探测与授权校验。"
  >
    <template #actions>
      <button type="button" class="wc-btn" @click="go('devices')">返回设备列表</button>
    </template>
  </PageHeader>

  <div class="wc-content">
    <!-- 步骤指示器 -->
    <div class="wc-card">
      <ol class="dv-steps">
        <li
          v-for="(step, i) in STEPS"
          :key="step"
          class="dv-steps__item"
          :class="{ 'is-on': currentStep === i + 1, 'is-done': currentStep > i + 1 }"
        >
          <span class="dv-steps__num">{{ i + 1 }}</span>
          <span class="dv-steps__label">{{ step }}</span>
        </li>
      </ol>
    </div>

    <!-- 步骤 1：基础信息 -->
    <section v-show="currentStep === 1" class="wc-card">
      <div class="wc-card__head"><h3>基础信息</h3></div>
      <div class="wc-card__body">
        <div class="dv-form">
          <UiField label="设备名称" required hint="用于列表与北向目标前缀，建议体现产线 / 工位">
            <UiInput v-model="name" placeholder="如 2#注塑机" :invalid="nameTouched && !name.trim()" />
          </UiField>

          <UiField label="通信协议" required hint="决定下一步出现的连接字段">
            <UiRadio v-model="protocol" :options="protocolRadioOptions" />
          </UiField>
        </div>
        <p v-if="nameTouched && !name.trim()" class="wc-note"><span class="wc-note__icon">ⓘ</span><span>请填写设备名称。</span></p>
      </div>
    </section>

    <!-- 步骤 2：连接参数（随协议整块替换） -->
    <section v-show="currentStep === 2" class="wc-card">
      <div class="wc-card__head">
        <h3>连接参数</h3>
        <span class="wc-card__sub">{{ protocolLabel }} · 字段随协议变化</span>
      </div>
      <div class="wc-card__body">
        <div class="dv-form">
          <UiField
            v-for="field in currentProtoFields"
            :key="field.key"
            :label="field.label"
            :required="true"
            :hint="field.hint"
            :error="connTouched && !isFieldValid(field) ? '必填，且需为有效数值' : ''"
          >
            <UiInput
              v-model="conn[field.key]"
              :type="field.kind === 'number' ? 'number' : (field.key === 'password' ? 'password' : 'text')"
              :placeholder="field.placeholder ?? ''"
            />
          </UiField>
        </div>
        <p class="wc-note">
          <span class="wc-note__icon">ⓘ</span>
          <span>连接参数仅用于网关侧连通性探测；本页不发起任何真实连接，也不做任何授权判定。</span>
        </p>
      </div>
    </section>

    <!-- 步骤 3：采集策略 -->
    <section v-show="currentStep === 3" class="wc-card">
      <div class="wc-card__head"><h3>采集策略</h3></div>
      <div class="wc-card__body">
        <div class="dv-form dv-form--3">
          <UiField label="采集频率（ms）" required hint="≥ 50，建议 1000" :error="policyTouched && !intervalValid ? '需为 ≥ 50 的整数' : ''">
            <UiInput v-model="intervalMs" type="number" placeholder="1000" />
          </UiField>
          <UiField label="超时（ms）" required hint="≥ 100" :error="policyTouched && !timeoutValid ? '需为 ≥ 100 的整数' : ''">
            <UiInput v-model="timeoutMs" type="number" placeholder="3000" />
          </UiField>
          <UiField label="重试次数（次）" required hint="≥ 0" :error="policyTouched && !retryValid ? '需为 ≥ 0 的整数' : ''">
            <UiInput v-model="retryTimes" type="number" placeholder="3" />
          </UiField>
        </div>
      </div>
    </section>

    <!-- 步骤 4：确认提交 -->
    <section v-show="currentStep === 4" class="wc-card">
      <div class="wc-card__head"><h3>确认提交</h3></div>
      <div class="wc-card__body">
        <dl class="wc-kv">
          <dt>设备名称</dt><dd>{{ name }}</dd>
          <dt>通信协议</dt><dd>{{ protocolLabel }}</dd>
          <dt>连接摘要</dt><dd class="wc-mono">{{ connectionSummary }}</dd>
          <dt>采集频率</dt><dd>{{ intervalMs }} ms</dd>
          <dt>超时</dt><dd>{{ timeoutMs }} ms</dd>
          <dt>重试次数</dt><dd>{{ retryTimes }} 次</dd>
        </dl>
        <p class="wc-note">
          <span class="wc-note__icon">ⓘ</span>
          <span>提交后设备以「离线」态入库，网关侧将按上述配置发起连通性探测与授权校验，结果在设备列表实时反映。</span>
        </p>
      </div>
    </section>

    <!-- 提交成功 -->
    <div v-if="createdName" class="wc-banner wc-banner--ok">
      <span>✓</span>
      <span>设备「{{ createdName }}」已提交，网关侧正在校验连通性与授权。</span>
      <span class="wc-banner__ops">
        <button type="button" class="wc-btn wc-btn--sm" @click="go('devices')">查看设备列表</button>
        <button type="button" class="wc-btn wc-btn--sm" @click="resetWizard">继续新增</button>
      </span>
    </div>

    <!-- 底部操作 -->
    <div class="dv-foot">
      <RoleGate :allowed="canWrite" mode="disable" fallback-label="仅工程师及以上可新增">
        <div class="dv-foot__ops">
          <button type="button" class="wc-btn" :disabled="currentStep === 1" @click="prev">上一步</button>
          <button v-if="currentStep < 4" type="button" class="wc-btn wc-btn--primary" :disabled="!stepValid" @click="next">
            下一步
          </button>
          <button v-else type="button" class="wc-btn wc-btn--primary" :disabled="!stepValid || !!createdName" @click="submit">
            提交设备
          </button>
        </div>
      </RoleGate>
      <span v-if="!stepValid && currentStep < 4" class="dv-foot__hint">请完成本步必填项后再继续。</span>
    </div>
  </div>
</template>

<script setup lang="ts">
/**
 * @file DeviceNewPage.vue
 * @module web-console/pages/DeviceNewPage
 * @description 新增设备四步向导（基础信息 / 连接参数 / 采集策略 / 确认提交）。
 *
 * 连接参数字段随协议整块替换（PROTO_FIELDS），避免通用表单硬塞异构协议字段。
 * 校验实时驱动「下一步」可用性；提交经 `repo.createDevice` 落内存态 + 审计。
 */
import { computed, reactive, ref, watch } from 'vue';
import { useRouter } from 'vue-router';
import {
  PageHeader,
  UiField,
  UiInput,
  UiRadio,
  RoleGate,
  type RadioOption,
} from '@ui-kit';
import {
  repo,
  PROTOCOL_OPTIONS,
  type ProtocolType,
  type DeviceDraft,
} from '../mock/mock-data';
import { session } from '../store/session';

const router = useRouter();

/** 步骤标题。 */
const STEPS = ['基础信息', '连接参数', '采集策略', '确认提交'] as const;

/** 协议 → 连接字段定义（随协议整块替换）。 */
interface ProtoField {
  /** 字段键 */
  key: string;
  /** 字段标签 */
  label: string;
  /** 输入类型 */
  kind: 'text' | 'number';
  /** 占位符 */
  placeholder?: string;
  /** 行内提示 */
  hint?: string;
}

/** 各协议的连接字段（顺序即表单顺序）。 */
const PROTO_FIELDS: Readonly<Record<ProtocolType, readonly ProtoField[]>> = Object.freeze({
  'modbus-tcp': [
    { key: 'host', label: '主站 IP', kind: 'text', placeholder: '192.168.10.31' },
    { key: 'port', label: '端口', kind: 'number', placeholder: '502' },
    { key: 'slaveId', label: '从站地址', kind: 'number', placeholder: '1' },
  ],
  'modbus-rtu': [
    { key: 'com', label: '串口', kind: 'text', placeholder: 'COM3' },
    { key: 'baud', label: '波特率', kind: 'number', placeholder: '9600' },
    { key: 'parity', label: '校验位', kind: 'text', placeholder: '8E1（数据位/校验/停止位）' },
    { key: 'slaveId', label: '从站地址', kind: 'number', placeholder: '1' },
  ],
  'opc-ua': [
    { key: 'endpoint', label: '终端地址', kind: 'text', placeholder: 'opc.tcp://192.168.10.31:4840' },
    { key: 'securityMode', label: '安全模式', kind: 'text', placeholder: 'None / Sign / Sign&Encrypt' },
    { key: 'username', label: '用户名（可选）', kind: 'text', placeholder: '留空则匿名' },
    { key: 'password', label: '密码（可选）', kind: 'text', placeholder: '留空则匿名' },
  ],
  s7: [
    { key: 'host', label: 'PLC IP', kind: 'text', placeholder: '192.168.10.31' },
    { key: 'rack', label: '机架', kind: 'number', placeholder: '0' },
    { key: 'slot', label: '槽号', kind: 'number', placeholder: '1' },
  ],
  mc: [
    { key: 'host', label: 'PLC IP', kind: 'text', placeholder: '192.168.10.31' },
    { key: 'port', label: '端口', kind: 'number', placeholder: '5007' },
    { key: 'netType', label: '网络类型', kind: 'text', placeholder: 'TCP / UDP' },
  ],
  http: [
    { key: 'url', label: '接口地址', kind: 'text', placeholder: 'http://host/api/metrics' },
    { key: 'method', label: '方法', kind: 'text', placeholder: 'GET / POST' },
  ],
  mqtt: [
    { key: 'broker', label: 'Broker', kind: 'text', placeholder: 'tcp://broker:1883' },
    { key: 'topic', label: '主题', kind: 'text', placeholder: 'plant/line1/metrics' },
    { key: 'clientId', label: '客户端 ID', kind: 'text', placeholder: 'gw-line1-01' },
  ],
});

/** 连接摘要拼接（按协议把字段拼成可读串，落库用）。 */
function summarize(proto: ProtocolType, c: Record<string, string>): string {
  const v = (k: string): string => (c[k] ?? '').trim();
  switch (proto) {
    case 'modbus-tcp':
      return `${v('host')}:${v('port')} · 从站 ${v('slaveId')}`;
    case 'modbus-rtu':
      return `${v('com')} · ${v('baud')} ${v('parity')} · 从站 ${v('slaveId')}`;
    case 'opc-ua':
      return v('endpoint');
    case 's7':
      return `${v('host')} · 机架 ${v('rack')} 槽 ${v('slot')}`;
    case 'mc':
      return `${v('host')}:${v('port')} · ${v('netType')}`;
    case 'http':
      return `${v('method')} ${v('url')}`;
    case 'mqtt':
      return `${v('broker')} · ${v('topic')}`;
    default:
      return '';
  }
}

/** 协议单选选项（带后果说明）。 */
const protocolRadioOptions = computed<readonly RadioOption[]>(() =>
  PROTOCOL_OPTIONS.map((p) => ({
    value: p.value,
    label: p.label,
    desc: '选择后将进入对应的连接参数录入',
  })),
);

/** 是否可写（工程师及以上）。 */
const canWrite = computed(() => session.state.role === 'admin' || session.state.role === 'engineer');

// ---------------------------------------------------------------------------
// 表单状态
// ---------------------------------------------------------------------------

const currentStep = ref(1);
const name = ref('');
const protocol = ref<ProtocolType | ''>('');
const conn = reactive<Record<string, string>>({});
const intervalMs = ref('1000');
const timeoutMs = ref('3000');
const retryTimes = ref('3');

/** 交互标记（控制错误提示是否显示，避免一打开就标红）。 */
const nameTouched = ref(false);
const connTouched = ref(false);
const policyTouched = ref(false);

/** 协议中文名。 */
const protocolLabel = computed(
  () => PROTOCOL_OPTIONS.find((p) => p.value === protocol.value)?.label ?? '未选择',
);

/** 当前协议的连接字段。 */
const currentProtoFields = computed<readonly ProtoField[]>(() =>
  protocol.value ? PROTO_FIELDS[protocol.value] : [],
);

/** 连接摘要（实时）。 */
const connectionSummary = computed(() =>
  protocol.value ? summarize(protocol.value, conn) : '',
);

/** 协议切换时清空已录入的连接字段，避免字段串台。 */
watch(protocol, () => {
  for (const key of Object.keys(conn)) {
    delete conn[key];
  }
  connTouched.value = false;
});

// ---------------------------------------------------------------------------
// 逐步校验（实时驱动「下一步」可用性）
// ---------------------------------------------------------------------------

/** 步骤 1 校验：名称 + 协议。 */
const step1Valid = computed(() => name.value.trim().length > 0 && protocol.value !== '');

/** 单连接字段校验：必填且数值字段需可解析为有限数。 */
function isFieldValid(field: ProtoField): boolean {
  const raw = (conn[field.key] ?? '').trim();
  if (raw.length === 0) {
    return false;
  }
  if (field.kind === 'number') {
    const n = Number(raw);
    return Number.isFinite(n) && n > 0;
  }
  return true;
}

/** 步骤 2 校验：所有字段有效。 */
const step2Valid = computed(() =>
  currentProtoFields.value.length > 0 && currentProtoFields.value.every(isFieldValid),
);

/** 数值策略校验。 */
const intervalValid = computed(() => {
  const n = Number(intervalMs.value);
  return Number.isFinite(n) && n >= 50;
});
const timeoutValid = computed(() => {
  const n = Number(timeoutMs.value);
  return Number.isFinite(n) && n >= 100;
});
const retryValid = computed(() => {
  const n = Number(retryTimes.value);
  return Number.isFinite(n) && n >= 0;
});
const step3Valid = computed(() => intervalValid.value && timeoutValid.value && retryValid.value);

/** 当前步是否合法（用于「下一步」/「提交」禁用态）。 */
const stepValid = computed(() => {
  if (currentStep.value === 1) {
    return step1Valid.value;
  }
  if (currentStep.value === 2) {
    return step2Valid.value;
  }
  if (currentStep.value === 3) {
    return step3Valid.value;
  }
  return step1Valid.value && step2Valid.value && step3Valid.value;
});

// ---------------------------------------------------------------------------
// 导航
// ---------------------------------------------------------------------------

/** 下一步。 */
function next(): void {
  if (currentStep.value === 1) {
    nameTouched.value = true;
  }
  if (currentStep.value === 2) {
    connTouched.value = true;
  }
  if (currentStep.value === 3) {
    policyTouched.value = true;
  }
  if (stepValid.value && currentStep.value < 4) {
    currentStep.value += 1;
  }
}

/** 上一步。 */
function prev(): void {
  if (currentStep.value > 1) {
    currentStep.value -= 1;
  }
}

/** 提交后创建的设备名。 */
const createdName = ref('');

/** 提交设备。 */
function submit(): void {
  if (!stepValid.value || !protocol.value) {
    return;
  }
  const draft: DeviceDraft = {
    name: name.value,
    protocol: protocol.value,
    connectionSummary: connectionSummary.value,
    intervalMs: Number(intervalMs.value),
    timeoutMs: Number(timeoutMs.value),
    retryTimes: Number(retryTimes.value),
    actor: session.state.displayName,
  };
  const created = repo.createDevice(draft);
  createdName.value = created.name;
  currentStep.value = 4;
}

/** 重置向导（继续新增）。 */
function resetWizard(): void {
  currentStep.value = 1;
  name.value = '';
  protocol.value = '';
  for (const key of Object.keys(conn)) {
    delete conn[key];
  }
  intervalMs.value = '1000';
  timeoutMs.value = '3000';
  retryTimes.value = '3';
  nameTouched.value = false;
  connTouched.value = false;
  policyTouched.value = false;
  createdName.value = '';
}

/** 跳转。 */
function go(name: string): void {
  void router.push({ name });
}
</script>

<style scoped>
.dv-steps {
  display: flex;
  gap: 8px;
  list-style: none;
  margin: 0;
  padding: 14px 16px;
  flex-wrap: wrap;
}
.dv-steps__item {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 6px 14px;
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  font-size: var(--fs-table);
  color: var(--text-3);
}
.dv-steps__item.is-on {
  border-color: var(--brand);
  background: var(--brand-subtle);
  color: var(--brand);
  font-weight: 600;
}
.dv-steps__item.is-done {
  border-color: var(--ok-border);
  color: var(--ok-fg);
}
.dv-steps__num {
  width: 20px;
  height: 20px;
  border-radius: 50%;
  background: var(--divider);
  color: var(--text-2);
  display: inline-flex;
  align-items: center;
  justify-content: center;
  font-size: 12px;
  font-weight: 600;
}
.dv-steps__item.is-on .dv-steps__num {
  background: var(--brand);
  color: #fff;
}
.dv-steps__item.is-done .dv-steps__num {
  background: var(--ok);
  color: #fff;
}
.dv-form {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: 14px 16px;
  max-width: 880px;
}
.dv-form--3 {
  grid-template-columns: repeat(3, minmax(0, 1fr));
}
.dv-suffix {
  font-size: var(--fs-caption);
  color: var(--text-3);
}
.dv-foot {
  display: flex;
  align-items: center;
  gap: 16px;
}
.dv-foot__ops {
  display: flex;
  gap: 8px;
}
.dv-foot__hint {
  font-size: var(--fs-caption);
  color: var(--text-3);
}
</style>
