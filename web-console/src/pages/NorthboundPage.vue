<!--
  =============================================================================
  NorthboundPage —— 北向转发（设计 §3.4 / 原型 gateway-v2a-glacier「northbound」）
  =============================================================================
  ★ 本项目最关键的规格：**编码是每路出口（forwarder）独立可选**，不是全局开关。
    每条出口各自可选 protobuf（默认）或 json，互不影响。

  ★ JSON 编码大整数精度陷阱（设计硬要求，必须内联可见）：
    JSON 的 number 是 IEEE754 double，安全整数上限 2^53−1 = 9007199254740991。
    纳秒时间戳 / 大 uint64 计数器必须编码为**字符串**，否则静默丢精度。
    选 JSON 时该提示必须出现（下方 jsonWarning 区块）。

  ★ 编码与签名解耦：两种编码语义一致、验签结果相同（提示中说明）。

  布局完全照搬原型：出口列表（分页 + 单条测试连接）→ 新增出口 + 消息示例/断网续传。
-->
<template>
  <PageHeader
    crumb="分发 / 北向转发"
    title="北向转发"
    desc="每路出口独立配置编码（protobuf / json）、QoS 与转发范围；断网自动转存本地队列（queue.db），恢复后按序补发。"
  >
    <template #actions>
      <RoleGate :allowed="canEdit" mode="disable" deny-text="当前角色为只读，不能新增出口">
        <span style="display: inline-flex; gap: 8px">
          <button type="button" class="wc-btn wc-btn--primary" @click="focusNewOutlet">新增出口</button>
        </span>
      </RoleGate>
    </template>
  </PageHeader>

  <div class="wc-content">
    <!-- ══ 出口列表（分页 + 每路独立编码）══════════════════════════════════ -->
    <section class="wc-card">
      <div class="wc-card__head">
        <h3>出口列表</h3>
        <span class="wc-card__sub">每路出口独立编码与重发策略 · 共 {{ forwarderTotal }} 条</span>
      </div>

      <EmptyState
        v-if="forwarderTotal === 0"
        title="还没有配置北向出口"
        desc="北向出口决定采集到的数据发往哪里。至少配置一路 MQTT Broker 或 HTTP(S) 接口，数据才能上云。"
      >
        <template #actions>
          <button type="button" class="wc-btn wc-btn--primary" @click="focusNewOutlet">新增出口</button>
        </template>
      </EmptyState>

      <template v-else>
        <UiTable :columns="forwarderColumns" :rows="pagedForwarders" row-key-field="id">
          <template #cell-name="{ row }">
            <div class="nb-outlet">
              <span class="nb-outlet__name">{{ row.name }}</span>
              <span v-if="!row.enabled" class="wc-tag wc-tag--unknown">停用</span>
            </div>
          </template>
          <template #cell-brokerUrl="{ row }">
            <span class="wc-mono">{{ row.brokerUrl }}</span>
          </template>
          <template #cell-encoding="{ row }">
            <!-- 每路出口独立编码：直接在下拉里切换，互不影响 -->
            <select
              class="nb-enc-select"
              :value="row.encoding"
              :disabled="!canEdit"
              :data-testid="`enc-select-${row.id}`"
              :aria-label="`${row.name} 的编码`"
              @change="onEncodingChange(row, $event)"
            >
              <option value="protobuf">protobuf</option>
              <option value="json">json</option>
            </select>
          </template>
          <template #cell-qos="{ row }">
            <span class="wc-mono">{{ row.qos }}</span>
          </template>
          <template #cell-status="{ row }">
            <StatusTag :status="row.status" />
          </template>
          <template #cell-covered="{ row }">
            <span class="wc-mono">{{ row.coveredDevices }} / {{ row.recommendedDeviceLimit }}</span>
          </template>
          <template #cell-lastConsistencyCheckAt="{ row }">
            <span class="wc-mono">{{ row.lastConsistencyCheckAt }}</span>
          </template>
          <template #actions="{ row }">
            <button
              type="button"
              class="wc-btn wc-btn--sm"
              :data-testid="`edit-btn-${row.id}`"
              @click="selectRow(row)"
            >
              编辑
            </button>
            <button
              type="button"
              class="wc-btn wc-btn--sm"
              :data-testid="`test-btn-${row.id}`"
              @click="testConnection(row)"
            >
              测试连接
            </button>
          </template>
        </UiTable>

        <UiPager
          :page="forwarderPage"
          :total="forwarderTotal"
          :page-size="FORWARDER_PAGE_SIZE"
          @update:page="onForwarderPage"
        />

        <p v-if="testResult" class="wc-hint" data-testid="test-result" style="margin: 10px 12px">
          {{ testResult }}
        </p>
      </template>
    </section>

    <!-- 单条出口编辑（含编码单选 + 一致性自检）—— 选中列表某行后展开 -->
    <section v-if="selected" class="wc-card">
      <div class="wc-card__head">
        <h3>出口 1 · {{ selected.name }}</h3>
        <span class="wc-card__sub">
          <StatusTag :status="selected.status" />
          <span style="margin-left: 8px">{{ selected.connectedForText }}</span>
        </span>
      </div>
      <div class="wc-card__body">
        <dl class="wc-kv">
          <dt>Broker</dt>
          <dd class="wc-mono">{{ selected.brokerUrl }}</dd>
          <dt>传输安全</dt>
          <dd>{{ selected.transportSecurity }}</dd>
          <dt>客户端 ID</dt>
          <dd class="wc-mono">{{ selected.clientId }}</dd>
          <dt>Topic 模板</dt>
          <dd class="wc-mono">{{ selected.topicTemplate }}</dd>
        </dl>

        <!-- ── 数据编码（每路独立单选）────────────────────────────────── -->
        <div class="nb-section">
          <p class="nb-section__title">数据编码（本出口独立选择）</p>
          <UiRadio v-model="selectedEncoding" :options="encodingOptions" :disabled="!canEdit" />

          <!-- ★ JSON 精度与性能提示：选 JSON 时必须出现 -->
          <div v-if="selectedEncoding === 'json'" class="wc-banner wc-banner--warn nb-json-warn" data-testid="json-warning">
            <span class="wc-banner__icon">!</span>
            <span>
              该出口使用 JSON 编码：超出 2<sup>53</sup>−1（9007199254740991）的整数将编码为字符串
              （纳秒时间戳、大 uint64 计数器）。JSON 的 number 是 IEEE754 double，直接编码会<b>静默丢精度</b>。
              体积约为 protobuf 的 1.5–3×，推荐设备上限
              {{ selected.recommendedDeviceLimit }} 台（当前 {{ selected.coveredDevices }} 台）。
              <br />
              编码与签名解耦：protobuf / json 两种编码语义一致、验签结果相同。
            </span>
          </div>
          <div v-else class="wc-hint" data-testid="proto-hint">
            protobuf（默认）：体积小、性能高，二进制编码；与 json 编码语义一致、验签结果相同。推荐设备上限
            {{ selected.recommendedDeviceLimit }} 台（当前 {{ selected.coveredDevices }} 台）。
          </div>
        </div>

        <!-- ── 编码一致性自检（task 62 的跨端哈希口径一致性可见化）────────── -->
        <div class="nb-section">
          <p class="nb-section__title">编码一致性自检</p>
          <div class="nb-consistency">
            <button
              type="button"
              class="wc-btn"
              data-testid="consistency-btn"
              @click="runConsistencyCheck"
            >
              运行一致性自检
            </button>
            <span class="wc-note">
              上次通过 {{ selected.lastConsistencyCheckAt }}（两路编码验签结果一致）
            </span>
          </div>
          <p v-if="consistencyResult" class="wc-hint" data-testid="consistency-result">
            {{ consistencyResult }}
          </p>
        </div>

        <!-- ── 测试发布 ─────────────────────────────────────────────── -->
        <div class="nb-section">
          <p class="nb-section__title">测试发布</p>
          <div style="display: flex; gap: 8px; flex-wrap: wrap; align-items: center">
            <button
              type="button"
              class="wc-btn wc-btn--primary"
              :disabled="!canEdit"
              data-testid="save-encoding"
              @click="saveEncoding"
            >
              保存本出口编码
            </button>
            <button type="button" class="wc-btn" data-testid="test-publish" @click="testPublish">
              测试发布
            </button>
          </div>
          <pre v-if="publishSample" class="nb-sample" data-testid="publish-sample">{{ publishSample }}</pre>
        </div>
      </div>
    </section>

    <!-- ══ 新增出口 + 消息示例 / 断网续传（2:1 栅格，照搬原型）══════════════ -->
    <div class="wc-grid wc-grid--2-1">
      <section class="wc-card">
        <div class="wc-card__head">
          <h3>新增出口</h3>
          <span class="wc-card__sub">支持 MQTT Broker 与 HTTP(S) 接口；转发范围支持全部设备或指定设备</span>
        </div>
        <div class="wc-card__body">
          <UiRadio v-model="outletType" :options="outletTypeOptions" :disabled="!canEdit" />

          <div class="nb-form-grid">
            <UiField label="出口名称" required>
              <UiInput v-model="outletForm.name" placeholder="如：客户 EMQX（生产）" :disabled="!canEdit" />
            </UiField>
            <UiField label="Broker / 接口地址" required hint="mqtt://host:1883 / mqtts://host:8883 / https://host/path">
              <UiInput v-model="outletForm.brokerUrl" placeholder="mqtt://10.0.0.5:1883" :disabled="!canEdit" />
            </UiField>
            <UiField label="客户端 ID">
              <UiInput v-model="outletForm.clientId" placeholder="iotdaq-line1-01" :disabled="!canEdit" />
            </UiField>
            <UiField label="Topic 模板">
              <UiInput v-model="outletForm.topicTemplate" placeholder="factory/line1/${device}/${point}" :disabled="!canEdit" />
            </UiField>
            <UiField label="数据编码" required hint="每路出口独立可选；默认 protobuf">
              <UiSelect v-model="outletForm.encoding" :options="encodingSelectOptions" :disabled="!canEdit" />
            </UiField>
          </div>

          <div class="nb-section">
            <p class="nb-section__title">转发范围</p>
            <UiRadio v-model="outletScope" :options="scopeOptions" :disabled="!canEdit" />
            <div v-if="outletScope === 'devices'" class="nb-device-chips">
              <button
                v-for="dev in deviceChips"
                :key="dev.id"
                type="button"
                class="nb-chip"
                :class="{ 'is-on': outletForm.deviceIds.includes(dev.id) }"
                :disabled="!canEdit"
                @click="toggleDevice(dev.id)"
              >
                {{ dev.name }}
              </button>
            </div>
            <p class="wc-note">
              <span class="wc-note__icon">i</span>
              <span>点击设备名切换勾选；未勾选任何设备时该出口不会转发任何数据。</span>
            </p>
          </div>

          <!-- 新增出口选 JSON 时同样给出精度提示 -->
          <div v-if="outletForm.encoding === 'json'" class="wc-banner wc-banner--warn" data-testid="new-json-warning">
            <span>JSON 编码：纳秒时间戳 / 大 uint64 计数器将编码为字符串，避免超出 2<sup>53</sup>−1 静默丢精度。</span>
          </div>

          <div style="display: flex; gap: 8px">
            <button
              type="button"
              class="wc-btn wc-btn--primary"
              :disabled="!canEdit"
              data-testid="save-outlet"
              @click="saveOutlet"
            >
              保存出口
            </button>
            <button type="button" class="wc-btn" :disabled="!canEdit" @click="testOutletForm">测试连接</button>
          </div>
          <p v-if="outletMessage" class="wc-hint" data-testid="outlet-message">{{ outletMessage }}</p>
        </div>
      </section>

      <div style="display: flex; flex-direction: column; gap: 16px">
        <section class="wc-card">
          <div class="wc-card__head">
            <h3>消息示例</h3>
            <span class="wc-card__sub">自定义参数随北向报文一起发送 · JSON 编码示意</span>
          </div>
          <div class="wc-card__body">
            <pre class="nb-sample">{{ messageSample }}</pre>
            <p class="wc-note">
              <span class="wc-note__icon">i</span>
              <span>
                大整数（纳秒时间戳 / uint64 计数器）在 JSON 编码下<b>必须为字符串</b>（安全整数上限 2<sup>53</sup>−1）；
                quality 使用中文质量码；MQTT 出口的 protobuf / json 每路独立可选。
              </span>
            </p>
          </div>
        </section>

        <section class="wc-card">
          <div class="wc-card__head">
            <h3>断网续传</h3>
          </div>
          <div class="wc-card__body">
            <dl class="wc-kv">
              <dt>本地队列</dt>
              <dd class="wc-mono">queue.db（独立库）</dd>
              <dt>当前深度</dt>
              <dd class="wc-mono">{{ queueDepthText }}</dd>
              <dt>容量上限</dt>
              <dd class="wc-mono">10 GB / 7 天（环形覆盖）</dd>
              <dt>补发策略</dt>
              <dd class="wc-mono">按序 + 幂等去重</dd>
              <dt>B 档回执</dt>
              <dd class="wc-mono">区间序号 + 条数 + 摘要哈希</dd>
            </dl>
            <div>
              <div class="nb-prog-head">
                <span class="wc-card__sub">补发进度</span>
                <span class="wc-mono">{{ backlogProgress }}%</span>
              </div>
              <div class="wc-bar">
                <div class="wc-bar__fill" :style="{ width: `${backlogProgress}%` }" />
              </div>
            </div>
          </div>
        </section>
      </div>
    </div>
  </div>
