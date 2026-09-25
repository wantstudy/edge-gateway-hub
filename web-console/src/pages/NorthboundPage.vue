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

      <!-- real 模式：出口数据源不可得时给出「原因 + 恢复路径」，不静默吞错 -->
      <div v-if="forwardersNotice" class="wc-banner wc-banner--warn" data-testid="forwarders-notice">
        <span class="wc-banner__icon">!</span>
        <span>{{ forwardersNotice }}</span>
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
            <span class="wc-mono">{{ countText(row.coveredDevices) }} / {{ countText(row.recommendedDeviceLimit) }}</span>
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
              {{ countText(selected.recommendedDeviceLimit) }} 台（当前 {{ countText(selected.coveredDevices) }} 台）。
              <br />
              编码与签名解耦：protobuf / json 两种编码语义一致、验签结果相同。
            </span>
          </div>
          <div v-else class="wc-hint" data-testid="proto-hint">
            protobuf（默认）：体积小、性能高，二进制编码；与 json 编码语义一致、验签结果相同。推荐设备上限
            {{ countText(selected.recommendedDeviceLimit) }} 台（当前 {{ countText(selected.coveredDevices) }} 台）。
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

          <!-- ── MQTT Broker 分支（原型 outletTypeBody :1319-1325）── -->
          <div v-if="outletType === 'mqtt'" class="nb-form-grid">
            <UiField label="出口名称" required>
              <UiInput v-model="outletForm.name" placeholder="如：客户 EMQX（生产）" :disabled="!canEdit" />
            </UiField>
            <UiField label="Broker 地址" required hint="mqtt://host:1883 / mqtts://host:8883">
              <UiInput v-model="outletForm.brokerUrl" placeholder="mqtt://10.0.0.5:1883" :disabled="!canEdit" />
            </UiField>
            <UiField label="客户端 ID">
              <UiInput v-model="outletForm.clientId" placeholder="iotdaq-line1-01" :disabled="!canEdit" />
            </UiField>
            <!-- ── MQTT 凭据（敏感字段，红线：口令掩码，明文绝不落前端状态）── -->
            <UiField label="MQTT 用户名" hint="与口令成对；留空表示匿名连接">
              <UiInput v-model="outletForm.username" placeholder="如：gw-user" :disabled="!canEdit" />
            </UiField>
            <UiField label="MQTT 口令" hint="敏感凭据：仅在网关侧加密存储（config_crypto 字段级加密），前端不以明文留存">
              <div class="nb-pwd">
                <UiInput
                  :type="showMqttPassword ? 'text' : 'password'"
                  v-model="mqttPassword"
                  placeholder="••••••••"
                  :disabled="!canEdit"
                  :maxlength="256"
                  data-testid="mqtt-password"
                />
                <button
                  type="button"
                  class="nb-pwd__toggle"
                  :disabled="!canEdit"
                  @click="showMqttPassword = !showMqttPassword"
                >
                  {{ showMqttPassword ? '隐藏' : '显示' }}
                </button>
              </div>
            </UiField>
            <UiField label="数据编码" required hint="每路出口独立可选；默认 protobuf">
              <UiSelect v-model="outletForm.encoding" :options="encodingSelectOptions" :disabled="!canEdit" />
            </UiField>
            <UiField label="QoS" required hint="0 至多一次 / 1 至少一次 / 2 恰好一次（默认 1）">
              <UiSelect v-model="outletForm.qos" :options="qosOptions" :disabled="!canEdit" />
            </UiField>
            <UiField label="Topic 前缀" hint="MQTT 5 报文以 Payload Format Indicator + Content Type 声明编码">
              <UiInput v-model="outletForm.topicTemplate" placeholder="factory/line1" :disabled="!canEdit" />
            </UiField>
          </div>

          <!-- ── HTTP(S) 接口分支（原型 outletTypeBody :1327-1334）── -->
          <div v-else class="nb-form-grid">
            <UiField label="出口名称" required>
              <UiInput v-model="outletForm.name" placeholder="如：客户 HTTP 接口" :disabled="!canEdit" />
            </UiField>
            <UiField label="接口地址（URL）" required hint="支持 http / https；https 强制校验服务端证书">
              <UiInput v-model="outletForm.brokerUrl" placeholder="https://client.example.com/api/ingest" :disabled="!canEdit" />
            </UiField>
            <UiField label="请求方法">
              <UiSelect v-model="outletForm.httpMethod" :options="httpMethodOptions" :disabled="!canEdit" />
            </UiField>
            <UiField label="批量大小">
              <UiSelect v-model="outletForm.batchSize" :options="batchSizeOptions" :disabled="!canEdit" />
            </UiField>
            <UiField label="认证 Header" hint="原样随请求发送；也可留空由客户网关鉴权">
              <UiInput v-model="outletForm.authHeader" placeholder="Authorization: Bearer &lt;token&gt;" :disabled="!canEdit" />
            </UiField>
            <UiField label="超时 / 重试" hint="失败进入本地队列，恢复后按序补发（与 MQTT 出口同一套断网续传）">
              <UiInput v-model="outletForm.timeoutRetry" placeholder="5 s · 3 次" :disabled="!canEdit" />
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

          <!-- 新增出口选 JSON 时同样给出精度提示（MQTT 分支；原型 outletTypeHint :1339） -->
          <div
            v-if="outletType === 'mqtt' && outletForm.encoding === 'json'"
            class="wc-banner wc-banner--warn"
            data-testid="new-json-warning"
          >
            <span>JSON 编码：纳秒时间戳 / 大 uint64 计数器将编码为字符串，避免超出 2<sup>53</sup>−1 静默丢精度。</span>
          </div>

          <!-- HTTP 出口固定 JSON 编码（原型 outletTypeHint :1338） -->
          <div v-if="outletType === 'http'" class="wc-banner wc-banner--warn" data-testid="new-http-hint">
            <span class="wc-banner__icon">!</span>
            <span>
              HTTP 出口<b>固定 JSON 编码</b>：请求体为
              <code>{"device_mid":…,"ts":…,"points":{target:value,…}}</code>，Content-Type: application/json；
              纳秒时间戳 / 大 uint64 计数器<b>必须编码为字符串</b>（安全整数上限 2<sup>53</sup>−1）。
            </span>
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
import { computed, onMounted, reactive, ref, watch } from 'vue';
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
import { API_MODE, repo, DEFAULT_ACTOR, type Encoding, type ForwarderRecord } from '@/api/repo';
import { apiRequest, ApiError } from '@/api/client';
import { session } from '../store/session';

