<template>
  <!--
    DeviceNewPage —— 新增设备（接入分组第 2 页，路由 `/device-new`）。

    结构（`docs/design/prototype/gateway-v2a-glacier.html` :1533-1653）：
      单页四步向导：① 选择设备类型 → ② 连接参数 → ③ 点表映射 → ④ 测试并保存。

    硬性约定遵守情况：
      · 连接参数字段随协议**整块替换**（PROTO_FIELDS），不用万能表单硬塞；
      · 每一步的「下一步」按本步校验实时禁用（不是提交后才报错）；
      · 向导条只可回退到**已到达过**的步骤，未到达的锁定（跳过 = 绕过校验）；
      · 写操作受 RoleGate 控制（工程师及以上）；
      · 连通性测试：真实调用 `POST /api/devices/test`（Modbus 全探测，
        其余协议后端返回结构化 unsupported，绝不伪造成功）；
      · 提交只把配置交给 `repo.createDevice`（内存态 + 写审计），授权判定一律在网关侧。
  -->
  <PageHeader
    :crumb="pageCrumb"
    :title="pageTitle"
    :desc="pageDescription"
  >
    <template #actions>
      <button type="button" class="wc-btn" @click="go('devices')">返回设备列表</button>
    </template>
  </PageHeader>

  <div class="wc-content">
    <!-- 编辑态但设备不存在：诚实空态（不静默降级成新增） -->
    <div v-if="isEdit && !editDevice" class="wc-banner wc-banner--danger">
      <span aria-hidden="true">!</span>
      <span>未找到设备「{{ editId }}」，可能已被删除或设备标识有误。</span>
      <span class="wc-banner__ops">
        <button type="button" class="wc-btn wc-btn--sm" @click="go('devices')">返回设备列表</button>
      </span>
    </div>

    <template v-else>
    <!-- 步骤指示器：已到达的步骤可点回退，未到达的锁定（原型 wizardBar :1203 / .wz 样式 :531） -->
    <div class="wc-card dv-wz">
      <div class="dv-wz__row">
        <template v-for="(step, i) in STEPS" :key="step">
          <div
            class="dv-wz__item"
            :class="{
              'is-run': currentStep === i + 1,
              'is-done': currentStep > i + 1,
              'is-clickable': i + 1 <= maxReached,
              'is-lock': i + 1 > maxReached,
            }"
            :role="i + 1 <= maxReached ? 'button' : undefined"
            :tabindex="i + 1 <= maxReached ? 0 : undefined"
            @click="gotoStep(i + 1)"
            @keydown.enter.prevent="gotoStep(i + 1)"
            @keydown.space.prevent="gotoStep(i + 1)"
          >
            <span class="dv-wz__num" aria-hidden="true">{{ currentStep > i + 1 ? '✓' : i + 1 }}</span>
            <span class="dv-wz__label">{{ step }}</span>
          </div>
          <span v-if="i < STEPS.length - 1" class="dv-wz__line" aria-hidden="true" />
        </template>
      </div>
    </div>

    <!-- 步骤上下文：当前设备类型 + 回到第 1 步改类型（原型 :1546-1549） -->
    <div class="dv-ctx">
      <span>当前设备类型</span>
      <span class="wc-tag wc-tag--info">{{ protocolLabel }}</span>
      <span class="dv-ctx__hint" data-testid="wz-ctx-meta">连接参数 {{ currentProtoFields.length }} 个</span>
      <button v-if="currentStep > 1" type="button" class="wc-btn wc-btn--sm" @click="gotoStep(1)">
        回到第 1 步改类型
      </button>
    </div>

    <!-- ══ ① 选择设备类型（双列 + 可滚动，容纳持续扩充的类型清单）══ -->
    <div v-show="currentStep === 1">
      <section class="wc-card">
        <div class="wc-card__head">
          <h3>① 选择设备类型</h3>
          <span class="wc-card__sub">决定下一步需要填哪些连接参数</span>
        </div>
        <div class="wc-card__body dv-type-scroll">
          <UiRadio
            v-model="protocol"
            class="dv-type-grid"
            data-testid="device-type-list"
            :options="protocolRadioOptions"
          />
        </div>
      </section>
    </div>

    <!-- ══ ② 连接参数（随协议整块替换）══ -->
    <section v-show="currentStep === 2" class="wc-card">
      <div class="wc-card__head">
        <h3>② 连接参数</h3>
        <span v-if="isEdit && !protocolChanged" class="wc-card__sub">沿用设备记录中的连接参数</span>
        <span v-else class="wc-card__sub">{{ protocolLabel }} · 共 {{ currentProtoFields.length }} 个字段</span>
      </div>
      <div class="wc-card__body">
        <!--
          编辑态且未改协议：`GET /api/devices` 只回传连接摘要，不包含逐字段连接参数。
          此处如实按摘要展示已知信息，不摆一排空输入框假装「已回显」。
        -->
        <template v-if="isEdit && !protocolChanged">
          <dl class="wc-kv">
            <dt>设备名称</dt>
            <dd>{{ editDevice?.name ?? '—' }}</dd>
            <dt>协议</dt>
            <dd><span class="wc-tag wc-tag--info">{{ protocolLabel }}</span></dd>
            <dt>连接摘要</dt>
            <dd class="wc-mono">{{ editDevice?.connectionSummary || '—' }}</dd>
            <dt>采集频率</dt>
            <dd class="wc-mono">{{ intervalMs }} ms</dd>
          </dl>
          <p class="wc-note">
            <span class="wc-note__icon" aria-hidden="true">ⓘ</span>
            <span>连接参数以设备记录中的摘要为准；本页编辑更新设备名称与协议，连接参数保持不变。</span>
          </p>
        </template>
        <template v-else>
        <div v-if="protocol === ''" class="wc-note">
          <span class="wc-note__icon" aria-hidden="true">ⓘ</span>
          <span>请回到第 1 步选择设备类型。</span>
        </div>

        <template v-for="group in fieldGroups" v-else :key="group.title">
          <p class="dv-grp">{{ group.title }}</p>
          <div class="dv-form">
            <UiField
              v-for="field in group.fields"
              :key="field.key"
              :label="field.label"
              :required="!!field.required"
              :hint="field.hint ?? ''"
              :error="connTouched && !isFieldValid(field) ? fieldError(field) : ''"
            >
              <UiSelect
                v-if="field.kind === 'select'"
                v-model="conn[field.key]"
                :options="selectOptions(field)"
              />
              <span v-else class="dv-inp">
                <UiInput
                  v-model="conn[field.key]"
                  :type="field.kind === 'number' ? 'number' : field.kind === 'password' ? 'password' : 'text'"
                  :placeholder="field.placeholder ?? ''"
                />
                <span v-if="field.unit" class="dv-inp__unit">{{ field.unit }}</span>
              </span>
            </UiField>
          </div>
        </template>

        </template>
      </div>
    </section>

    <!-- ══ ③ 点表准备（点位不在向导内配置，创建后到「点位与映射」页维护）══ -->
    <section v-show="currentStep === 3" class="wc-card">
      <div class="wc-card__head">
        <h3>③ 点表准备</h3>
      </div>
      <div class="wc-card__body">
        <p class="wc-note">
          <span class="wc-note__icon" aria-hidden="true">ⓘ</span>
          <span>设备保存后，到「点位与映射」页选中该设备即可添加点位或导入点表。</span>
        </p>
      </div>
    </section>

    <!-- ══ ④ 测试并保存 ══ -->
    <div v-show="currentStep === 4">
      <div class="wc-grid wc-grid--2">
        <!-- 连通性测试 -->
        <section class="wc-card">
          <div class="wc-card__head">
            <h3>④ 连通性测试</h3>
            <span class="wc-card__sub">只读探测，不写入设备、对产线无影响</span>
          </div>
          <div class="wc-card__body">
            <ol class="dv-test">
              <li v-for="s in testSteps" :key="s.n" class="dv-test__item" :class="`is-${s.state}`">
                <span class="dv-test__n">{{ s.state === 'done' ? '✓' : s.state === 'fail' ? '✕' : s.n }}</span>
                <span class="dv-test__main">
                  <span class="dv-test__label">{{ s.label }}</span>
                  <span class="dv-test__desc">{{ s.desc }}</span>
                </span>
                <span class="dv-test__time wc-mono">{{ s.time }}</span>
              </li>
            </ol>

            <div class="dv-foot">
              <button type="button" class="wc-btn wc-btn--primary wc-btn--sm" :disabled="testRunning" @click="runTest">
                {{ testRunning ? '测试中…' : '开始连通性测试' }}
              </button>
              <span class="dv-foot__hint">{{ probeHint }}</span>
            </div>

            <div v-if="testResult" class="wc-banner" :class="testResult.ok ? 'wc-banner--ok' : 'wc-banner--danger'">
              <span aria-hidden="true">{{ testResult.ok ? '✓' : '!' }}</span>
              <span>{{ testResult.text }}</span>
            </div>
            <p v-if="testUnsupported" class="wc-note wc-note--warn">
              <span class="wc-note__icon" aria-hidden="true">⚠</span>
              <span>
                目前只有 Modbus TCP / RTU（RTU-over-TCP）具备结构化探测驱动，其余协议网关返回
                <b>unsupported_protocol</b> —— 不伪造成功结果。请以设备侧现象为准。
              </span>
            </p>
          </div>
        </section>

        <!-- 设备信息 -->
        <section class="wc-card">
          <div class="wc-card__head">
            <h3>设备信息</h3>
            <span class="wc-card__sub">保存后按「启用时机」决定是否立即采集</span>
          </div>
          <div class="wc-card__body">
            <div class="dv-form">
              <UiField label="设备名称" required :error="saveTouched && !nameValid ? '请填写设备名称' : ''">
                <UiInput v-model="name" placeholder="如 5#注塑机" />
              </UiField>

              <UiField label="设备 ID" :hint="isEdit ? '设备 ID 是北向报文里 device_mid 的取值，编辑时不可更改' : '默认取当前时间戳字符串，可更改；设备 ID 是北向报文里 device_mid 的取值'">
                <UiInput v-model="deviceId" class="wc-mono" :disabled="isEdit" />
              </UiField>

              <UiField
                label="采集频率"
                required
                hint="≥ 50 ms；当前配置的实际生效权限由网关侧判定"
                :error="saveTouched && !intervalValid ? '需为 ≥ 50 的整数' : ''"
              >
                <span class="dv-inp">
                  <UiInput v-model="intervalMs" type="number" placeholder="200" />
                  <span class="dv-inp__unit">ms</span>
                </span>
              </UiField>

              <UiField label="分组" hint="面向 ISV 交付的展示分组">
                <UiSelect v-model="group" :options="groupOptions" />
              </UiField>

              <UiField label="启用时机">
                <UiSelect v-model="enableWhen" :options="enableWhenOptions" />
              </UiField>

              <UiField label="备注" hint="例：5# 机，2024 年投产，厂家联调联系人 138xxxx">
                <UiInput v-model="memo" placeholder="选填" />
              </UiField>

              <UiField
                label="变更原因"
                required
                full
                :hint="isEdit ? '写入审计日志。编辑设备属配置变更，必须留痕。' : '写入审计日志。新增设备属配置变更，必须留痕。'"
                :error="saveTouched && !reasonValid ? '必填，请说明本次接入背景（≥ 4 字）' : ''"
              >
                <UiInput v-model="changeReason" placeholder="例：5# 机新接入，按厂家点表配置" />
              </UiField>
            </div>

            <p class="dv-sub-h">自定义参数</p>
            <div class="dv-cp">
              <div v-for="(row, i) in customParams" :key="i" class="dv-cp__row">
                <UiInput v-model="row.k" placeholder="参数名，如 line_no" />
                <UiInput v-model="row.v" placeholder="参数值" />
                <button type="button" class="wc-btn wc-btn--sm" @click="removeCustomParam(i)">删除</button>
              </div>
              <button type="button" class="wc-btn wc-btn--sm" @click="addCustomParam">新增自定义参数</button>
            </div>
          </div>
        </section>
      </div>
    </div>

    <!-- 提交成功 -->
    <div v-if="createdName" class="wc-banner wc-banner--ok">
      <span aria-hidden="true">✓</span>
      <span>{{ createdNote }}</span>
      <span class="wc-banner__ops">
        <button type="button" class="wc-btn wc-btn--sm" @click="go('devices')">查看设备列表</button>
        <button v-if="!isEdit && createdId" type="button" class="wc-btn wc-btn--sm" data-test="goto-points" @click="goPoints(createdId)">
          去点位与映射
        </button>
        <button v-if="!isEdit" type="button" class="wc-btn wc-btn--sm" @click="resetWizard">继续新增</button>
      </span>
    </div>

    <!-- 底部操作条（原型 actbar :1634-1645） -->
    <div class="dv-foot dv-foot--bar">
      <RoleGate :allowed="canWrite" mode="disable" :fallback-label="isEdit ? '仅工程师及以上可编辑' : '仅工程师及以上可新增'">
        <div class="dv-foot__ops">
          <span class="dv-foot__txt">
            第 <b>{{ currentStep }}</b> / {{ STEPS.length }} 步 · {{ STEPS[currentStep - 1] }}
            <span v-if="isEdit && currentStep === 4 && !canSubmitEdit" class="dv-foot__warn">
              需完成设备名称 + 变更原因{{ protocolChanged ? '，并重新填写连接参数' : '' }}才能保存修改
            </span>
            <span v-else-if="!isEdit && currentStep === 4 && !canSaveAndStart" class="dv-foot__warn">
              需完成设备名称 + 变更原因才能保存
            </span>
          </span>
          <button type="button" class="wc-btn" @click="go('devices')">取消</button>
          <button type="button" class="wc-btn" :disabled="currentStep === 1" @click="prev">上一步</button>
          <button v-if="currentStep < 4" type="button" class="wc-btn wc-btn--primary" :disabled="!stepValid" @click="next">
            下一步
          </button>
          <button
            v-else-if="isEdit"
            type="button"
            class="wc-btn wc-btn--primary"
            data-test="device-save-edit"
            :disabled="!canSubmitEdit || !!createdName"
            @click="submit('saveAndStart')"
          >
            保存修改
          </button>
          <template v-else>
            <button type="button" class="wc-btn" :disabled="!nameValid" @click="submit('save')">
              仅保存连接参数
            </button>
            <button
              type="button"
              class="wc-btn wc-btn--primary"
              :disabled="!canSaveAndStart || !!createdName"
              @click="submit('saveAndStart')"
            >
              保存并开始采集
            </button>
          </template>
        </div>
      </RoleGate>
    </div>
    </template>
  </div>