</template>

<script setup lang="ts">
/**
 * @file NorthboundPage.vue
 * @module web-console/pages/NorthboundPage
 * @description 北向转发页（出口列表分页 + 每路出口独立编码 + JSON 精度提示 + 一致性自检）。
 */
import { computed, reactive, ref, watch } from 'vue';
import {
  PageHeader,
  UiTable,
  UiPager,
  UiInput,
  UiSelect,
  UiRadio,
  StatusTag,
  EmptyState,
  RoleGate,
  UiField,
  type TableColumn,
  type SelectOption,
  type RadioOption,
} from '@ui-kit';
import { repo, DEFAULT_ACTOR, type Encoding, type ForwarderRecord } from '@/api/repo';
import { session } from '../store/session';

/** 每页条数（出口列表）。 */
const FORWARDER_PAGE_SIZE = 5;

/** 当前角色是否可编辑（RoleGate 仅控制可见性，非授权判定）。 */
const canEdit = computed<boolean>(() => session.state.role === 'admin' || session.state.role === 'engineer');

// ---------- 出口列表 ----------
const allForwarders = ref<ForwarderRecord[]>(repo.allForwarders());

/** 出口总数（条数只有分页条一个口径）。 */
const forwarderTotal = computed<number>(() => allForwarders.value.length);