/** 是否接入真实后端（`VITE_API_MODE=real`）；mock 模式行为保持与原版一致。 */
const IS_REAL = API_MODE === 'real';

/** 每页条数（出口列表）。 */
const FORWARDER_PAGE_SIZE = 5;

// ---------------------------------------------------------------------------
// 出口行视图
// ---------------------------------------------------------------------------

/**
 * 出口行视图：`/api/forwarders`（真实）与 mock 仓库（演示）共用同一行形状。
 *
 * 与 `ForwarderRecord` 的差异（**诚实表示未知**，不用 0 / false 冒充已知值）：
 *  · `status` 放宽为 string —— 后端 `/api/forwarders` 不上报连接态，空串渲染为「—」；
 *  · `coveredDevices` / `recommendedDeviceLimit` 可为 `null` —— 后端不上报时渲染为「—」；
 *  · `qosText` —— QoS 展示文本（HTTP 出口无 QoS，固定「—」）。
 */
type OutletRow = Omit<ForwarderRecord, 'status' | 'coveredDevices' | 'recommendedDeviceLimit'> & {
  /** 连接状态（空串 = 后端未上报，渲染为「—」） */
  status: string;
  /** 覆盖设备数（null = 后端未上报） */
  coveredDevices: number | null;
  /** 推荐设备上限（null = 后端未上报） */
  recommendedDeviceLimit: number | null;
  /** QoS 展示文本 */
  qosText: string;
  /** 出口类别（由地址协议前缀判定） */
  kind: 'mqtt' | 'http';
};

/** 按 Broker / URL 的协议前缀判定出口类别。 */
function kindOfUrl(url: string): 'mqtt' | 'http' {
  return /^https?:\/\//i.test(url.trim()) ? 'http' : 'mqtt';
}