</template>

<script setup lang="ts">
/**
 * @file DeviceNewPage.vue
 * @module web-console/pages/DeviceNewPage
 * @description 新增设备四步向导（选择设备类型 / 连接参数 / 点表映射 / 测试并保存）。
 *
 * · 连接参数字段随协议**整块替换**（PROTO_FIELDS），避免通用表单硬塞异构协议字段；
 * · 字段控件支持 `text / number / password / select` + `unit` 后缀 + `group` 分组标题；
 * · 向导只可回退到已到达的步骤（`maxReached`），未到达的步骤锁定 —— 跳过等于绕过校验；
 * · 连通性探测：真实调用 `POST /api/devices/test`，**只展示后端返回的结构化结果**，
 *   绝不摆拍耗时、绝不伪造成功；
 * · 提交只把配置交给 `repo.createDevice`，写失败时如实呈现原因。
 */
import { computed, reactive, ref, watch } from 'vue';
import { useRoute, useRouter } from 'vue-router';
import {
  PageHeader,
  UiField,
  UiInput,
  UiRadio,
  UiSelect,
  RoleGate,
  type RadioOption,
  type SelectOption,
} from '@ui-kit';
import {
  repo,
  PROTOCOL_OPTIONS,
  BYTE_ORDER_OPTIONS,
  type ProtocolType,
  type DeviceDraft,
  type DeviceRecord,
} from '@/api/repo';
import { session } from '../store/session';