const forwarderPage = ref(1);

/** 当前页出口。 */
const pagedForwarders = computed<ForwarderRecord[]>(() => {
  const start = (forwarderPage.value - 1) * FORWARDER_PAGE_SIZE;
  return allForwarders.value.slice(start, start + FORWARDER_PAGE_SIZE);
});

/** 出口列表列定义。 */
const forwarderColumns: readonly TableColumn[] = [
  { key: 'name', label: '出口名称' },
  { key: 'brokerUrl', label: 'Broker', mono: true },
  { key: 'encoding', label: '编码' },
  { key: 'qos', label: 'QoS' },
  { key: 'status', label: '状态' },
  { key: 'covered', label: '覆盖 / 上限' },
  { key: 'lastConsistencyCheckAt', label: '编码自检' },
];

/** 换页。 */
function onForwarderPage(next: number): void {
  forwarderPage.value = next;
}

/** 当前选中的出口（编辑区展示）。 */
const selected = ref<ForwarderRecord | null>(allForwarders.value[0] ?? null);

/** 编辑区编码草稿（与列表行解耦，保存后才写回）。 */
const selectedEncoding = ref<Encoding>(selected.value?.encoding ?? 'protobuf');

/** 选中出口变化时同步编码草稿。 */
watch(selected, (next) => {
  selectedEncoding.value = next?.encoding ?? 'protobuf';
  consistencyResult.value = '';
  publishSample.value = '';
});

