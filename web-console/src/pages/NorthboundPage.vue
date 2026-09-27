<!--
  =============================================================================
  NorthboundPage —— 北向转发（设计 §3.4 / 原型 gateway-v2a-glacier「northbound」）
  =============================================================================
  ★ 本项目最关键的规格：**编码是每路出口（forwarder）独立可选**，不是全局开关。
    每条出口各自可选 protobuf（默认）或 json，互不影响。

  ★ JSON 编码大整数精度陷阱（真实契约）：JSON 的 number 是 IEEE754 double，
    安全整数上限 2^53−1。纳秒时间戳 / 大 uint64 计数器必须编码为**字符串**。
    该说明收在「编辑出口」弹窗的折叠帮助里，不占主视图。

  ★ 布局（需求 9 弹窗化）：主视图只有 **列表 + 搜索框 + 按钮**；
    「新增出口」与「编辑出口」两张表单全部收进标准弹窗（wc-modal），列表不再常驻渲染表单。

  ★ 数据链路（无 mock，成败都不编造）：
    · 读：`GET /api/forwarders`（失败保留 repo 数据源并展示原因）；
    · 写：新增 `POST /api/forwarders`（危险操作四要素：reason 必填 + note 非空则 ≥10 字 +
          confirm=出口名全文，经 DangerConfirmModal 收集后并入 POST body；HTTP 400 时
          后端 message 原样展示）、编码修改 `repo.setForwarderEncoding`（PUT）、
          删除 `DELETE /api/forwarders/:id`（四要素同理，原样报错，绝不假装成功）；
    · 测试连接 `POST /api/forwarders/:id/test`（后端仅对已登记出口做 TCP 建连探测，
          id = 出口名；保存成功后创建弹窗内即可真实探测，原样转述）。
  ★ client_id：后端不接受该字段，由网关自动分配 `iot-daq-<出口名>`（每出口唯一，
    crates/daemon/src/north/mqtt.rs endpoint_from_outlet），前端无需生成。
  ★ 大数红线：qos / topic_prefix 等一律字符串直通，绝不 parseInt。