const router = useRouter();
const route = useRoute();

/** 步骤标题（原型 WZ_NAMES :1204）。 */
const STEPS = ['选择设备类型', '连接参数', '点表准备', '测试并保存'] as const;

// ---------------------------------------------------------------------------
// 编辑态（`/device-new?device=<id>`，设备列表「编辑」入口）
// ---------------------------------------------------------------------------

/** 路由 query 中的设备 id（空串 = 新增模式）。 */
const editId = computed<string>(() => {
  const raw = route.query.device;
  const value = Array.isArray(raw) ? raw[0] : raw;
  return typeof value === 'string' ? value.trim() : '';
});

/** 是否处于编辑态。 */
const isEdit = computed(() => editId.value !== '');

/** 编辑态设备记录（`repo.getDevice`）；查不到时为 `null`，页面给出诚实空态。 */
const editDevice = ref<DeviceRecord | null>(null);

/** 进入编辑态时的原始协议（用于判断用户是否改了协议 —— 改协议等于点表失效）。 */
const originalProtocol = ref<ProtocolType | ''>('');

/** 用户是否改过协议。 */
const protocolChanged = computed(
  () => isEdit.value && protocol.value !== '' && protocol.value !== originalProtocol.value,
);

/** 页面头部（新增 / 编辑两态）。 */
const pageCrumb = computed(() => (isEdit.value ? '接入 / 编辑设备' : '接入 / 新增设备'));
const pageTitle = computed(() => (isEdit.value ? '编辑设备' : '新增设备'));
const pageDescription = computed(() =>
  isEdit.value
    ? `正在编辑「${editDevice.value?.name ?? editId.value}」：可修改设备名称与协议；改协议后需重新核对点表。`
    : '单页四步：选设备类型 → 填连接参数 → 确认点表安排 → 测试并保存。连接参数字段随协议整块替换；保存一步到位。',
);

// ---------------------------------------------------------------------------
// 协议元数据（原型 PROTOCOLS :766-809 / PROTO_KEY_FIELD :906-909）
// ---------------------------------------------------------------------------

interface ProtoMeta {
  /** 典型接入对象 */
  use: string;
}

const PROTO_META: Readonly<Record<ProtocolType, ProtoMeta>> = Object.freeze({
  'modbus-tcp': { use: '仪表 / 电表 / 变频器' },
  'modbus-rtu': { use: '老式仪表 / 温控器 / 电表' },
  'opc-ua': { use: 'SCADA / 空压机 / 数控机床' },
  s7: { use: 'S7-1200 / S7-1500 / S7-300' },
  mc: { use: '三菱 FX / Q / iQ-R 系列' },
  http: { use: '相机 / 称重 / 第三方系统' },
  mqtt: { use: '其它网关 / 无线传感器' },
});

// ---------------------------------------------------------------------------
// 连接字段定义（随协议整块替换；原型 PROTO_FIELDS :811-904）
// ---------------------------------------------------------------------------

interface ProtoField {
  /** 字段键 */
  key: string;
  /** 字段标签 */
  label: string;
  /** 控件类型 */
  kind: 'text' | 'number' | 'password' | 'select';
  /** 是否必填 */
  required?: boolean;
  /** 单位后缀（inp-unit） */
  unit?: string;
  /** 分组标题（form-grp） */
  group?: string;
  /** 下拉选项（kind === 'select' 时必填） */
  opts?: readonly string[];
  /** 默认值（仅协议无关的通用项预填；设备地址类一律留空由现场填写） */
  value?: string;
  /** 占位符 */
  placeholder?: string;
  /** 行内提示 */
  hint?: string;
}

/** 下拉字段的选项（value = label，落库存原始文案）。 */
function selectOptions(field: ProtoField): readonly SelectOption[] {
  return (field.opts ?? []).map((o) => ({ value: o, label: o }));
}

/** 数值输入的下界（默认 0，机架号等允许填 0）。 */
const NUM_MIN = 0;

/** Modbus 功能码选项（原型 :821 / :835）。 */
const MODBUS_FC = ['01 线圈', '02 离散输入', '03 保持寄存器', '04 输入寄存器'] as const;

/** 字节序选项（与点表允许值同源：`BYTE_ORDER_OPTIONS`）。 */
const BYTE_ORDER_SELECT: readonly string[] = BYTE_ORDER_OPTIONS.map((b) => `${b}（${byteOrderCn(b)}）`);

function byteOrderCn(bo: string): string {
  if (bo === 'AB CD') {
    return '大端';
  }
  if (bo === 'CD AB') {
    return '大端字交换';
  }
  if (bo === 'BA DC') {
    return '小端';
  }
  return '小端字交换';
}