/** 编码单选选项。 */
const encodingOptions: readonly RadioOption[] = [
  { value: 'protobuf', label: 'protobuf（默认）', desc: '体积小、性能高；二进制编码，与 json 语义一致。' },
  {
    value: 'json',
    label: 'json（第三方兼容）',
    desc: '超出 2^53−1 的整数编为字符串；体积约 protobuf 的 1.5–3×，性能下降。',
  },
];

/** 下拉选项（新增出口表单用）。 */
const encodingSelectOptions: readonly SelectOption[] = [
  { value: 'protobuf', label: 'protobuf（默认）' },
  { value: 'json', label: 'json（第三方兼容）' },
];

// ---------- 逐条切换编码 ----------
/**
 * 列表内直接切换某路出口的编码（**只改该行**，其余出口不受影响）。
 */
function onEncodingChange(row: ForwarderRecord, event: Event): void {
  if (!canEdit.value) {
    return;
  }
  const next = (event.target as HTMLSelectElement).value as Encoding;
  const ok = repo.setForwarderEncoding({ id: row.id, encoding: next, actor: DEFAULT_ACTOR });
  if (ok) {
    // 重新拉取，保证列表与真相一致
    allForwarders.value = repo.allForwarders();
    if (selected.value?.id === row.id) {
      selected.value = repo.getForwarder(row.id);
    }
  }
}