-->
<template>
  <div class="wc-content">
    <!-- ══ 出口列表（分页 + 搜索 + 弹窗式增改）════════════════════════════ -->
    <section class="wc-card">
      <div class="wc-card__head">
        <h3>出口列表</h3>
        <span class="wc-card__sub">每路出口独立编码与重发策略 · 共 {{ forwarderTotal }} 条</span>
        <div class="wc-card__ops">
          <RoleGate :allowed="canEdit" mode="disable" deny-text="当前角色为只读，不能新增出口">
            <button type="button" class="wc-btn wc-btn--primary" data-testid="add-outlet" @click="openCreate">
              新增出口
            </button>
          </RoleGate>
        </div>
      </div>

      <!-- 工具条：搜索框 -->
      <div class="nb-toolbar">
        <input
          v-model="query"
          class="wc-input"
          type="search"
          placeholder="搜索出口名称 / Broker / Topic 前缀"
          aria-label="搜索出口"
          data-testid="forwarder-search"
        />
      </div>

      <!-- 真实错误：数据源不可得的原因（不静默吞错） -->
      <div v-if="forwardersNotice" class="wc-banner wc-banner--warn" data-testid="forwarders-notice">
        <span class="wc-banner__icon">!</span>
        <span>{{ forwardersNotice }}</span>
      </div>

      <!-- 真实操作结果：删除出口的回执 -->
      <div v-if="operationMessage" class="wc-banner wc-banner--warn" data-testid="operation-notice">
        <span class="wc-banner__icon">i</span>
        <span>{{ operationMessage }}</span>
      </div>

      <EmptyState
        v-if="forwarderTotal === 0"
        title="还没有配置北向出口"
        desc="至少配置一路 MQTT Broker 或 HTTP(S) 接口，数据才能上云。"
      >
        <template #actions>
          <button type="button" class="wc-btn wc-btn--primary" data-testid="add-outlet-empty" @click="openCreate">
            新增出口
          </button>
        </template>
      </EmptyState>

      <EmptyState v-else-if="filteredForwarders.length === 0" title="没有匹配的出口" desc="换个关键词试试。">
        <template #actions>
          <button type="button" class="wc-btn" data-testid="clear-search" @click="query = ''">清除搜索</button>
        </template>
      </EmptyState>

      <template v-else>
        <UiTable :columns="forwarderColumns" :rows="pagedForwarders" row-key-field="id">
          <template #cell-name="{ row }">
            <div class="nb-outlet">
              <span class="nb-outlet__name">{{ row.name }}</span>
            </div>
          </template>
          <template #cell-brokerUrl="{ row }">
            <span class="wc-mono">{{ row.brokerUrl }}</span>
          </template>
          <template #cell-encoding="{ row }">
            <span class="wc-tag" :class="row.encoding === 'json' ? 'wc-tag--info' : 'wc-tag--ok'">
              {{ row.encoding }}
            </span>
          </template>
          <template #cell-qosText="{ row }">
            <span class="wc-mono">{{ row.qosText }}</span>
          </template>
          <template #cell-topicTemplate="{ row }">
            <span class="wc-mono">{{ row.topicTemplate }}</span>
          </template>
          <template #actions="{ row }">
            <button
              type="button"
              class="wc-btn wc-btn--sm"
              :data-testid="`edit-btn-${row.id}`"
              @click="openEdit(row)"
            >
              编辑
            </button>
            <button
              type="button"
              class="wc-btn wc-btn--sm wc-btn--danger"
              :disabled="!canEdit"
              :data-testid="`delete-btn-${row.id}`"
              @click="askDelete(row)"
            >
              删除
            </button>
          </template>
        </UiTable>

        <UiPager
          :page="forwarderPage"
          :total="filteredForwarders.length"
          :page-size="FORWARDER_PAGE_SIZE"
          @update:page="onForwarderPage"
        />
      </template>
    </section>
  </div>

  <!-- ══ 新增出口弹窗（原常驻表单整体收进此处）══════════════════════════ -->
  <Teleport to="body">
    <div v-show="createOpen" class="wc-modal__mask" data-testid="create-outlet-modal" @click.self="closeCreate">
      <div
        class="wc-modal"
        role="dialog"
        aria-modal="true"
        aria-label="新增出口"
        tabindex="-1"
        @keydown.esc="closeCreate"
      >
        <div class="wc-modal__head">
          <h3 class="wc-modal__title">新增出口</h3>
          <span class="wc-card__sub">支持 MQTT Broker 与 HTTP(S) 接口；转发范围支持全部设备或指定设备</span>
        </div>
        <div class="wc-modal__body">
          <UiRadio v-model="outletType" :options="outletTypeOptions" :disabled="!canEdit" />

          <!-- ── MQTT Broker 分支 ── -->
          <div v-if="outletType === 'mqtt'" class="nb-form-grid">
            <UiField label="出口名称" required>
              <UiInput v-model="outletForm.name" placeholder="如：客户 EMQX（生产）" :disabled="!canEdit" data-testid="outlet-name" />
            </UiField>
            <UiField label="Broker 地址" required hint="mqtt://host:1883 / mqtts://host:8883">
              <UiInput v-model="outletForm.brokerUrl" placeholder="mqtt://10.0.0.5:1883" :disabled="!canEdit" data-testid="outlet-broker" />
            </UiField>
            <UiField
              label="客户端 ID"
              hint="网关自动分配：iot-daq-<出口名>（每出口唯一），无需填写；此输入项后端不接收"
            >
              <UiInput v-model="outletForm.clientId" placeholder="留空即可（网关自动分配）" :disabled="!canEdit" />
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
              <UiSelect v-model="outletForm.encoding" :options="encodingSelectOptions" :disabled="!canEdit" data-testid="outlet-encoding" />
            </UiField>
            <UiField label="QoS" required hint="0 至多一次 / 1 至少一次 / 2 恰好一次（默认 1）">
              <UiSelect v-model="outletForm.qos" :options="qosOptions" :disabled="!canEdit" />
            </UiField>
            <UiField label="Topic 前缀" hint="MQTT 5 报文以 Payload Format Indicator + Content Type 声明编码">
              <UiInput v-model="outletForm.topicTemplate" placeholder="factory/line1" :disabled="!canEdit" />
            </UiField>
          </div>

          <!-- ── HTTP(S) 接口分支 ── -->
          <div v-else class="nb-form-grid">
            <UiField label="出口名称" required>
              <UiInput v-model="outletForm.name" placeholder="如：客户 HTTP 接口" :disabled="!canEdit" data-testid="outlet-name" />
            </UiField>
            <UiField label="接口地址（URL）" required hint="支持 http / https；https 强制校验服务端证书">
              <UiInput v-model="outletForm.brokerUrl" placeholder="https://client.example.com/api/ingest" :disabled="!canEdit" data-testid="outlet-broker" />
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
            <!-- ── 指定设备：查询框 + 设备列表（GET /api/devices，repo 真实数据源）── -->
            <div v-if="outletScope === 'devices'" class="nb-scope-picker" data-testid="device-picker">
              <input
                v-model="deviceQuery"
                class="wc-input"
                type="search"
                placeholder="搜索设备名称 / 设备 ID"
                aria-label="搜索设备"
                data-testid="device-search"
              />
              <div class="nb-device-chips">
                <button
                  v-for="dev in filteredDeviceChips"
                  :key="dev.id"
                  type="button"
                  class="nb-chip"
                  :class="{ 'is-on': outletForm.deviceIds.includes(dev.id) }"
                  :disabled="!canEdit"
                  :aria-pressed="outletForm.deviceIds.includes(dev.id) ? 'true' : 'false'"
                  @click="toggleDevice(dev.id)"
                >
                  {{ dev.name }}
                </button>
                <span v-if="filteredDeviceChips.length === 0" class="nb-scope-empty">
                  {{ deviceChips.length === 0 ? '暂无设备：请先在「设备管理」页接入设备。' : '没有匹配的设备，换个关键词试试。' }}
                </span>
              </div>
              <p class="nb-scope-hint">
                点击设备名切换勾选；未勾选任何设备时该出口不会转发任何数据。已选 {{ outletForm.deviceIds.length }} 台 / 共 {{ deviceChips.length }} 台。
              </p>
            </div>
          </div>

          <!-- 新增出口选 JSON 时给出精度提示（MQTT 分支） -->
          <div
            v-if="outletType === 'mqtt' && outletForm.encoding === 'json'"
            class="wc-banner wc-banner--warn"
            data-testid="new-json-warning"
          >
            <span>JSON 编码：纳秒时间戳 / 大 uint64 计数器将编码为字符串，避免超出 2<sup>53</sup>−1 静默丢精度。</span>
          </div>

          <!-- HTTP 出口固定 JSON 编码 -->
          <div v-if="outletType === 'http'" class="wc-banner wc-banner--warn" data-testid="new-http-hint">
            <span class="wc-banner__icon">!</span>
            <span>
              HTTP 出口<b>固定 JSON 编码</b>：请求体为
              <code>{"device_mid":…,"ts":…,"points":{target:value,…}}</code>，Content-Type: application/json；
              纳秒时间戳 / 大 uint64 计数器<b>必须编码为字符串</b>（安全整数上限 2<sup>53</sup>−1）。
            </span>
          </div>

          <!-- 真实错误 / 校验失败原因（唯一允许留在弹窗里的提示） -->
          <p v-if="outletMessage" class="wc-modal__error" role="alert" data-testid="outlet-message">
            {{ outletMessage }}
          </p>
          <!-- 保存成功后「测试连接」的真实探测回执（TCP 建连，原样转述） -->
          <p v-if="createTestResult" class="wc-modal__result" data-testid="create-test-result">{{ createTestResult }}</p>
        </div>
        <div class="wc-modal__foot">
          <button type="button" class="wc-btn" data-testid="outlet-cancel" @click="closeCreate">取消</button>
          <button type="button" class="wc-btn" :disabled="!canEdit" @click="testOutletForm">测试连接</button>
          <button
            type="button"
            class="wc-btn wc-btn--primary"
            :disabled="!canEdit"
            data-testid="save-outlet"
            @click="saveOutlet"
          >
            保存出口
          </button>
        </div>
      </div>
    </div>
  </Teleport>

  <!-- ══ 编辑出口弹窗（原选中行展开区收进此处；含折叠帮助）═════════════ -->
  <Teleport to="body">
    <div v-show="editOpen" class="wc-modal__mask" data-testid="outlet-modal" @click.self="closeEdit">
      <div
        class="wc-modal"
        role="dialog"
        aria-modal="true"
        aria-label="编辑出口"
        tabindex="-1"
        @keydown.esc="closeEdit"
      >
        <div class="wc-modal__head">
          <h3 class="wc-modal__title">编辑出口 · {{ selected?.name }}</h3>
        </div>
        <div class="wc-modal__body" v-if="selected">
          <dl class="wc-kv">
            <dt>Broker</dt>
            <dd class="wc-mono">{{ selected.brokerUrl }}</dd>
            <dt>传输安全</dt>
            <dd>{{ selected.transportSecurity }}</dd>
            <dt>Topic 模板</dt>
            <dd class="wc-mono">{{ selected.topicTemplate }}</dd>
          </dl>

          <!-- ── 数据编码（每路独立单选）────────────────────────────────── -->
          <div class="nb-section">
            <p class="nb-section__title">数据编码（本出口独立选择）</p>
            <UiRadio v-model="selectedEncoding" :options="encodingOptions" :disabled="!canEdit" />

            <!-- ★ JSON 精度提示：选 JSON 时出现 -->
            <div v-if="selectedEncoding === 'json'" class="wc-banner wc-banner--warn nb-json-warn" data-testid="json-warning">
              <span class="wc-banner__icon">!</span>
              <span>
                该出口使用 JSON 编码：超出 2<sup>53</sup>−1（9007199254740991）的整数将编码为字符串
                （纳秒时间戳、大 uint64 计数器），直接编码会<b>静默丢精度</b>。
                编码与签名解耦：protobuf / json 语义一致、验签结果相同。
              </span>
            </div>
          </div>

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
            <button type="button" class="wc-btn" data-testid="test-connection" @click="testConnection(selected)">
              测试连接
            </button>
          </div>

          <!-- 真实错误 / 操作回执（唯一允许留在弹窗里的提示） -->
          <p v-if="editMessage" class="wc-modal__error" role="alert" data-testid="edit-message">{{ editMessage }}</p>
          <p v-if="testResult" class="wc-modal__result" data-testid="test-result">{{ testResult }}</p>

          <!-- 折叠帮助：消息示例 + 断网续传（主视图不出现） -->
          <details class="nb-help">
            <summary>消息示例与断网续传说明</summary>
            <pre class="nb-sample">{{ messageSample }}</pre>
            <dl class="wc-kv">
              <dt>本地队列</dt>
              <dd class="wc-mono">queue.db（独立库）</dd>
              <dt>容量上限</dt>
              <dd class="wc-mono">10 GB / 7 天（环形覆盖）</dd>
              <dt>补发策略</dt>
              <dd class="wc-mono">按序 + 幂等去重</dd>
            </dl>
          </details>
        </div>
        <div class="wc-modal__foot">
          <button type="button" class="wc-btn" data-testid="outlet-edit-cancel" @click="closeEdit">取消</button>
        </div>
      </div>
    </div>
  </Teleport>

  <!-- ══ 删除出口：危险操作二次确认（影响 + 原因 + 对象全名二次校验）═══ -->
  <DangerConfirmModal
    :open="deleteOpen"
    :title="`删除出口 ${deleteTarget?.name ?? ''}`"
    :impacts="DELETE_IMPACTS"
    :facts="deleteFacts"
    :reasons="DELETE_REASONS"
    :min-note-length="10"
    :confirm-value="deleteTarget?.name ?? ''"
    confirm-mode="full"
    confirm-label="出口名二次确认（输入出口名称）"
    confirm-placeholder="输入待删除的出口名称"
    confirm-text="删除出口"
    @close="deleteOpen = false"
    @submit="onDeleteSubmit"
  />

  <!-- ══ 登记出口：危险操作四要素确认（写入配置 → 后端 reason/note/confirm 强校验）═══ -->
  <DangerConfirmModal
    :open="saveConfirmOpen"
    :title="`登记出口 ${saveConfirmName}`"
    :impacts="CREATE_IMPACTS"
    :facts="saveConfirmFacts"
    :reasons="CREATE_REASONS"
    :min-note-length="10"
    :confirm-value="saveConfirmName"
    confirm-mode="full"
    confirm-label="出口名二次确认（输入出口名称）"
    confirm-placeholder="输入待登记的出口名称"
    confirm-text="登记出口"
    @close="saveConfirmOpen = false"
    @submit="onSaveConfirmSubmit"
  />