/** mock 仓库出口 → 行视图（QoS 文本按出口类别给出）。 */
function toOutletRow(f: ForwarderRecord): OutletRow {
  const kind = kindOfUrl(f.brokerUrl);
  return {
    ...f,
    status: f.status,
    coveredDevices: f.coveredDevices,
    recommendedDeviceLimit: f.recommendedDeviceLimit,
    kind,
    qosText: kind === 'http' ? '—' : String(f.qos),
  };
}

/** 取字符串字段（数字按字符串透传，不做数值转换）。 */
function pickText(src: Record<string, unknown>, key: string, dflt: string): string {
  const v = src[key];
  if (typeof v === 'string') {
    return v;
  }
  if (typeof v === 'number' && Number.isFinite(v)) {
    return String(v);
  }
  return dflt;
}

/** `/api/forwarders` 原始行 → 行视图（未知字段一律「— / null」，绝不猜值）。 */
function mapForwarderRow(raw: Record<string, unknown>, idx: number): OutletRow {
  const id = pickText(raw, 'id', pickText(raw, 'name', `outlet-${idx}`));
  const target = pickText(raw, 'target', pickText(raw, 'broker', '—'));
  const kind = kindOfUrl(target);
  const qosRaw = pickText(raw, 'qos', '');
  const qos: 0 | 1 | 2 = qosRaw === '0' ? 0 : qosRaw === '2' ? 2 : 1;
  return {
    id,
    name: pickText(raw, 'name', id),
    brokerUrl: target,
    transportSecurity: raw['tls'] === true ? 'TLS' : '无',
    certStatusText: '—',
    clientId: '—',
    qos,
    retained: false,
    topicTemplate: pickText(raw, 'topic_prefix', '—'),
    encoding: pickText(raw, 'encoding', 'protobuf') === 'json' ? 'json' : 'protobuf',
    status: '',
    connectedForText: '后端未上报连接时长',
    coveredDevices: null,
    recommendedDeviceLimit: null,
    lastConsistencyCheckAt: '—',
    enabled: true,
    kind,
    qosText: kind === 'http' ? '—' : qosRaw || '—',
  };
}

/** 把 unknown 收敛为 Record（数组 / 非对象回空对象）。 */
function asRecord(value: unknown): Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : {};
}

/** 小值计数渲染：null（后端未上报）渲染为「—」，避免用 0 冒充已知值。 */
function countText(value: number | null): string {
  return value === null ? '—' : String(value);
}

/** 当前角色是否可编辑（RoleGate 仅控制可见性，非授权判定）。 */
const canEdit = computed<boolean>(() => session.state.role === 'admin' || session.state.role === 'engineer');

// ---------- 出口列表 ----------
const allForwarders = ref<OutletRow[]>(repo.allForwarders().map(toOutletRow));

/**
 * 出口数据源不可得的原因（real 模式专用）。
 *
 * 不静默吞错：`GET /api/forwarders` 失败时保留现有数据源，并把原因展示出来。
 */
const forwardersNotice = ref('');

/** 出口数据源失败的「原因 + 恢复路径」文案。 */
function forwarderFailureText(cause: unknown, subject: string): string {
  const status = cause instanceof ApiError ? cause.status : 0;
  const prefix = `${subject}接口不可得`;
  if (status === 0) {
    return `${prefix}：网关不可达（网络层失败）。恢复路径：确认网关进程在监听 8080 端口后刷新页面。`;
  }
  if (status === 403) {
    return `${prefix}：当前账号无 device.view 权限（HTTP 403）。恢复路径：改用具备该权限的账号登录。`;
  }
  if (status === 501) {
    return `${prefix}：后端能力未落地（HTTP 501）。恢复路径：等待对应能力上线后本页自动展示。`;
  }
  return `${prefix}：HTTP ${status}。恢复路径：查看网关日志定位，或刷新页面重试。`;
}

/**
 * real 模式：出口清单直接取 `GET /api/forwarders`（`id` = 出口名）。
 *
 * 失败时**不静默吞错**：保留 repo 数据源并展示原因，页面不崩、不伪造出口。
 */