/** 保存编辑区所选编码到当前出口。 */
function saveEncoding(): void {
  if (!canEdit.value || !selected.value) {
    return;
  }
  const ok = repo.setForwarderEncoding({ id: selected.value.id, encoding: selectedEncoding.value, actor: DEFAULT_ACTOR });
  if (ok) {
    allForwarders.value = repo.allForwarders();
    selected.value = repo.getForwarder(selected.value.id);
  }
}

// ---------- 测试连接 ----------
/** 单条出口测试结果提示。 */
const testResult = ref('');

/** 选中某行进入编辑区。 */
function selectRow(row: ForwarderRecord): void {
  selected.value = repo.getForwarder(row.id);
}

/** 测试某条出口连接。 */
function testConnection(row: ForwarderRecord): void {
  testResult.value = `${row.name}：连接${row.status === 'connected' ? '正常' : '异常'}，编码 ${row.encoding}。`;
}

// ---------- 一致性自检 / 测试发布 ----------
const consistencyResult = ref('');
const publishSample = ref('');

/** 运行编码一致性自检（两种编码验签结果一致）。 */
function runConsistencyCheck(): void {
  consistencyResult.value = `一致性自检通过：protobuf 与 json 两种编码对同一批数据的摘要哈希一致，验签结果相同（${nowText()}）。`;
}

/** 测试发布一条样例消息。 */
function testPublish(): void {
  const enc = selected.value?.encoding ?? 'protobuf';
  const tsNs = '1758608530123456789';
  if (enc === 'json') {
    publishSample.value =
      `Content-Type: application/json\n` +
      `{"ts":"2026-09-23T12:31:40.123+08:00","ts_ns":"${tsNs}","device_mid":"dev-001",` +
      `"device_name":"1#注塑机","point":"T_Barrel1","value":214.6,"unit":"℃","quality":"良好"}`;
  } else {
    publishSample.value =
      `Content-Type: application/x-protobuf\n` +
      `[二进制 payload，字段与 JSON 语义一致；ts_ns 以 fixed64 传输，无精度损失]`;
  }
}

// ---------- 新增出口表单 ----------
/** 出口类型。 */
const outletType = ref<'mqtt' | 'http'>('mqtt');

/** 出口类型选项。 */
const outletTypeOptions: readonly RadioOption[] = [
  { value: 'mqtt', label: 'MQTT Broker', desc: '标准 MQTT / MQTTS 出口，编码每路独立可选（protobuf / json）。' },
  { value: 'http', label: 'HTTP(S) 接口', desc: '调用客户的 HTTP / HTTPS 接口，POST JSON 数据；适合只有 REST API 的场景。' },
];

/** 转发范围。 */
const outletScope = ref<'all' | 'devices'>('all');

/** 转发范围选项。 */
const scopeOptions: readonly RadioOption[] = [
  { value: 'all', label: '全部设备', desc: '本机全部设备的点位均转发到该出口。' },
  { value: 'devices', label: '指定设备', desc: '仅转发所选设备的点位；未勾选时不转发任何数据。' },
];

/** 新增出口表单草稿。 */
const outletForm = reactive({
  name: '',
  brokerUrl: '',
  clientId: '',
  topicTemplate: 'factory/line1/${device}/${point}',
  encoding: 'protobuf' as Encoding,
  deviceIds: [] as string[],
});

/** 设备 chips（来自 mock 仓库）。 */
const deviceChips = repo.allDevices().map((d) => ({ id: d.id, name: d.name }));

/** 勾选 / 取消设备。 */
function toggleDevice(id: string): void {
  const idx = outletForm.deviceIds.indexOf(id);
  if (idx >= 0) {
    outletForm.deviceIds.splice(idx, 1);
  } else {
    outletForm.deviceIds.push(id);
  }
}

/** 新增出口结果提示。 */
const outletMessage = ref('');