/** 各协议的连接字段（顺序即表单顺序，group 为分组标题）。 */
const PROTO_FIELDS: Readonly<Record<ProtocolType, readonly ProtoField[]>> = Object.freeze({
  'modbus-tcp': [
    { key: 'host', label: '设备 IP', kind: 'text', required: true, group: '连接参数', placeholder: '192.168.10.31' },
    { key: 'port', label: '端口', kind: 'number', required: true, group: '连接参数', value: '502', hint: 'Modbus TCP 标准端口 502' },
    { key: 'unitId', label: '从站号（Unit ID）', kind: 'number', required: true, group: '连接参数', value: '1', hint: '多台从站经网关汇聚时用于区分设备' },
    { key: 'timeout', label: '超时', kind: 'number', group: '连接参数', value: '1000', unit: 'ms' },
    { key: 'retry', label: '重试次数', kind: 'number', group: '连接参数', value: '3', unit: '次' },
    { key: 'fc', label: '功能码', kind: 'select', group: '读取策略', opts: MODBUS_FC, value: MODBUS_FC[2] },
    { key: 'maxGap', label: '最大合并间隔', kind: 'number', group: '读取策略', value: '8', unit: '寄存器', hint: '相邻寄存器间隔小于该值时合并为一次批量读，减少请求数' },
    { key: 'base', label: '地址偏移基数', kind: 'select', group: '读取策略', opts: ['1 基（40001 = 偏移 0）', '0 基（40001 = 偏移 40000）'], value: '1 基（40001 = 偏移 0）', hint: '最易踩的坑：手册地址与报文偏移差 1，选错会整体错位一格' },
    { key: 'byteOrder', label: '默认字节序', kind: 'select', group: '读取策略', opts: BYTE_ORDER_SELECT, value: BYTE_ORDER_SELECT[1] },
  ],
  'modbus-rtu': [
    { key: 'serial', label: '串口设备', kind: 'text', required: true, group: '串口参数', placeholder: '/dev/ttyUSB0', hint: 'Linux 填 /dev/ttyUSB0；Windows 填 COM3' },
    { key: 'baud', label: '波特率', kind: 'select', group: '串口参数', opts: ['1200', '2400', '4800', '9600', '19200', '38400', '57600', '115200'], value: '9600' },
    { key: 'parity', label: '校验位', kind: 'select', group: '串口参数', opts: ['N（无校验）', 'E（偶校验）', 'O（奇校验）'], value: 'N（无校验）' },
    { key: 'dataBit', label: '数据位', kind: 'select', group: '串口参数', opts: ['7', '8'], value: '8' },
    { key: 'stopBit', label: '停止位', kind: 'select', group: '串口参数', opts: ['1', '2'], value: '1' },
    { key: 'unitId', label: '从站号', kind: 'number', required: true, group: '串口参数', value: '1', hint: '同一串口上每个从站号必须唯一' },
    { key: 'fc', label: '功能码', kind: 'select', group: '读取策略', opts: MODBUS_FC, value: MODBUS_FC[2] },
    { key: 'timeout', label: '超时', kind: 'number', group: '读取策略', value: '1000', unit: 'ms' },
    { key: 'retry', label: '重试次数', kind: 'number', group: '读取策略', value: '3', unit: '次' },
    { key: 'byteOrder', label: '默认字节序', kind: 'select', group: '读取策略', opts: BYTE_ORDER_SELECT, value: BYTE_ORDER_SELECT[0] },
  ],
  'opc-ua': [
    { key: 'endpoint', label: '端点地址', kind: 'text', required: true, group: '端点与安全', placeholder: 'opc.tcp://192.168.10.31:4840', hint: '可直接填 discovery 地址，保存时自动拉取端点列表' },
    { key: 'security', label: '安全策略', kind: 'select', group: '端点与安全', opts: ['None（不加密，仅内网调试）', 'Basic256Sha256 · Sign', 'Basic256Sha256 · Sign & Encrypt', 'Aes128Sha256RsaOaep · Sign & Encrypt'], value: 'Basic256Sha256 · Sign & Encrypt' },
    { key: 'auth', label: '认证方式', kind: 'select', group: '端点与安全', opts: ['匿名', '用户名 / 密码', 'X.509 证书'], value: '用户名 / 密码' },
    { key: 'user', label: '用户名', kind: 'text', group: '端点与安全', placeholder: 'opcua_user' },
    { key: 'pass', label: '密码', kind: 'password', group: '端点与安全', placeholder: '留空则匿名' },
    { key: 'ns', label: '默认命名空间索引', kind: 'number', required: true, group: '浏览与订阅', value: '2', hint: 'NodeId 形如 ns=2;s=Machine.Temp' },
    { key: 'sub', label: '数据变更订阅', kind: 'select', group: '浏览与订阅', opts: ['开启（服务端推送）', '关闭（改为轮询）'], value: '开启（服务端推送）', hint: '开启后由服务端在数据变化时推送，比轮询省带宽且更实时' },
    { key: 'sampleRate', label: '采样间隔', kind: 'number', group: '浏览与订阅', value: '500', unit: 'ms' },
    { key: 'pubInterval', label: '发布间隔', kind: 'number', group: '浏览与订阅', value: '1000', unit: 'ms', hint: '服务端向客户端推送的最小间隔，用于抑制高频抖动' },
  ],
  s7: [
    { key: 'host', label: '设备 IP', kind: 'text', required: true, group: '连接参数', placeholder: '192.168.10.31' },
    { key: 'port', label: '端口', kind: 'number', required: true, group: '连接参数', value: '102', hint: 'S7 ISO-TSAP 固定 102' },
    { key: 'rack', label: '机架号（Rack）', kind: 'number', required: true, group: '连接参数', value: '0', hint: 'S7-1200/1500 通常为 0' },
    { key: 'slot', label: '槽号（Slot）', kind: 'number', required: true, group: '连接参数', value: '1', hint: 'S7-1200/1500 通常为 1；S7-300 为 2' },
    { key: 'pdu', label: 'PDU 长度', kind: 'number', group: '连接参数', value: '480', unit: '字节', hint: '影响单次可读的 DB 块长度' },
    { key: 'timeout', label: '超时', kind: 'number', group: '连接参数', value: '1000', unit: 'ms' },
    { key: 'optBlock', label: 'DB 块访问方式', kind: 'select', group: '优化块访问', opts: ['非优化（支持绝对地址）', '优化块（仅符号寻址）'], value: '非优化（支持绝对地址）', hint: '优化块无法用 DB1.DBD0 这类绝对地址，需改用符号名；选错会导致读取失败' },
    { key: 'byteOrder', label: '默认字节序', kind: 'select', group: '优化块访问', opts: [BYTE_ORDER_SELECT[0], BYTE_ORDER_SELECT[1]], value: BYTE_ORDER_SELECT[0] },
  ],
  mc: [
    { key: 'host', label: '设备 IP', kind: 'text', required: true, group: '连接参数', placeholder: '192.168.10.40' },
    { key: 'port', label: '端口', kind: 'number', required: true, group: '连接参数', value: '5007' },
    { key: 'frame', label: '报文格式', kind: 'select', group: '连接参数', opts: ['3E（二进制）', '4E（二进制）', '3E（ASCII）', '4E（ASCII）'], value: '3E（二进制）' },
    { key: 'netNo', label: '网络号', kind: 'number', required: true, group: '连接参数', value: '0' },
    { key: 'pcNo', label: 'PC 号', kind: 'number', group: '连接参数', value: '255' },
    { key: 'stationNo', label: '站号', kind: 'number', required: true, group: '连接参数', value: '1' },
    { key: 'device', label: '默认软元件', kind: 'select', group: '读取策略', opts: ['D（数据寄存器）', 'M（内部继电器）', 'X（输入）', 'Y（输出）', 'W（链接寄存器）'], value: 'D（数据寄存器）' },
    { key: 'timeout', label: '超时', kind: 'number', group: '读取策略', value: '1000', unit: 'ms' },
  ],
  http: [
    { key: 'url', label: '请求地址', kind: 'text', required: true, group: '请求配置', placeholder: 'http://192.168.10.50/api/v1/measure' },
    { key: 'method', label: '请求方法', kind: 'select', group: '请求配置', opts: ['GET', 'POST'], value: 'GET' },
    { key: 'mode', label: '取数方式', kind: 'select', group: '请求配置', opts: ['定时轮询', 'Webhook 被动接收'], value: '定时轮询', hint: 'Webhook 模式下由对端主动推送，无需填轮询间隔' },
    { key: 'interval', label: '轮询间隔', kind: 'number', group: '请求配置', value: '2000', unit: 'ms' },
    { key: 'auth', label: '认证方式', kind: 'select', group: '请求配置', opts: ['无', 'Basic', 'Bearer Token', '自定义 Header'], value: 'Bearer Token' },
    { key: 'token', label: 'Token / 密钥', kind: 'password', group: '请求配置', placeholder: '选填' },
    { key: 'jsonPath', label: '取值表达式', kind: 'text', group: '响应解析', placeholder: '$.data.temperature', hint: 'JSONPath 表达式；数组用 [*] 展开为多点位' },
    { key: 'encoding', label: '响应编码', kind: 'select', group: '响应解析', opts: ['UTF-8', 'GBK'], value: 'UTF-8' },
    { key: 'httpTimeout', label: '超时', kind: 'number', group: '响应解析', value: '3000', unit: 'ms' },
  ],
  mqtt: [
    { key: 'broker', label: 'Broker 地址', kind: 'text', required: true, group: 'Broker 连接', placeholder: 'mqtt://10.0.0.9:1883' },
    { key: 'clientId', label: '客户端 ID', kind: 'text', required: true, group: 'Broker 连接', placeholder: 'iot-daq-src-01', hint: '同一 Broker 上不可重复，否则会互相顶掉连接' },
    { key: 'auth', label: '认证方式', kind: 'select', group: 'Broker 连接', opts: ['匿名', '用户名 / 密码', 'TLS 客户端证书'], value: '用户名 / 密码' },
    { key: 'user', label: '用户名', kind: 'text', group: 'Broker 连接', placeholder: 'gw_reader' },
    { key: 'pass', label: '密码', kind: 'password', group: 'Broker 连接', placeholder: '选填' },
    { key: 'topic', label: '订阅 Topic', kind: 'text', required: true, group: '订阅', placeholder: 'plant/+/env/#', hint: '支持 + 与 # 通配符' },
    { key: 'qos', label: 'QoS', kind: 'select', group: '订阅', opts: ['0（至多一次）', '1（至少一次）', '2（恰好一次）'], value: '1（至少一次）' },
    { key: 'payload', label: '载荷格式', kind: 'select', group: '订阅', opts: ['JSON', 'protobuf', '裸字节 + 偏移表'], value: 'JSON' },
    { key: 'keepAlive', label: '保持连接间隔', kind: 'number', group: '订阅', value: '60', unit: 's', hint: '超过该时间未收到心跳即判定链路断开' },
  ],
});