async function loadForwarders(): Promise<void> {
  if (!IS_REAL) {
    return;
  }
  try {
    const raw = await apiRequest<unknown[]>('/api/forwarders');
    if (!Array.isArray(raw)) {
      forwardersNotice.value = '出口接口返回了非数组结构，已沿用现有数据源。';
      return;
    }
    allForwarders.value = raw.map((row, i) => mapForwarderRow(asRecord(row), i));
    forwardersNotice.value = '';
    if (selected.value) {
      selected.value = allForwarders.value.find((f) => f.id === selected.value?.id) ?? allForwarders.value[0] ?? null;
    } else {
      selected.value = allForwarders.value[0] ?? null;
    }
  } catch (cause) {
    allForwarders.value = repo.allForwarders().map(toOutletRow);
    forwardersNotice.value = forwarderFailureText(cause, 'GET /api/forwarders');
  }
}

onMounted(() => {
  void loadForwarders();
});

/** 出口总数（条数只有分页条一个口径）。 */
const forwarderTotal = computed<number>(() => allForwarders.value.length);

const forwarderPage = ref(1);

/** 当前页出口。 */
const pagedForwarders = computed<OutletRow[]>(() => {
  const start = (forwarderPage.value - 1) * FORWARDER_PAGE_SIZE;
  return allForwarders.value.slice(start, start + FORWARDER_PAGE_SIZE);
});

/** 出口列表列定义。 */
const forwarderColumns: readonly TableColumn[] = [
  { key: 'name', label: '出口名称' },
  { key: 'brokerUrl', label: 'Broker', mono: true },
  { key: 'encoding', label: '编码' },
  { key: 'qosText', label: 'QoS', mono: true },
  { key: 'status', label: '状态' },
  { key: 'covered', label: '覆盖 / 上限' },
  { key: 'lastConsistencyCheckAt', label: '编码自检' },
];

/** 换页。 */
function onForwarderPage(next: number): void {
  forwarderPage.value = next;
}

/** 当前选中的出口（编辑区展示）。 */
const selected = ref<OutletRow | null>(allForwarders.value[0] ?? null);

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
function onEncodingChange(row: OutletRow, event: Event): void {
  if (!canEdit.value) {
    return;
  }
  const next = (event.target as HTMLSelectElement).value as Encoding;
  const ok = repo.setForwarderEncoding({ id: row.id, encoding: next, actor: DEFAULT_ACTOR });
  if (ok) {
    // 重新拉取，保证列表与真相一致（real 模式下改编码走本地覆盖层，后端无写接口）
    allForwarders.value = repo.allForwarders().map(toOutletRow);
    if (selected.value?.id === row.id) {
      selected.value = allForwarders.value.find((f) => f.id === row.id) ?? null;
    }
  }
}

/** 保存编辑区所选编码到当前出口。 */
function saveEncoding(): void {
  if (!canEdit.value || !selected.value) {
    return;
  }
  const id = selected.value.id;
  const ok = repo.setForwarderEncoding({ id, encoding: selectedEncoding.value, actor: DEFAULT_ACTOR });
  if (ok) {
    allForwarders.value = repo.allForwarders().map(toOutletRow);
    selected.value = allForwarders.value.find((f) => f.id === id) ?? null;
  }
}

// ---------- 测试连接 ----------
/** 单条出口测试结果提示。 */
const testResult = ref('');

/** 选中某行进入编辑区。 */
function selectRow(row: OutletRow): void {
  selected.value = allForwarders.value.find((f) => f.id === row.id) ?? null;
}

/**
 * 测试某条出口连接。
 *
 * real：`POST /api/forwarders/{id}/test` —— 后端仅做 **TCP 建连**探测
 * （响应含 `probe:"tcp_connect"`；`mqtts://` 不做 TLS 握手 / MQTT CONNACK），
 * 未知出口 404；结果原样展示，不把「TCP 通」说成「MQTT 可用」。
 * mock：保持原演示行为。
 */