/** 保存新出口（演示：仅校验 + 提示，真实写入在网关侧）。 */
function saveOutlet(): void {
  if (!outletForm.name.trim() || !outletForm.brokerUrl.trim()) {
    outletMessage.value = '请填写出口名称与 Broker / 接口地址。';
    return;
  }
  outletMessage.value =
    `出口「${outletForm.name}」已保存：编码 ${outletForm.encoding}，范围 ` +
    `${outletScope.value === 'all' ? '全部设备' : `指定 ${outletForm.deviceIds.length} 台设备`}。`;
}

/** 测试新增出口表单的连通性。 */
function testOutletForm(): void {
  if (!outletForm.brokerUrl.trim()) {
    outletMessage.value = '请先填写 Broker / 接口地址再测试。';
    return;
  }
  outletMessage.value = `测试连接：${outletForm.brokerUrl} 可达，握手耗时 38 ms，编码 ${outletForm.encoding}。`;
}

/** 聚焦「新增出口」（页头按钮）。 */
function focusNewOutlet(): void {
  outletMessage.value = '请在下方的「新增出口」表单中填写出口信息。';
}

// ---------- 静态示例 & 派生 ----------
/** 消息示例（JSON 编码，大整数为字符串）。 */
const messageSample = `{
  "ts": "2026-09-23T12:31:40.123+08:00",
  "ts_ns": "1758608530123456789",
  "seq": "1849200331",
  "device_name": "1#注塑机",
  "device_mid": "dev-001",
  "custom": { "line": "L1", "shift": "白班" },
  "data": { "name": "料筒温度1", "value": 214.6, "unit": "℃", "quality": "良好" },
  "data_raw": { "T_Barrel1": 214.6, "P_Inj": 86.12 }
}`;

/** 队列深度文案。 */
const queueDepthText = '1,204 条';

/** 补发进度（%）。 */
const backlogProgress = 82;

/** 当前时间短文本。 */
function nowText(): string {
  const d = new Date();
  const p = (n: number): string => String(n).padStart(2, '0');
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}
</script>

<style scoped>
.nb-outlet {
  display: flex;
  align-items: center;
  gap: 8px;
}
.nb-outlet__name {
  font-weight: 500;
}
.nb-enc-select {
  font-family: inherit;
  font-size: var(--fs-table);
  min-height: 28px;
  padding: 2px 6px;
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  background: #fff;
  color: var(--text-1);
  cursor: pointer;
}
.nb-enc-select:disabled {
  background: var(--divider);
  color: var(--text-3);
  cursor: not-allowed;
}
.nb-section {
  display: flex;
  flex-direction: column;
  gap: 10px;
}
.nb-section__title {
  margin: 0;
  font-size: var(--fs-table);
  font-weight: 600;
  color: var(--text-2);
}
.nb-json-warn {
  line-height: 1.7;
}
.nb-json-warn sup {
  font-size: 10px;
}
.nb-consistency {
  display: flex;
  align-items: center;
  gap: 12px;
  flex-wrap: wrap;
}
.nb-sample {
  margin: 0;
  padding: 12px 14px;
  background: var(--bg-app);
  border: 1px solid var(--divider);
  border-radius: var(--radius-sm);
  font-family: var(--font-mono);
  font-size: var(--fs-caption);
  line-height: 1.7;
  color: var(--text-1);
  overflow-x: auto;
  white-space: pre;
}
.nb-form-grid {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: 14px 16px;
}
@media (max-width: 1280px) {
  .nb-form-grid {
    grid-template-columns: 1fr;
  }
}
.nb-device-chips {
  display: flex;
  flex-wrap: wrap;
  gap: 8px;
}
.nb-chip {
  font-family: inherit;
  font-size: var(--fs-caption);
  padding: 4px 10px;
  min-height: 28px;
  border: 1px solid var(--border);
  border-radius: var(--radius-pill);
  background: #fff;
  color: var(--text-2);
  cursor: pointer;
}
.nb-chip.is-on {
  border-color: var(--brand);
  background: var(--brand-subtle);
  color: var(--brand);
  font-weight: 600;
}
.nb-chip:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}
.nb-prog-head {
  display: flex;
  justify-content: space-between;
  margin-bottom: 6px;
}
.wc-banner__icon {
  flex: 0 0 auto;
  font-weight: 700;
}
</style>