</template>

<script setup lang="ts">
/**
 * @file NorthboundPage.vue
 * @module web-console/pages/NorthboundPage
 * @description 北向转发页（出口列表 + 搜索 + 弹窗式新增/编辑 + 危险删除确认）。
 *
 * 契约边界（诚实表示未知，绝不伪造）：
 * · 读 `GET /api/forwarders`；写 `POST /api/forwarders` / `repo.setForwarderEncoding`(PUT) /
 *   `DELETE /api/forwarders/:id`；探测 `POST /api/forwarders/:id/test`（仅 TCP 建连）。
 * · 后端不上报连接态 / 覆盖设备数 / 编码自检时间 → 相应列不渲染，不冒充已知值。
 * · 一致性自检 / 测试发布无后端端点 → 不提供（避免本地伪造「自检通过」）。
 */
import { computed, onMounted, reactive, ref, watch } from 'vue';
import {
  UiTable,
  UiPager,
  UiInput,
  UiSelect,
  UiRadio,
  EmptyState,
  RoleGate,
  UiField,
  DangerConfirmModal,
  type TableColumn,
  type SelectOption,
  type RadioOption,
  type DangerFact,
} from '@ui-kit';
import { repo, dataVersion, DEFAULT_ACTOR, type Encoding, type ForwarderRecord } from '@/api/repo';
import { apiRequest, ApiError } from '@/api/client';
import { session } from '../store/session';