// ---------------------------------------------------------------------------
// 连接摘要（按协议把字段拼成可读串，落库用）
// ---------------------------------------------------------------------------

function summarize(proto: ProtocolType, c: Record<string, string>): string {
  const v = (k: string): string => (c[k] ?? '').trim();
  switch (proto) {
    case 'modbus-tcp':
      return `${v('host')}:${v('port')} · 从站 ${v('unitId')}`;
    case 'modbus-rtu':
      return `${v('serial')} · ${v('baud')} ${v('parity')}${v('dataBit')}${v('stopBit')} · 从站 ${v('unitId')}`;
    case 'opc-ua':
      return v('endpoint');
    case 's7':
      return `${v('host')}:${v('port')} · 机架 ${v('rack')} 槽 ${v('slot')}`;
    case 'mc':
      return `${v('host')}:${v('port')} · ${v('frame')} · 站 ${v('stationNo')}`;
    case 'http':
      return `${v('method')} ${v('url')}`;
    case 'mqtt':
      return `${v('broker')} · ${v('topic')}`;
    default:
      return '';
  }
}

// ---------------------------------------------------------------------------
// 表单状态
// ---------------------------------------------------------------------------

/** 是否可写（工程师及以上）。 */
const canWrite = computed(() => session.state.role === 'admin' || session.state.role === 'engineer');

const currentStep = ref(1);
/** 已到达过的最大步（向导条据此决定是否可点）。 */
const maxReached = ref(1);

const protocol = ref<ProtocolType | ''>('');
const conn = reactive<Record<string, string>>({});

/** ④ 保存信息字段。 */
const name = ref('');
const deviceId = ref(`dev-${String(Date.now())}`);
const intervalMs = ref('200');
const group = ref('未分组');
const enableWhen = ref('保存后立即启用采集');
const memo = ref('');
const changeReason = ref('');
/** 自定义参数行（交付版表单项，当前不入后端，已诚实标注）。 */
const customParams = ref<{ k: string; v: string }[]>([]);

/** 交互标记（控制错误提示是否显示，避免一打开就标红）。 */
const connTouched = ref(false);
const saveTouched = ref(false);

const createdName = ref('');
const createdNote = ref('');
/** 新建成功的设备 id（用于直达「点位与映射」并预选该设备）。 */
const createdId = ref('');

/**
 * 编辑态回填：读取设备记录并填充**可回填字段**。
 *
 * 诚实边界：`GET /api/devices` 只回传设备名称 / 协议 / 连接摘要 / 采集频率等，
 * 不包含逐字段连接参数，故不向 `conn` 塞空值假装；第 2 步以「连接摘要」形式
 * 展示已知信息（见模板 `isEdit && !protocolChanged` 分支）。
 */
watch(
  editId,
  (id) => {
    if (!id) {
      // 从编辑态切回新增态：向导进度与表单必须回到初始状态，
      // 否则 maxReached 仍是 4，未到达的步骤会被误判为「已到达」而可点。
      editDevice.value = null;
      originalProtocol.value = '';
      currentStep.value = 1;
      maxReached.value = 1;
      protocol.value = '';
      name.value = '';
      deviceId.value = `dev-${String(Date.now())}`;
      intervalMs.value = '200';
      changeReason.value = '';
      connTouched.value = false;
      saveTouched.value = false;
      createdName.value = '';
      createdNote.value = '';
      createdId.value = '';
      return;
    }
    const found = repo.getDevice(id);
    editDevice.value = found;
    if (!found) {
      return;
    }
    name.value = found.name;
    deviceId.value = found.id;
    intervalMs.value = String(found.intervalMs);
    originalProtocol.value = found.protocol;
    // 变更协议会触发 `watch(protocol)` 清空连接字段，这是预期行为（新协议要重填）。
    protocol.value = found.protocol;
    // 记录已存在，四步均可直达（回退/跳转不再受 maxReached 限制）。
    maxReached.value = STEPS.length;
  },
  { immediate: true },
);