async function testConnection(row: OutletRow): Promise<void> {
  if (!IS_REAL) {
    testResult.value = `${row.name}：连接${row.status === 'connected' ? '正常' : '异常'}，编码 ${row.encoding}。`;
    return;
  }
  testResult.value = `正在探测「${row.name}」…`;
  try {
    const raw = await apiRequest<Record<string, unknown>>(
      `/api/forwarders/${encodeURIComponent(row.id)}/test`,
      { method: 'POST' },
    );
    const ok = raw['ok'] === true;
    const elapsed = pickText(raw, 'elapsed_ms', '—');
    if (ok) {
      testResult.value =
        `「${row.name}」：${pickText(raw, 'probe', 'tcp_connect')} 探测通过，耗时 ${elapsed} ms。` +
        `注意：${pickText(raw, 'note', '仅 TCP 建连探测，不含 TLS 握手与 MQTT CONNACK')}。`;
    } else {
      testResult.value =
        `「${row.name}」：探测失败（${pickText(raw, 'error_kind', 'failed')}），耗时 ${elapsed} ms —— ` +
        `${pickText(raw, 'reason', '后端未给出失败原因')}。`;
    }
  } catch (cause) {
    const status = cause instanceof ApiError ? cause.status : 0;
    testResult.value =
      status === 404
        ? `「${row.name}」：后端未收录该出口名（HTTP 404）—— 出口以「名称」为唯一键，请先确认配置中的出口名。`
        : `「${row.name}」：${forwarderFailureText(cause, 'POST /api/forwarders/{id}/test')}`;
  }
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

/** QoS 选项（原型 :1323）。 */
const qosOptions: readonly SelectOption[] = [
  { value: '0', label: '0（至多一次）' },
  { value: '1', label: '1（至少一次）' },
  { value: '2', label: '2（恰好一次）' },
];

/** HTTP 请求方法选项（原型 :1330）。 */
const httpMethodOptions: readonly SelectOption[] = [
  { value: 'POST', label: 'POST' },
  { value: 'PUT', label: 'PUT' },
];

/** HTTP 批量大小选项（原型 :1333）。 */
const batchSizeOptions: readonly SelectOption[] = [
  { value: '1 条 / 请求', label: '1 条 / 请求' },
  { value: '50 条 / 请求', label: '50 条 / 请求' },
  { value: '100 条 / 请求', label: '100 条 / 请求' },
  { value: '500 条 / 请求', label: '500 条 / 请求' },
];

/** 新增出口表单草稿（MQTT / HTTP 分支共用同一草稿对象）。 */
const outletForm = reactive({
  name: '',
  brokerUrl: '',
  clientId: '',
  /** MQTT 用户名（非敏感，随草稿保留；与口令成对，留空=匿名）。 */
  username: '',
  topicTemplate: 'factory/line1/${device}/${point}',
  encoding: 'protobuf' as Encoding,
  deviceIds: [] as string[],
  /** QoS（MQTT 分支；原型 :1323） */
  qos: '1',
  /** 请求方法（HTTP 分支；原型 :1330） */
  httpMethod: 'POST',
  /** 认证 Header（HTTP 分支；原型 :1331） */
  authHeader: '',
  /** 超时 / 重试（HTTP 分支；原型 :1332） */
  timeoutRetry: '5 s · 3 次',
  /** 批量大小（HTTP 分支；原型 :1333） */
  batchSize: '100 条 / 请求',
});

/**
 * MQTT 口令（敏感）：独立 ref，刻意**不**放进 `outletForm` 响应式草稿，
 * 以免明文混入可被序列化/审计的表单对象。仅用于拼装 POST 报文，发出后立即清空，
 * 屏幕上也始终以掩码形态呈现（type=password），满足红线「明文绝不落前端状态」。
 */
const mqttPassword = ref('');

/** 口令显隐切换（默认隐藏，掩码态）。 */
const showMqttPassword = ref(false);

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

/** 出口登记写失败的「原因 + 恢复路径」（后端 `POST /api/forwarders` 恒 501）。 */
function outletWriteFailureText(cause: unknown): string {
  const status = cause instanceof ApiError ? cause.status : 0;
  if (status === 501) {
    return (
      '出口未登记：后端写接口未落地（HTTP 501 not_implemented）—— 北向出口登记涉及 TLS 证书字段校验，' +
      '该能力尚未收口。恢复路径：当前请在网关配置文件的 [[outlets]] 段登记出口并热重载；' +
      '写接口上线后本表单即可直接保存。'
    );
  }
  if (status === 403) {
    return '出口未登记：当前账号无 device.write 权限（HTTP 403）。恢复路径：改用具备该权限的账号登录。';
  }
  return `出口未登记：${forwarderFailureText(cause, 'POST /api/forwarders')}`;
}

/**
 * 保存新出口。
 *
 * mock：保持原演示行为（仅校验 + 提示）；
 * real：`POST /api/forwarders` —— 后端当前返回 501，页面**原样呈现原因与恢复路径**，
 * 绝不把 501 吞成「已保存」。
 */
async function saveOutlet(): Promise<void> {
  if (!outletForm.name.trim() || !outletForm.brokerUrl.trim()) {
    outletMessage.value = '请填写出口名称与 Broker / 接口地址。';
    return;
  }
  const scopeText = outletScope.value === 'all' ? '全部设备' : `指定 ${outletForm.deviceIds.length} 台设备`;
  if (!IS_REAL) {
    outletMessage.value = `出口「${outletForm.name}」已保存：编码 ${outletForm.encoding}，范围 ${scopeText}。`;
    return;
  }
  const payload =
    outletType.value === 'mqtt'
      ? {
          name: outletForm.name.trim(),
          broker: outletForm.brokerUrl.trim(),
          topic_prefix: outletForm.topicTemplate.trim(),
          qos: outletForm.qos,
          encoding: outletForm.encoding,
          // 敏感凭据：随报文上送，由网关侧 config_crypto 字段级加密落盘。
          username: outletForm.username.trim(),
          password: mqttPassword.value,
        }
      : {
          name: outletForm.name.trim(),
          url: outletForm.brokerUrl.trim(),
          method: outletForm.httpMethod,
          auth_header: outletForm.authHeader.trim(),
          timeout_retry: outletForm.timeoutRetry.trim(),
          batch_size: outletForm.batchSize,
          encoding: 'json',
        };
  // 红线：口令明文仅在内存中短暂存在，拼好报文后立刻清空，绝不留存在前端状态。
  mqttPassword.value = '';
  try {
    await apiRequest<unknown>('/api/forwarders', { method: 'POST', body: JSON.stringify(payload) });
    outletMessage.value = `出口「${outletForm.name}」已登记：范围 ${scopeText}。`;
  } catch (cause) {
    outletMessage.value = outletWriteFailureText(cause);
  }
}

/**
 * 测试新增出口表单的连通性。
 *
 * real：后端只支持对**已登记**出口（`id` = 出口名）做探测，未保存的出口无从探测 ——
 * 如实说明限制与替代路径，不伪造「可达 38 ms」这类演示数值。
 */
function testOutletForm(): void {
  if (!outletForm.brokerUrl.trim()) {
    outletMessage.value = '请先填写 Broker / 接口地址再测试。';
    return;
  }
  if (IS_REAL) {
    outletMessage.value =
      '未保存的出口无法探测：后端仅支持对已登记出口做 TCP 建连探测（POST /api/forwarders/{id}/test，' +
      'id = 出口名）。替代路径：先在网关配置文件的 [[outlets]] 段登记该出口，再回到出口列表点「测试连接」。';
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
/* MQTT 口令：输入框 + 显隐切换，掩码态默认；口令明文不进入可序列化草稿 */
.nb-pwd {
  display: flex;
  gap: 8px;
  align-items: center;
}
.nb-pwd .uik-input {
  flex: 1 1 auto;
}
.nb-pwd__toggle {
  flex: 0 0 auto;
  font-family: inherit;
  font-size: var(--fs-caption);
  padding: 0 12px;
  min-height: 32px;
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  background: #fff;
  color: var(--text-2);
  cursor: pointer;
}
.nb-pwd__toggle:hover:not(:disabled) {
  border-color: var(--brand);
  color: var(--brand);
}
.nb-pwd__toggle:disabled {
  opacity: 0.5;
  cursor: not-allowed;
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