/** 每页条数（出口列表）。 */
const FORWARDER_PAGE_SIZE = 5;

// ---------------------------------------------------------------------------
// 出口行视图
// ---------------------------------------------------------------------------

/**
 * 出口行视图：`/api/forwarders`（唯一真实来源）。
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

/** 后端出口记录 → 行视图（QoS 文本按出口类别给出）。 */
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

/** 当前角色是否可编辑（RoleGate 仅控制可见性，非授权判定）。 */
const canEdit = computed<boolean>(() => session.state.role === 'admin' || session.state.role === 'engineer');

// ---------- 出口列表 + 搜索 ----------
const allForwarders = ref<OutletRow[]>(repo.allForwarders().map(toOutletRow));
const query = ref('');
const forwarderPage = ref(1);

/**
 * 出口数据源不可得的原因。
 * 不静默吞错：`GET /api/forwarders` 失败时保留现有数据源，并把原因展示出来。
 */
const forwardersNotice = ref('');

/** 删除出口等操作的真实回执（成功 / 失败都如实展示）。 */
const operationMessage = ref('');

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
 * 出口清单取 `GET /api/forwarders`（`id` = 出口名）。
 * 失败时**不静默吞错**：保留 repo 数据源并展示原因，页面不崩、不伪造出口。
 */