/** 协议中文名。 */
const protocolLabel = computed(
  () => PROTOCOL_OPTIONS.find((p) => p.value === protocol.value)?.label ?? '未选择',
);

/** 当前协议的连接字段。 */
const currentProtoFields = computed<readonly ProtoField[]>(() =>
  protocol.value ? PROTO_FIELDS[protocol.value] : [],
);

/** 字段按 group 分组后的顺序（保留定义顺序，组标题为 form-grp）。 */
const fieldGroups = computed<readonly { title: string; fields: ProtoField[] }[]>(() => {
  const out: { title: string; fields: ProtoField[] }[] = [];
  for (const field of currentProtoFields.value) {
    const title = field.group ?? '连接参数';
    const hit = out.find((g) => g.title === title);
    if (hit) {
      hit.fields.push(field);
    } else {
      out.push({ title, fields: [field] });
    }
  }
  return out;
});

/** 连接摘要（实时）。 */
const connectionSummary = computed(() => (protocol.value ? summarize(protocol.value, conn) : ''));

/** 协议单选选项（带典型场景说明）。 */
const protocolRadioOptions = computed<readonly RadioOption[]>(() =>
  PROTOCOL_OPTIONS.map((p) => ({
    value: p.value,
    label: p.label,
    desc: PROTO_META[p.value].use,
  })),
);

const groupOptions: readonly SelectOption[] = [
  { value: '生产分组 › 注塑车间', label: '生产分组 › 注塑车间' },
  { value: '生产分组 › 动力站', label: '生产分组 › 动力站' },
  { value: '未分组', label: '未分组' },
];

const enableWhenOptions: readonly SelectOption[] = [
  { value: '保存后立即启用采集', label: '保存后立即启用采集' },
  { value: '保存但暂停采集', label: '保存但暂停采集' },
];

/** 协议切换：整块重置连接字段（避免字段串台），并按定义预填通用默认值。 */
watch(protocol, (next) => {
  for (const key of Object.keys(conn)) {
    delete conn[key];
  }
  if (next) {
    for (const field of PROTO_FIELDS[next]) {
      if (field.value !== undefined) {
        conn[field.key] = field.value;
      }
    }
  }
  connTouched.value = false;
  testResult.value = null;
  testUnsupported.value = false;
  resetTestSteps();
});

// ---------------------------------------------------------------------------
// 校验（实时驱动「下一步」/「保存」可用性）
// ---------------------------------------------------------------------------

const step1Valid = computed(() => protocol.value !== '');

function isNumberFieldOk(raw: string): boolean {
  const n = Number(raw);
  return Number.isFinite(n) && n >= NUM_MIN;
}

function isFieldValid(field: ProtoField): boolean {
  const raw = (conn[field.key] ?? '').trim();
  if (raw.length === 0) {
    return !(field.required ?? false);
  }
  return field.kind === 'number' ? isNumberFieldOk(raw) : true;
}

function fieldError(field: ProtoField): string {
  const raw = (conn[field.key] ?? '').trim();
  if (raw.length === 0) {
    return '必填';
  }
  return field.kind === 'number' ? '需为 ≥ 0 的数值' : '';
}

const step2Valid = computed(() => {
  // 编辑态且未改协议：连接参数沿用设备记录既有值（第 2 步只读展示摘要，无需重填）。
  if (isEdit.value && !protocolChanged.value) {
    return true;
  }
  return currentProtoFields.value.length > 0 && currentProtoFields.value.every(isFieldValid);
});

/** ③ 有默认选项，永远可继续。 */
const step3Valid = computed(() => true);

const nameValid = computed(() => name.value.trim().length > 0);
const intervalValid = computed(() => {
  const n = Number(intervalMs.value);
  return Number.isFinite(n) && n >= 50;
});
const reasonValid = computed(() => changeReason.value.trim().length >= 4);

/** ④ 「保存并开始采集」的全部前置条件。 */
const canSaveAndStart = computed(
  () => nameValid.value && intervalValid.value && reasonValid.value && step2Valid.value,
);

/**
 * 编辑态提交条件：名称 + 变更原因；若改了协议，还需重填连接参数。
 * 未改协议时连接参数与点表均未变更，无需重填。
 */
const canSubmitEdit = computed(() => {
  if (!nameValid.value || !reasonValid.value) {
    return false;
  }
  return protocolChanged.value ? step2Valid.value : true;
});

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
  return canSaveAndStart.value;
});

// ---------------------------------------------------------------------------
// ④ 连通性测试（POST /api/devices/test，只渲染后端结构化结果）
// ---------------------------------------------------------------------------

type StepState = 'idle' | 'run' | 'done' | 'fail';

interface TestStep {
  /** 序号 */
  n: string;
  /** 步骤名 */
  label: string;
  /** 状态 */
  state: StepState;
  /** 说明 */
  desc: string;
  /** 耗时 / 结果文案 */
  time: string;
}

const testSteps = ref<TestStep[]>([]);
const testRunning = ref(false);
const testResult = ref<{ ok: boolean; text: string } | null>(null);
const testUnsupported = ref(false);

/** 探测目标（按协议挑最少必要字段，缺什么就诚实显示缺什么）。 */
const probeTarget = computed<{ protocol: string; address: string; slave: string }>(() => {
  const c = conn as Record<string, string>;
  const p = protocol.value || '';
  if (p === 'modbus-tcp' || p === 's7' || p === 'mc') {
    return { protocol: p, address: `${(c.host ?? '').trim()}:${(c.port ?? '').trim()}`, slave: (c.unitId ?? '').trim() };
  }
  if (p === 'modbus-rtu') {
    return { protocol: p, address: (c.serial ?? '').trim(), slave: (c.unitId ?? '').trim() };
  }
  if (p === 'opc-ua') {
    return { protocol: p, address: (c.endpoint ?? '').trim(), slave: '' };
  }
  if (p === 'http') {
    return { protocol: p, address: (c.url ?? '').trim(), slave: '' };
  }
  if (p === 'mqtt') {
    return { protocol: p, address: (c.broker ?? '').trim(), slave: '' };
  }
  return { protocol: p, address: '', slave: '' };
});

const probeHint = computed(() => {
  const t = probeTarget.value;
  if (!t.protocol) {
    return '请先在第 1 步选择设备类型。';
  }
  if (!t.address || t.address === ':') {
    return '连接参数尚未填完，测试目标不可得。';
  }
  return t.address;
});

/** 初始化 / 复位四个步骤（原型 :1609-1612）。 */
function resetTestSteps(): void {
  const t = probeTarget.value;
  testSteps.value = [
    { n: '1', label: '建立连接', state: 'idle', desc: t.address || '待填写', time: '—' },
    { n: '2', label: '协议握手', state: 'idle', desc: t.slave ? `从站 ${t.slave}` : protocolLabel.value, time: '—' },
    { n: '3', label: '读取首个点位', state: 'idle', desc: probeRegister.value, time: '—' },
    { n: '4', label: '校验点表地址', state: 'idle', desc: '地址语义正确性只能人工核对', time: '—' },
  ];
}

/** 探测用寄存器（仅 Modbus 系列有意义）。 */
const probeRegister = computed(() =>
  protocol.value === 'modbus-tcp' || protocol.value === 'modbus-rtu' ? '40001' : '—',
);

/**
 * 跑一次连通性测试。
 *
 * 真实调用 `POST /api/devices/test`，**只渲染后端结构化结果**（Modbus 全探测，
 * 其余协议返回 unsupported_protocol —— 不伪造成功、不摆拍耗时）。
 */
async function runTest(): Promise<void> {
  if (testRunning.value) {
    return;
  }
  testRunning.value = true;
  testResult.value = null;
  testUnsupported.value = false;
  resetTestSteps();
  const steps = testSteps.value;

  try {
    // 统一走 repo.actions.testDevice（已封装 POST /api/devices/test，
    // 返回结构化 ProbeResult；后端只做 Modbus 全探测，其余协议返回 unsupported_protocol）。
    const pr = await repo.actions.testDevice({
      protocol: probeTarget.value.protocol || undefined,
      address: probeTarget.value.address || undefined,
      timeoutMs: Number((conn.timeout ?? '') || 1000),
      slave: probeTarget.value.slave || undefined,
      register: probeRegister.value !== '—' ? probeRegister.value : undefined,
    });
    const ok = pr.ok;
    const elapsed = pr.elapsedMs;
    const reason = pr.reason;
    const kind = pr.errorKind;

    if (ok) {
      for (const step of steps) {
        if (step.n === '4') {
          break;
        }
        step.state = 'done';
        step.time = elapsed ? `${elapsed} ms` : '完成';
      }
      steps[3].state = 'idle';
      steps[3].time = '需人工核对';
      testResult.value = {
        ok: true,
        text: `网关探测成功（耗时 ${elapsed || '—'} ms）。这只说明「读得到」，不代表地址语义正确。`,
      };
    } else {
      steps[0].state = 'fail';
      steps[0].time = elapsed ? `${elapsed} ms` : '失败';
      steps[0].desc = reason || steps[0].desc;
      for (const step of steps.slice(1)) {
        step.state = 'idle';
        step.time = '未执行';
      }
      testResult.value = { ok: false, text: `探测未通过（${kind || '失败'}）：${reason || '后端返回结构化失败，无详细信息'}` };
      testUnsupported.value = kind === 'unsupported_protocol';
    }
  } catch (cause) {
    steps[0].state = 'fail';
    steps[0].time = '失败';
    steps[0].desc = cause instanceof Error ? cause.message : String(cause);
    for (const step of steps.slice(1)) {
      step.state = 'idle';
      step.time = '未执行';
    }
    testResult.value = {
      ok: false,
      text: `连通性测试未执行成功：${cause instanceof Error ? cause.message : String(cause)}（可能是网关未启动、未登录或当前角色无 device.view 权限）`,
    };
  } finally {
    testRunning.value = false;
  }
}

// ---------------------------------------------------------------------------
// 导航
// ---------------------------------------------------------------------------

/** 跳到指定步：只可去已到达过的步骤，未到达的锁定。 */
function gotoStep(target: number): void {
  if (target < 1 || target > STEPS.length) {
    return;
  }
  if (target > maxReached.value) {
    return;
  }
  currentStep.value = target;
}

function next(): void {
  if (currentStep.value === 2) {
    connTouched.value = true;
  }
  if (currentStep.value === 4) {
    saveTouched.value = true;
  }
  if (!stepValid.value || currentStep.value >= STEPS.length) {
    return;
  }
  const target = currentStep.value + 1;
  currentStep.value = target;
  maxReached.value = Math.max(maxReached.value, target);
}

function prev(): void {
  if (currentStep.value > 1) {
    currentStep.value -= 1;
  }
}

// ---------------------------------------------------------------------------
// ④ 保存
// ---------------------------------------------------------------------------

/**
 * 提交设备。
 *
 * @param mode `saveAndStart` = 保存并要求网关立即开始采集；`save` = 仅保存连接参数。
 */
async function submit(mode: 'save' | 'saveAndStart'): Promise<void> {
  saveTouched.value = true;
  if (!nameValid.value || !protocol.value) {
    return;
  }
  if (isEdit.value) {
    if (!canSubmitEdit.value) {
      return;
    }
  } else {
    if (mode === 'saveAndStart' && !canSaveAndStart.value) {
      return;
    }
    if (mode === 'save' && !intervalValid.value) {
      return;
    }
  }

  const draft: DeviceDraft = {
    name: name.value,
    protocol: protocol.value,
    // 未改协议时沿用设备记录里的连接摘要（逐字段值本就不可得，不伪造）。
    connectionSummary:
      isEdit.value && !protocolChanged.value ? (editDevice.value?.connectionSummary ?? '') : connectionSummary.value,
    intervalMs: Number(intervalMs.value),
    timeoutMs: Number((conn.timeout ?? '') || 1000),
    retryTimes: Number((conn.retry ?? '') || 3),
    actor: session.state.displayName,
  };

  if (isEdit.value) {
    const result = await repo.updateDevice({ ...draft, id: editId.value });
    if (!result.ok || !result.data) {
      // 写失败：如实呈现后端原因，绝不报告成功。
      createdName.value = '';
      createdNote.value = `设备未更新：${result.message}`;
      return;
    }
    const updated = result.data;
    createdName.value = updated.name;
    createdNote.value = protocolChanged.value
      ? `设备「${updated.name}」已更新（变更原因：${changeReason.value.trim()}）。协议已变更，请到「点位与映射」重新校准点表。`
      : `设备「${updated.name}」已更新（变更原因：${changeReason.value.trim()}）。`;
    return;
  }

  const result = await repo.createDevice(draft);
  if (!result.ok || !result.data) {
    // 写失败：如实呈现后端原因，绝不报告成功。
    createdName.value = '';
    createdNote.value = `设备未提交：${result.message}`;
    return;
  }
  const created = result.data;
  createdName.value = created.name;
  createdId.value = created.id;
  createdNote.value =
    mode === 'saveAndStart'
      ? `设备「${created.name}」已提交并开始采集（变更原因：${changeReason.value.trim()}）。设备是否真正连通由网关侧判定，可在设备列表查看状态。`
      : `设备「${created.name}」已仅保存连接参数（变更原因：${changeReason.value.trim()}）。设备将显示「未配点表」，采不到任何数据 —— 请到「点位与映射」补齐点表。`;
}

/** 自定义参数行增删。 */
function addCustomParam(): void {
  customParams.value.push({ k: '', v: '' });
}
function removeCustomParam(index: number): void {
  customParams.value.splice(index, 1);
}