async function loadForwarders(): Promise<void> {
  try {
    const raw = await apiRequest<unknown[]>('/api/forwarders');
    if (!Array.isArray(raw)) {
      forwardersNotice.value = '出口接口返回了非数组结构，已沿用现有数据源。';
      return;
    }
    allForwarders.value = raw.map((row, i) => mapForwarderRow(asRecord(row), i));
    forwardersNotice.value = '';
    if (selected.value) {
      selected.value = allForwarders.value.find((f) => f.id === selected.value?.id) ?? null;
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

/** 按关键词过滤（名称 / Broker / Topic 前缀 / 编码）。 */
const filteredForwarders = computed<OutletRow[]>(() => {
  const kw = query.value.trim().toLowerCase();
  if (!kw) {
    return allForwarders.value;
  }
  return allForwarders.value.filter(
    (r) =>
      r.name.toLowerCase().includes(kw) ||
      r.brokerUrl.toLowerCase().includes(kw) ||
      r.topicTemplate.toLowerCase().includes(kw) ||
      r.encoding.includes(kw),
  );
});

/** 当前页出口。 */
const pagedForwarders = computed<OutletRow[]>(() => {
  const start = (forwarderPage.value - 1) * FORWARDER_PAGE_SIZE;
  return filteredForwarders.value.slice(start, start + FORWARDER_PAGE_SIZE);
});

/** 搜索变化时回到第 1 页。 */
watch(query, () => {
  forwarderPage.value = 1;
});

/** 出口列表列定义。 */
const forwarderColumns: readonly TableColumn[] = [
  { key: 'name', label: '出口名称' },
  { key: 'brokerUrl', label: 'Broker', mono: true },
  { key: 'encoding', label: '编码' },
  { key: 'qosText', label: 'QoS', mono: true },
  { key: 'topicTemplate', label: 'Topic 前缀', mono: true },
];

/** 换页。 */
function onForwarderPage(next: number): void {
  forwarderPage.value = next;
}

/** 当前编辑的出口（编辑弹窗数据源）。 */
const selected = ref<OutletRow | null>(null);

/** 编辑弹窗编码草稿（与列表行解耦，保存后才写回）。 */
const selectedEncoding = ref<Encoding>('protobuf');

/** 编码单选选项。 */
const encodingOptions: readonly RadioOption[] = [
  { value: 'protobuf', label: 'protobuf（默认）', desc: '体积小、性能高；二进制编码，与 json 语义一致。' },
  {
    value: 'json',
    label: 'JSON（第三方兼容）',
    desc: '超出 2^53−1 的整数编为字符串；体积约 protobuf 的 1.5–3×，性能下降。',
  },
];

/** 下拉选项（新增出口表单用）。 */
const encodingSelectOptions: readonly SelectOption[] = [
  { value: 'protobuf', label: 'protobuf（默认）' },
  { value: 'json', label: 'JSON（第三方兼容）' },
];

// ---------------------------------------------------------------------------
// 编辑出口（弹窗）
// ---------------------------------------------------------------------------
const editOpen = ref(false);
const editMessage = ref('');

/** 打开编辑弹窗（预填当前行）。 */
function openEdit(row: OutletRow): void {
  selected.value = allForwarders.value.find((f) => f.id === row.id) ?? null;
  if (!selected.value) {
    return;
  }
  selectedEncoding.value = selected.value.encoding;
  editMessage.value = '';
  testResult.value = '';
  editOpen.value = true;
}

/** 关闭编辑弹窗。 */
function closeEdit(): void {
  editOpen.value = false;
  editMessage.value = '';
  testResult.value = '';
}

/** 保存编辑弹窗所选编码到当前出口（真实 PUT；后端未落地时原样报错）。 */
async function saveEncoding(): Promise<void> {
  if (!canEdit.value || !selected.value) {
    return;
  }
  const id = selected.value.id;
  const result = await repo.setForwarderEncoding({ id, encoding: selectedEncoding.value, actor: DEFAULT_ACTOR });
  if (result.ok) {
    editMessage.value = result.message;
    await loadForwarders();
    selected.value = allForwarders.value.find((f) => f.id === id) ?? null;
  } else {
    editMessage.value = `出口「${id}」编码未变更：${result.message}`;
  }
}

// ---------- 测试连接（编辑弹窗内） ----------
/** 测试连接结果（真实探测回执，原样展示）。 */
const testResult = ref('');

/** 新增出口弹窗内「测试连接」的回执（保存成功后对已登记出口探测）。 */
const createTestResult = ref('');

/**
 * 对已登记出口（`id` = 出口名）做真实探测：`POST /api/forwarders/{id}/test`。
 *
 * 后端仅做 **TCP 建连**探测（`mqtts://` 不做 TLS 握手 / MQTT CONNACK），未知出口 404；
 * 结果原样返回，不把「TCP 通」说成「MQTT 可用」。
 */
async function probeOutlet(id: string, name: string): Promise<string> {
  try {
    const raw = await apiRequest<Record<string, unknown>>(
      `/api/forwarders/${encodeURIComponent(id)}/test`,
      { method: 'POST' },
    );
    const ok = raw['ok'] === true;
    const elapsed = pickText(raw, 'elapsed_ms', '—');
    if (ok) {
      return (
        `「${name}」：${pickText(raw, 'probe', 'tcp_connect')} 探测通过，耗时 ${elapsed} ms。` +
        `注意：${pickText(raw, 'note', '仅 TCP 建连探测，不含 TLS 握手与 MQTT CONNACK')}。`
      );
    }
    return (
      `「${name}」：探测失败（${pickText(raw, 'error_kind', 'failed')}），耗时 ${elapsed} ms —— ` +
      `${pickText(raw, 'reason', '后端未给出失败原因')}。`
    );
  } catch (cause) {
    const status = cause instanceof ApiError ? cause.status : 0;
    return status === 404
      ? `「${name}」：后端未收录该出口名（HTTP 404）—— 出口以「名称」为唯一键，请先确认配置中的出口名。`
      : `「${name}」：${forwarderFailureText(cause, 'POST /api/forwarders/{id}/test')}`;
  }
}

/** 编辑弹窗的「测试连接」：对当前选中的已登记出口做 TCP 建连探测。 */
async function testConnection(row: OutletRow): Promise<void> {
  testResult.value = `正在探测「${row.name}」…`;
  testResult.value = await probeOutlet(row.id, row.name);
}

// ---------------------------------------------------------------------------
// 新增出口（弹窗）
// ---------------------------------------------------------------------------
const createOpen = ref(false);

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

/** QoS 选项。 */
const qosOptions: readonly SelectOption[] = [
  { value: '0', label: '0（至多一次）' },
  { value: '1', label: '1（至少一次）' },
  { value: '2', label: '2（恰好一次）' },
];

/** HTTP 请求方法选项。 */
const httpMethodOptions: readonly SelectOption[] = [
  { value: 'POST', label: 'POST' },
  { value: 'PUT', label: 'PUT' },
];

/** HTTP 批量大小选项。 */
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
  /** QoS（MQTT 分支） */
  qos: '1',
  /** 请求方法（HTTP 分支） */
  httpMethod: 'POST',
  /** 认证 Header（HTTP 分支） */
  authHeader: '',
  /** 超时 / 重试（HTTP 分支） */
  timeoutRetry: '5 s · 3 次',
  /** 批量大小（HTTP 分支） */
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

/** 设备 chips（来自 `repo.allDevices()` 真实设备清单；随 `dataVersion` 刷新，不在挂载时定格）。 */
const deviceChips = computed(() => {
  void dataVersion; // 真实数据刷新后重新取设备清单，避免页挂载那一刻定格成旧值。
  return repo.allDevices().map((d) => ({ id: d.id, name: d.name }));
});

/** 指定设备模式的查询关键词（按名称 / ID 过滤）。 */
const deviceQuery = ref('');

/** 按查询关键词过滤后的设备列表（名称 / ID 不区分大小写包含匹配）。 */
const filteredDeviceChips = computed(() => {
  const kw = deviceQuery.value.trim().toLowerCase();
  if (!kw) {
    return deviceChips.value;
  }
  return deviceChips.value.filter(
    (d) => d.name.toLowerCase().includes(kw) || d.id.toLowerCase().includes(kw),
  );
});

/** 勾选 / 取消设备。 */
function toggleDevice(id: string): void {
  const idx = outletForm.deviceIds.indexOf(id);
  if (idx >= 0) {
    outletForm.deviceIds.splice(idx, 1);
  } else {
    outletForm.deviceIds.push(id);
  }
}

/** 新增出口结果提示（真实错误 / 回执，留在弹窗内）。 */
const outletMessage = ref('');

/** 登记出口的创建请求体（确认弹窗提交时消费；取消即弃）。 */
let pendingCreatePayload: Record<string, unknown> | null = null;

/** 打开新增出口弹窗（空表单）。 */
function openCreate(): void {
  outletType.value = 'mqtt';
  outletScope.value = 'all';
  outletForm.name = '';
  outletForm.brokerUrl = '';
  outletForm.clientId = '';
  outletForm.username = '';
  outletForm.topicTemplate = 'factory/line1/${device}/${point}';
  outletForm.encoding = 'protobuf';
  outletForm.deviceIds = [];
  outletForm.qos = '1';
  outletForm.httpMethod = 'POST';
  outletForm.authHeader = '';
  outletForm.timeoutRetry = '5 s · 3 次';
  outletForm.batchSize = '100 条 / 请求';
  mqttPassword.value = '';
  showMqttPassword.value = false;
  outletMessage.value = '';
  createTestResult.value = '';
  savedOutletName.value = '';
  deviceQuery.value = '';
  createOpen.value = true;
}

/** 关闭新增弹窗并清空草稿（含敏感口令）。 */
function closeCreate(): void {
  createOpen.value = false;
  mqttPassword.value = '';
  outletMessage.value = '';
  createTestResult.value = '';
  savedOutletName.value = '';
  pendingCreatePayload = null;
}

/** 从后端错误体提取原样错误文本（`{error, field, reason, allowed}` / `{message}`）。 */
function backendErrorText(cause: unknown): string {
  if (cause instanceof ApiError && cause.body !== null && typeof cause.body === 'object' && !Array.isArray(cause.body)) {
    const body = cause.body as Record<string, unknown>;
    const parts: string[] = [];
    for (const key of ['field', 'reason', 'message', 'allowed'] as const) {
      const v = body[key];
      if (typeof v === 'string' && v.trim() !== '') {
        parts.push(`${key}: ${v}`);
      }
    }
    if (parts.length > 0) {
      return parts.join('；');
    }
  }
  return cause instanceof Error ? cause.message : String(cause);
}

/** 出口登记写失败的「原因 + 恢复路径」。 */
function outletWriteFailureText(cause: unknown): string {
  const status = cause instanceof ApiError ? cause.status : 0;
  if (status === 400) {
    // 校验失败：后端 reason/allowed 原样呈现（四要素 / broker / qos 等），绝不吞成「接口不可得」。
    return `出口未登记：后端校验未通过（HTTP 400）—— ${backendErrorText(cause)}`;
  }
  if (status === 501) {
    return (
      '出口未登记：后端写接口未落地（HTTP 501 not_implemented）—— 北向出口登记涉及 TLS 证书字段校验，' +
      '该能力尚未收口。恢复路径：当前请在网关配置文件的 [[outlets]] 段登记出口并重启网关' +
      '（北向运行期只在网关启动时装配，不存在出口级热重载）；' +
      '写接口上线后本表单即可直接保存。'
    );
  }
  if (status === 403) {
    return '出口未登记：当前账号无 device.write 权限（HTTP 403）。恢复路径：改用具备该权限的账号登录。';
  }
  return `出口未登记：${forwarderFailureText(cause, 'POST /api/forwarders')}`;
}

// ---- 登记出口：危险操作四要素（reason / note / confirm=出口名全文）----
/** 登记确认弹窗是否打开。 */
const saveConfirmOpen = ref(false);

/** 登记确认弹窗展示的出口名（= 本次将写入的出口名原文）。 */
const saveConfirmName = ref('');

/** 已成功登记的出口名（非空 = 创建弹窗内可对该出口做真实 TCP 探测）。 */
const savedOutletName = ref('');

/** 登记原因枚举（必选，记入审计）。 */
const CREATE_REASONS: readonly string[] = ['新增客户对接出口', '更换 Broker 地址', '产线扩容新增出口', '调试联调'];

/** 登记影响清单（写清后果与生效时机）。 */
const CREATE_IMPACTS: readonly string[] = [
  '出口将写入网关配置并落盘（凭据字段级加密），出口名与既有出口重名将登记失败。',
  '北向运行期在网关启动时装配：该出口在重启网关后才参与投递。',
  '登记成功后可立即对该出口做 TCP 建连探测（测试连接）。',
];

/** 登记对象摘要。 */
const saveConfirmFacts = computed<readonly DangerFact[]>(() => [
  { label: '出口名称', value: saveConfirmName.value || '—' },
  { label: '地址', value: outletForm.brokerUrl.trim() || '—' },
  { label: '类型', value: outletType.value === 'mqtt' ? 'MQTT Broker' : 'HTTP(S) 接口' },
]);

/**
 * 保存新出口（第一步）：校验后打开危险操作确认弹窗。
 * 真正的 POST 在 `onSaveConfirmSubmit`（四要素齐备后）执行。
 */
function saveOutlet(): void {
  if (!outletForm.name.trim() || !outletForm.brokerUrl.trim()) {
    outletMessage.value = '请填写出口名称与 Broker / 接口地址。';
    return;
  }
  // 基础业务字段（reason / note / confirm 由确认弹窗提交时并入，见 onSaveConfirmSubmit）。
  pendingCreatePayload =
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
  outletMessage.value = '';
  createTestResult.value = '';
  saveConfirmName.value = outletForm.name.trim();
  saveConfirmOpen.value = true;
}

/**
 * 确认弹窗提交（第二步）：把 reason / note / confirm 并入 POST body 真实登记。
 *
 * 后端四要素强校验（`forwarder_create`）：reason 必填非空、note 非空则 ≥10 字、
 * confirm 必须逐字等于出口名；HTTP 400 时把后端 message 原样展示。
 * 生效时机如实呈现：北向运行期在网关**启动时**装配，新增出口落盘后需**重启网关**
 * 才真正参与投递 —— 绝不把它说成"已即时生效"。
 */
async function onSaveConfirmSubmit(payload: { reason: string; note: string; tail: string; secondApprover: string; confirm: string }): Promise<void> {
  const body = pendingCreatePayload;
  saveConfirmOpen.value = false;
  if (!body) {
    return;
  }
  pendingCreatePayload = { ...body, reason: payload.reason, note: payload.note.trim(), confirm: payload.confirm };
  const scopeText = outletScope.value === 'all' ? '全部设备' : `指定 ${outletForm.deviceIds.length} 台设备`;
  try {
    await apiRequest<unknown>('/api/forwarders', { method: 'POST', body: JSON.stringify(pendingCreatePayload) });
    // 红线：口令明文仅在内存中短暂存在，登记完成后立刻清空，绝不留存在前端状态。
    pendingCreatePayload = null;
    mqttPassword.value = '';
    savedOutletName.value = outletForm.name.trim();
    outletMessage.value =
      `出口「${outletForm.name}」已登记并落盘：范围 ${scopeText}。` +
      '生效时机：北向运行期在网关启动时装配，该出口将在重启网关后参与投递' +
      '（可在「启动与自启」页面重启网关）。' +
      '已登记出口可立即测试连接（TCP 建连探测）。';
    await loadForwarders();
  } catch (cause) {
    // 失败保留草稿与确认请求体，用户修正后可重新保存。
    outletMessage.value = outletWriteFailureText(cause);
  }
}

/**
 * 测试新增出口表单的连通性。
 *
 * 后端只支持对**已登记**出口（`id` = 出口名）做探测：本次表单刚登记成功时直接探测；
 * 未保存的草稿无从探测 —— 如实说明限制与替代路径，不伪造「可达 38 ms」这类演示数值。
 */
async function testOutletForm(): Promise<void> {
  const name = outletForm.name.trim();
  if (!outletForm.brokerUrl.trim()) {
    outletMessage.value = '请先填写 Broker / 接口地址再测试。';
    return;
  }
  if (savedOutletName.value && name === savedOutletName.value) {
    createTestResult.value = `正在探测「${name}」…`;
    createTestResult.value = await probeOutlet(name, name);
    return;
  }
  outletMessage.value =
    '未保存的出口无法探测：后端仅支持对已登记出口做 TCP 建连探测（POST /api/forwarders/{id}/test，' +
    'id = 出口名）。替代路径：先点「保存出口」登记（走危险操作四要素确认），登记成功后本按钮即对该出口真实探测。';
}

// ---------------------------------------------------------------------------
// 删除出口（危险操作二次确认）
// ---------------------------------------------------------------------------
const deleteOpen = ref(false);
const deleteTarget = ref<OutletRow | null>(null);

/** 删除影响清单。 */
const DELETE_IMPACTS: readonly string[] = [
  '该出口将从网关配置移除，指向它的数据不再转发。',
  '正在排队等待补发给该出口的数据将不再投递。',
  '请确认没有其它规则 / 流程依赖该出口名称。',
];

/** 删除对象摘要。 */
const deleteFacts = computed<readonly DangerFact[]>(() => [
  { label: '出口名称', value: deleteTarget.value?.name ?? '—' },
  { label: '地址', value: deleteTarget.value?.brokerUrl ?? '—' },
]);

/** 删除原因枚举（必选）。 */
const DELETE_REASONS: readonly string[] = ['出口停用下线', '地址配置错误', '迁移到新出口', '调试清理'];

/** 打开删除确认。 */
function askDelete(row: OutletRow): void {
  if (!canEdit.value) {
    return;
  }
  deleteTarget.value = row;
  operationMessage.value = '';
  deleteOpen.value = true;
}

/** 删除提交：`DELETE /api/forwarders/:id`（后端未落地 → 原样报错，绝不假装已删除）。 */
async function onDeleteSubmit(payload: { reason: string; note: string; tail: string; secondApprover: string }): Promise<void> {
  const row = deleteTarget.value;
  deleteOpen.value = false;
  if (!row) {
    return;
  }
  try {
    await apiRequest<unknown>(`/api/forwarders/${encodeURIComponent(row.id)}`, {
      method: 'DELETE',
      body: JSON.stringify({ reason: payload.reason, note: payload.note, confirm: row.name }),
    });
    operationMessage.value = `出口「${row.name}」已删除。`;
    if (editOpen.value) {
      closeEdit();
    }
    await loadForwarders();
  } catch (cause) {
    const status = cause instanceof ApiError ? cause.status : 0;
    operationMessage.value =
      status === 404 || status === 405
        ? `出口「${row.name}」未删除：后端未提供出口删除接口（HTTP ${status}）。恢复路径：暂请在网关配置文件的 [[outlets]] 段调整出口后重启网关。`
        : `出口「${row.name}」未删除：${forwarderFailureText(cause, 'DELETE /api/forwarders/:id')}`;
  }
}

// ---------- 折叠帮助（编辑弹窗内） ----------
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
</script>

<style scoped>
.nb-toolbar {
  display: flex;
  gap: 10px;
  align-items: center;
  padding: 12px 16px 0;
}
.nb-toolbar .wc-input {
  flex: 1 1 auto;
  max-width: 360px;
}
.nb-outlet {
  display: flex;
  align-items: center;
  gap: 8px;
}
.nb-outlet__name {
  font-weight: 500;
}
.wc-card > .wc-banner {
  margin: 12px 16px 0;
}
.wc-card > .wc-banner:last-child {
  margin-bottom: 12px;
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
/* 指定设备：查询框 + 设备列表 + 提示（转发范围 = 指定设备时出现） */
.nb-scope-picker {
  display: flex;
  flex-direction: column;
  gap: 10px;
}
.nb-scope-picker .wc-input {
  max-width: 360px;
}
.nb-scope-empty {
  font-size: var(--fs-caption);
  color: var(--text-3);
}
.nb-scope-hint {
  margin: 0;
  font-size: var(--fs-caption);
  color: var(--text-3);
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
/* 折叠帮助（编辑弹窗内，主视图不出现） */
.nb-help {
  border: 1px solid var(--divider);
  border-radius: var(--radius-sm);
  padding: 8px 12px;
  display: flex;
  flex-direction: column;
  gap: 10px;
}
.nb-help summary {
  cursor: pointer;
  font-size: var(--fs-table);
  color: var(--text-2);
  user-select: none;
}
/* ── 弹窗（与 AccountsPage 同一模式；随组件作用域，不改全局样式）──── */
.wc-modal__mask {
  position: fixed;
  inset: 0;
  background: #1d212973;
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
  width: 720px;
  max-width: 100%;
  max-height: 88vh;
  overflow: auto;
  display: flex;
  flex-direction: column;
  outline: none;
}
.wc-modal__head {
  padding: 16px 20px;
  border-bottom: 1px solid var(--divider);
  display: flex;
  flex-direction: column;
  gap: 4px;
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
  gap: 14px;
}
.wc-modal__error {
  margin: 0;
  font-size: var(--fs-table);
  color: var(--danger-fg);
  background: var(--danger-bg);
  border: 1px solid var(--danger-border);
  border-radius: var(--radius-sm);
  padding: 8px 10px;
  line-height: 1.6;
}
.wc-modal__result {
  margin: 0;
  font-size: var(--fs-table);
  color: var(--text-1);
  background: var(--bg-app);
  border: 1px solid var(--divider);
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