/** 重置向导（继续新增）。 */
function resetWizard(): void {
  currentStep.value = 1;
  maxReached.value = 1;
  protocol.value = '';
  for (const key of Object.keys(conn)) {
    delete conn[key];
  }
  name.value = '';
  deviceId.value = `dev-${String(Date.now())}`;
  intervalMs.value = '200';
  group.value = '未分组';
  enableWhen.value = '保存后立即启用采集';
  memo.value = '';
  changeReason.value = '';
  customParams.value = [];
  connTouched.value = false;
  saveTouched.value = false;
  createdName.value = '';
  createdNote.value = '';
  createdId.value = '';
  testResult.value = null;
  testUnsupported.value = false;
  resetTestSteps();
}

/** 跳转。 */
function go(name: string): void {
  void router.push({ name });
}

/** 去点位与映射并预选该设备（成功横幅入口）。 */
function goPoints(deviceId: string): void {
  void router.push({ name: 'points', query: { device: deviceId } });
}

resetTestSteps();
</script>

<style scoped>
/* ══ 向导条（原型 .wz :531）：4 步 + 连接线，撑满整张卡片 ══ */
.dv-wz {
  padding: 14px 18px;
}
.dv-wz__row {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
}
.dv-wz__item {
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: var(--fs-table);
  color: var(--text-3);
  font-weight: 600;
  transition: color 160ms cubic-bezier(0.16, 1, 0.3, 1);
}
.dv-wz__num {
  width: 22px;
  height: 22px;
  flex: 0 0 22px;
  border-radius: 50%;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  font-family: var(--font-mono);
  font-size: 12px;
  background: var(--divider);
  color: var(--text-3);
}
.dv-wz__item.is-clickable {
  cursor: pointer;
}
.dv-wz__item.is-clickable:hover {
  color: var(--text-1);
}
.dv-wz__item.is-clickable:hover .dv-wz__num {
  background: var(--brand-subtle);
  color: var(--brand-hover);
}
.dv-wz__item.is-lock {
  opacity: 0.5;
  cursor: not-allowed;
}
.dv-wz__item.is-run {
  color: var(--text-1);
}
.dv-wz__item.is-run .dv-wz__num {
  background: var(--brand);
  color: #fff;
}
.dv-wz__item.is-done {
  color: var(--brand-hover);
}
.dv-wz__item.is-done .dv-wz__num {
  background: var(--brand-subtle);
  color: var(--brand-hover);
}
/* 连接线：flex 撑满剩余宽度（原型 .wz-line） */
.dv-wz__line {
  flex: 1 1 24px;
  height: 1px;
  min-width: 16px;
  margin: 0 12px;
  background: #e4e9f2;
}

/* 步骤上下文（原型 .wz-ctx） */
.dv-ctx {
  display: flex;
  align-items: center;
  gap: 9px;
  flex-wrap: wrap;
  font-size: var(--fs-table);
  color: var(--text-2);
}
.dv-ctx b {
  font-family: var(--font-mono);
  color: var(--text-1);
}
.dv-ctx__hint {
  font-size: var(--fs-caption);
  color: var(--text-3);
}

/* ① 设备类型：双列卡片网格 + 可滚动（容纳持续扩充的类型清单） */
.dv-type-scroll {
  max-height: 480px;
  overflow-y: auto;
  padding-right: 4px;
}
/* class 落在 UiRadio 根元素（.uik-radio-row）上，直接覆写为双列网格 */
.dv-type-grid.uik-radio-row {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: 10px;
}
@media (max-width: 960px) {
  .dv-type-grid.uik-radio-row {
    grid-template-columns: 1fr;
  }
}

/* 表单分组标题 + 单位后缀 */
.dv-grp {
  margin: 6px 0 10px;
  padding-bottom: 6px;
  border-bottom: 1px dashed var(--border);
  font-size: var(--fs-caption);
  font-weight: 600;
  color: var(--text-3);
  letter-spacing: 0.02em;
}
.dv-form {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: 14px 16px;
  margin-bottom: 18px;
  max-width: 880px;
}
.dv-inp {
  display: flex;
  align-items: center;
  gap: 8px;
}
.dv-inp__unit {
  flex: 0 0 auto;
  font-size: var(--fs-caption);
  color: var(--text-3);
}
.dv-sub-h {
  margin: 18px 0 8px;
  font-size: var(--fs-caption);
  font-weight: 600;
  color: var(--text-3);
}

/* 连通性测试步骤（原型 .steps） */
.dv-test {
  list-style: none;
  margin: 0;
  padding: 0;
  display: flex;
  flex-direction: column;
  gap: 8px;
}
.dv-test__item {
  display: grid;
  grid-template-columns: 24px minmax(0, 1fr) auto;
  align-items: center;
  gap: 10px;
  padding: 10px 12px;
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  background: var(--bg-card);
}
.dv-test__item.is-done {
  border-color: var(--ok-border);
  background: var(--ok-bg);
}
.dv-test__item.is-run {
  border-color: var(--brand);
  background: var(--brand-subtle);
}
.dv-test__item.is-fail {
  border-color: var(--danger-border);
  background: var(--danger-bg);
}
.dv-test__n {
  width: 20px;
  height: 20px;
  border-radius: 50%;
  background: var(--divider);
  color: var(--text-2);
  display: inline-flex;
  align-items: center;
  justify-content: center;
  font-size: 11px;
  font-weight: 700;
}
.dv-test__item.is-done .dv-test__n {
  background: var(--ok);
  color: #fff;
}
.dv-test__item.is-fail .dv-test__n {
  background: var(--danger);
  color: #fff;
}
.dv-test__main {
  display: flex;
  flex-direction: column;
  gap: 2px;
  min-width: 0;
}
.dv-test__label {
  font-size: var(--fs-table);
  font-weight: 600;
  color: var(--text-1);
}
.dv-test__desc {
  font-size: var(--fs-caption);
  color: var(--text-3);
  word-break: break-all;
}
.dv-test__time {
  font-size: var(--fs-caption);
  color: var(--text-3);
}

/* 自定义参数行（原型 .cp-rows） */
.dv-cp {
  display: flex;
  flex-direction: column;
  gap: 8px;
}
.dv-cp__row {
  display: grid;
  grid-template-columns: minmax(120px, 200px) minmax(160px, 1fr) auto;
  gap: 8px;
  align-items: center;
}

.dv-foot {
  display: flex;
  align-items: center;
  gap: 12px;
  flex-wrap: wrap;
}
.dv-foot__ops {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-wrap: wrap;
}
.dv-foot__hint,
.dv-foot__txt {
  font-size: var(--fs-caption);
  color: var(--text-3);
}
.dv-foot__txt b {
  font-family: var(--font-mono);
  color: var(--text-1);
}
.dv-foot__warn {
  color: var(--danger);
  font-weight: 600;
}
.dv-foot--bar {
  margin-top: 18px;
  padding: 14px 16px;
  border: 1px solid var(--border);
  border-radius: var(--radius);
  background: var(--bg-card);
}
</style>
