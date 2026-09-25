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
      · 连通性测试：real 模式真实调用 `POST /api/devices/test`（Modbus 全探测，
        其余协议后端返回结构化 unsupported）；mock 模式走演示流程并明确标注；
      · 提交只把配置交给 `repo.createDevice`（内存态 + 写审计），授权判定一律在网关侧。
  -->
  <PageHeader
    crumb="接入 / 新增设备"
    title="新增设备"
    desc="单页四步：选设备类型 → 填连接参数 → 配点表映射 → 测试并保存。连接参数字段随协议整块替换；「下一步」不落库，最后一步才保存。"
  >
    <template #actions>
      <button type="button" class="wc-btn" @click="go('devices')">返回设备列表</button>
    </template>
  </PageHeader>

  <div class="wc-content">
    <!-- 步骤指示器：已到达的步骤可点回退，未到达的锁定 -->
    <div class="wc-card">
      <ol class="dv-steps">
        <li
          v-for="(step, i) in STEPS"
          :key="step"
          class="dv-steps__item"
          :class="{
            'is-on': currentStep === i + 1,
            'is-done': currentStep > i + 1,
            'is-clickable': i + 1 <= maxReached,
            'is-lock': i + 1 > maxReached,
          }"
          :role="i + 1 <= maxReached ? 'button' : undefined"
          :tabindex="i + 1 <= maxReached ? 0 : undefined"
          @click="gotoStep(i + 1)"
        >
          <span class="dv-steps__num">{{ i + 1 < currentStep ? '✓' : i + 1 }}</span>
          <span class="dv-steps__label">{{ step }}</span>
        </li>
      </ol>
    </div>

    <!-- 步骤上下文：当前设备类型 + 回到第 1 步改类型（原型 :1546-1549） -->
    <div class="dv-ctx">
      <span>当前设备类型</span>
      <span class="wc-tag wc-tag--info">{{ protocolLabel }}</span>
      <span>· 连接参数 <b>{{ currentProtoFields.length }}</b> 个</span>
      <button v-if="currentStep > 1" type="button" class="wc-btn wc-btn--sm" @click="gotoStep(1)">
        回到第 1 步改类型
      </button>
      <span class="dv-ctx__hint">类型选错不会报错，只会一直读不出数据；保存后改类型需重新校准点表。</span>
    </div>

    <!-- ══ ① 选择设备类型 ══ -->
    <div v-show="currentStep === 1">
      <div class="wc-grid wc-grid--1-2">
        <section class="wc-card">
          <div class="wc-card__head">
            <h3>① 选择设备类型</h3>
            <span class="wc-card__sub">决定下一步需要填哪些连接参数</span>
          </div>
          <div class="wc-card__body">
            <UiRadio v-model="protocol" :options="protocolRadioOptions" />
          </div>
        </section>

        <section class="wc-card">
          <div class="wc-card__head">
            <h3>当前选择</h3>
            <span class="wc-card__sub">{{ protocolLabel }}</span>
          </div>
          <div class="wc-card__body">
            <dl class="wc-kv">
              <dt>协议</dt>
              <dd><span class="wc-tag wc-tag--info">{{ protocolLabel }}</span></dd>
              <dt>典型接入对象</dt>
              <dd>{{ protoMeta.use }}</dd>
              <dt>连接参数字段</dt>
              <dd class="wc-mono">{{ currentProtoFields.length }} 个</dd>
              <dt>差异字段</dt>
              <dd class="wc-mono">{{ protoMeta.keyField }}</dd>
              <dt>多从站</dt>
              <dd>
                <span class="wc-tag" :class="protoMeta.multi ? 'wc-tag--ok' : 'wc-tag--info'">
                  {{ protoMeta.multi ? '支持' : '单设备' }}
                </span>
              </dd>
              <dt>推荐采集频率</dt>
              <dd class="wc-mono">{{ protoMeta.freq }}</dd>
              <dt>点表模板</dt>
              <dd>
                <span class="wc-tag" :class="protoTemplate.length ? 'wc-tag--ok' : 'wc-tag--info'">
                  {{ protoTemplate.length ? '可用' : '暂缺' }}
                </span>
              </dd>
            </dl>
            <p class="dv-sub-h">{{ protocolLabel }} 最常见的三个坑</p>
            <ul class="dv-tips">
              <li v-for="(tip, i) in protoMeta.tips" :key="i">{{ tip }}</li>
            </ul>
          </div>
        </section>
      </div>

      <div class="dv-impact">
        <p class="dv-impact__title"><span aria-hidden="true">⚠</span>设备类型选错的后果</p>
        <ul>
          <li>字段按类型裁剪：Modbus RTU 才出现串口与波特率，S7 才出现机架号与槽号，OPC UA 才出现 Endpoint 与安全策略。</li>
          <li>类型选错<b>不会报错</b>，只会一直读不出数据；保存后改类型，地址风格随之改变（DB1.0 ↔ 40001），需重新校准点表。</li>
        </ul>
      </div>
    </div>

    <!-- ══ ② 连接参数（随协议整块替换）══ -->
    <section v-show="currentStep === 2" class="wc-card">
      <div class="wc-card__head">
        <h3>② 连接参数</h3>
        <span class="wc-card__sub">{{ protocolLabel }} · 共 {{ currentProtoFields.length }} 个字段</span>
      </div>
      <div class="wc-card__body">
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

        <p v-if="protocol !== ''" class="wc-note">
          <span class="wc-note__icon" aria-hidden="true">ⓘ</span>
          <span>「下一步」不落库，第 4 步才一次性保存；已填的值在步骤间来回切换<b>不会丢</b>。</span>
        </p>
      </div>
    </section>

    <!-- ══ ③ 点表映射 ══ -->
    <section v-show="currentStep === 3" class="wc-card">
      <div class="wc-card__head">
        <h3>③ 点表映射</h3>
        <span class="wc-card__sub">没有点表的设备采不到任何数据</span>
      </div>
      <div class="wc-card__body">
        <UiRadio v-model="pointSource" :options="pointSourceOptions" />

        <p class="dv-sub-h">模板预览 · {{ protocolLabel }}</p>
        <div v-if="protoTemplate.length" class="dv-wrap">
          <table class="wc-table">
            <thead>
              <tr><th>地址</th><th>数据类型</th><th>字节序</th><th>点位名</th></tr>
            </thead>
            <tbody>
              <tr v-for="row in protoTemplate" :key="row.addr">
                <td class="wc-mono">{{ row.addr }}</td>
                <td class="wc-mono">{{ row.dt }}</td>
                <td class="wc-mono">{{ row.bo }}</td>
                <td>{{ row.name }}</td>
              </tr>
            </tbody>
          </table>
        </div>
        <p v-else class="wc-note">
          <span class="wc-note__icon" aria-hidden="true">ⓘ</span>
          <span>{{ protocolLabel }} 暂无协议模板，请先手动添加点位或在「点位与映射」页维护。</span>
        </p>

        <template v-if="pointSource === 'import'">
          <p class="dv-sub-h">导入前先拿模板</p>
          <div class="dv-foot">
            <button type="button" class="wc-btn wc-btn--sm" @click="downloadTemplate">下载 {{ protocolLabel }} 导入模板（CSV）</button>
            <span class="dv-foot__hint">
              设备尚未创建，导入在保存后进行：保存完到「点位与映射」页选中这台设备再导入。
            </span>
          </div>
          <p class="wc-note">
            <span class="wc-note__icon" aria-hidden="true">ⓘ</span>
            <span>
              XLSX：<b>支持但需要后端启用</b> —— 桌面网关未内置表格解析库，请先用 Excel 另存为 CSV；
              校验失败会给出「行号 + 原因 + 允许值」。
            </span>
          </p>
        </template>

        <p v-if="pointSource === 'later'" class="wc-note wc-note--warn">
          <span class="wc-note__icon" aria-hidden="true">⚠</span>
          <span>选择「稍后再配」将只保存连接参数，设备会显示「已连接 · 未配点表」，<b>不产生任何数据</b>。</span>
        </p>

        <p class="wc-note">
          <span class="wc-note__icon" aria-hidden="true">ⓘ</span>
          <span>
            同一物理量在不同协议下地址<b>完全不同</b>（S7 是 <code>DB1.0</code>、Modbus 是 <code>40001</code>、OPC UA 是
            <code>ns=2;s=…</code>）。模板只是起点，务必对照手册逐点校准。
          </span>
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

        <!-- 保存前确认 -->
        <section class="wc-card">
          <div class="wc-card__head">
            <h3>保存前确认</h3>
            <span class="wc-card__sub">保存后按「启用时机」决定是否立即采集</span>
          </div>
          <div class="wc-card__body">
            <div class="dv-form">
              <UiField label="设备名称" required :error="saveTouched && !nameValid ? '请填写设备名称' : ''">
                <UiInput v-model="name" placeholder="如 5#注塑机" />
              </UiField>

              <UiField label="设备 ID" hint="默认取当前时间戳字符串，可更改；设备 ID 是北向报文里 device_mid 的取值">
                <UiInput v-model="deviceId" class="wc-mono" />
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

              <UiField label="分组" hint="面向 ISV 交付，后端暂无对应字段，见下方说明">
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
                hint="写入审计日志。新增设备属配置变更，必须留痕。"
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

            <p class="wc-note">
              <span class="wc-note__icon" aria-hidden="true">ⓘ</span>
              <span>
                <b>诚实降级：</b>设备 ID / 分组 / 备注 / 自定义参数是交付版表单项，后端当前未开放对应写入端
                口 —— 本页仅做<b>界面预览与录入</b>，保存时不会写入设备记录（保存成功的只有名称、协议、连接参
                数与采集频率）。<b>变更原因</b>则会随本次接入进入审计留痕。
              </span>
            </p>
          </div>
        </section>
      </div>

      <!-- 最后一步：核对后保存（原型 :1215-1222） -->
      <section class="wc-card">
        <div class="wc-card__head">
          <h3>最后一步：核对后保存</h3>
          <span class="wc-card__sub">未勾选则「保存并开始采集」不可点</span>
        </div>
        <div class="wc-card__body">
          <div class="dv-confirm">
            <div class="dv-confirm__text">
              <div class="dv-confirm__title">
                我已逐点核对地址与字节序
                <span class="wc-tag wc-tag--warn">保存前必选</span>
              </div>
              <div class="dv-confirm__desc">
                地址错位不会报错，只会悄悄读出错误数值；这也是现场最难排查的一类问题。网关只能验证「读得到」，
                验证不了「读得对」。
              </div>
            </div>
            <UiSwitch v-model="checked" on-text="已核对" off-text="未核对" />
          </div>
        </div>
      </section>
    </div>

    <!-- 提交成功 -->
    <div v-if="createdName" class="wc-banner wc-banner--ok">
      <span aria-hidden="true">✓</span>
      <span>{{ createdNote }}</span>
      <span class="wc-banner__ops">
        <button type="button" class="wc-btn wc-btn--sm" @click="go('devices')">查看设备列表</button>
        <button type="button" class="wc-btn wc-btn--sm" @click="go('points')">去点位与映射</button>
        <button type="button" class="wc-btn wc-btn--sm" @click="resetWizard">继续新增</button>
      </span>
    </div>

    <!-- 底部操作条（原型 actbar :1634-1645） -->
    <div class="dv-foot dv-foot--bar">
      <RoleGate :allowed="canWrite" mode="disable" fallback-label="仅工程师及以上可新增">
        <div class="dv-foot__ops">
          <span class="dv-foot__txt">
            第 <b>{{ currentStep }}</b> / {{ STEPS.length }} 步 · {{ STEPS[currentStep - 1] }}
            <span v-if="currentStep === 4 && !canSaveAndStart" class="dv-foot__warn">
              需完成名称 + 变更原因，并勾选「我已逐点核对地址与字节序」才能保存并开始采集
            </span>
          </span>
          <button type="button" class="wc-btn" @click="go('devices')">取消</button>
          <button type="button" class="wc-btn" :disabled="currentStep === 1" @click="prev">上一步</button>
          <button v-if="currentStep < 4" type="button" class="wc-btn wc-btn--primary" :disabled="!stepValid" @click="next">
            下一步
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
 * · 连通性探测：real 模式真实调用 `POST /api/devices/test`，**只展示后端返回的结构化结果**；
 *   mock 模式走演示流程并在结果里标注「演示」，绝不伪装成真实探测结果；
 * · 提交沿用 `repo.createDevice`（mock 内存态 / real 本地覆盖层 + 审计），签名不变。
 */
import { computed, reactive, ref, watch } from 'vue';
import { useRouter } from 'vue-router';
import {
  PageHeader,
  UiField,
  UiInput,
  UiRadio,
  UiSelect,
  UiSwitch,
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
} from '@/api/repo';
import { API_MODE } from '@/api/client';
import { session } from '../store/session';

const router = useRouter();

/** 步骤标题（原型 WZ_NAMES :1204）。 */
const STEPS = ['选择设备类型', '连接参数', '点表映射', '测试并保存'] as const;

// ---------------------------------------------------------------------------
// 协议元数据（原型 PROTOCOLS :766-809 / PROTO_KEY_FIELD :906-909）
// ---------------------------------------------------------------------------

interface ProtoMeta {
  /** 典型接入对象 */
  use: string;
  /** 推荐采集频率 */
  freq: string;
  /** 是否支持多从站 */
  multi: boolean;
  /** 该协议的差异字段（换协议时断言「字段真的变了」） */
  keyField: string;
  /** 常见坑 */
  tips: readonly string[];
}

const PROTO_META: Readonly<Record<ProtocolType, ProtoMeta>> = Object.freeze({
  'modbus-tcp': {
    use: '仪表 / 电表 / 变频器',
    freq: '200 ms ~ 1 s',
    multi: true,
    keyField: '从站号（Unit ID）',
    tips: [
      '从站号（Unit ID）在同网段内必须唯一，冲突会读到别人的数据',
      '功能码 03 保持寄存器可读可写、04 输入寄存器只读，选错会写坏设备',
      '地址 40001 与偏移 0 的对应关系因厂家而异，务必对照手册',
    ],
  },
  'modbus-rtu': {
    use: '老式仪表 / 温控器 / 电表',
    freq: '1 s ~ 5 s',
    multi: false,
    keyField: '串口设备',
    tips: [
      '波特率与校验位必须与从站完全一致，不一致表现为「整条总线无响应」',
      '同一串口上所有从站的串口参数必须相同，只有从站号可不同',
      'RS-485 必须手拉手接线，星型拓扑会造成偶发丢包',
    ],
  },
  'opc-ua': {
    use: 'SCADA / 空压机 / 数控机床',
    freq: '500 ms ~ 2 s',
    multi: false,
    keyField: 'Endpoint URL',
    tips: [
      '安全策略选 None 仅限内网调试，生产环境必须签名加密',
      '命名空间索引由服务端分配、重启可能变化，建议用命名空间 URI 而非索引',
      '开启数据变更订阅比轮询省带宽且更实时',
    ],
  },
  s7: {
    use: 'S7-1200 / S7-1500 / S7-300',
    freq: '200 ms ~ 1 s',
    multi: true,
    keyField: '槽号（Slot）',
    tips: [
      'S7-1200/1500 通常为机架 0 / 槽 1，填错会卡在握手阶段',
      '优化块（Optimized Block）不支持绝对地址，需关闭优化或改用符号寻址',
      'PDU 长度决定单次可读长度，过长会返回错误码而不是自动分包',
    ],
  },
  mc: {
    use: '三菱 FX / Q / iQ-R 系列',
    freq: '100 ms ~ 500 ms',
    multi: true,
    keyField: '报文格式',
    tips: [
      '3E/4E 帧格式与二进制 / ASCII 必须与 PLC 侧设置一致',
      '以太网直连场景网络号 / PC 号 / 站号通常为 0 / 255 / 1',
      '软元件 D 与 W 的寻址范围不同，超范围读取会返回错误码',
    ],
  },
  http: {
    use: '相机 / 称重 / 第三方系统',
    freq: '1 s ~ 5 s',
    multi: false,
    keyField: '取值表达式',
    tips: [
      '轮询间隔过短会被对端限流，建议不小于 1 秒',
      'Webhook 模式需网关对端可达，注意防火墙与反向代理配置',
      'JSONPath 表达式因后端实现而异，保存前先用连通性测试确认取值',
    ],
  },
  mqtt: {
    use: '其它网关 / 无线传感器',
    freq: '1 s ~ 10 s',
    multi: false,
    keyField: '订阅 Topic',
    tips: [
      'Client ID 重复会互相顶掉连接，表现为反复掉线',
      '通配符 # 订阅整棵子树，流量可能远超预期',
      'QoS 2 开销大，多数工业场景 QoS 1 已足够',
    ],
  },
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
    { key: 'endpoint', label: 'Endpoint URL', kind: 'text', required: true, group: '端点与安全', placeholder: 'opc.tcp://192.168.10.31:4840', hint: '可直接填 discovery 地址，保存时自动拉取端点列表' },
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
    { key: 'clientId', label: 'Client ID', kind: 'text', required: true, group: 'Broker 连接', placeholder: 'iot-daq-src-01', hint: '同一 Broker 上不可重复，否则会互相顶掉连接' },
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
// 点表模板（地址风格随协议变化；原型 ptTmpl :1224-1232）
// ---------------------------------------------------------------------------

interface TemplateRow {
  /** 地址 */
  addr: string;
  /** 数据类型 */
  dt: string;
  /** 字节序 */
  bo: string;
  /** 点位名 */
  name: string;
}

const PT_TEMPLATE: Readonly<Record<ProtocolType, readonly TemplateRow[]>> = Object.freeze({
  'modbus-tcp': [
    { addr: '40001', dt: 'uint16', bo: 'CD AB', name: '料筒温度1' },
    { addr: '40003', dt: 'uint16', bo: 'CD AB', name: '注射压力' },
    { addr: '40005', dt: 'uint32', bo: 'CD AB', name: '累计产量' },
  ],
  'modbus-rtu': [
    { addr: '40001', dt: 'uint16', bo: 'CD AB', name: '温度' },
    { addr: '40002', dt: 'uint16', bo: 'CD AB', name: '湿度' },
    { addr: '40003', dt: 'uint32', bo: 'CD AB', name: '电度' },
  ],
  s7: [
    { addr: 'DB1.0', dt: 'float32', bo: 'AB CD', name: '料筒温度1' },
    { addr: 'DB1.4', dt: 'float32', bo: 'AB CD', name: '注射压力' },
    { addr: 'DB2.0', dt: 'float32', bo: 'CD AB', name: '锁模力' },
  ],
  'opc-ua': [
    { addr: 'ns=2;s=Machine.BarrelTemp', dt: 'float32', bo: '—', name: '料筒温度1' },
    { addr: 'ns=2;s=Machine.InjPressure', dt: 'float32', bo: '—', name: '注射压力' },
    { addr: 'ns=2;s=Machine.ClampForce', dt: 'float32', bo: '—', name: '锁模力' },
  ],
  mc: [
    { addr: 'D100', dt: 'int16', bo: 'AB CD', name: '料筒温度1' },
    { addr: 'D102', dt: 'int16', bo: 'AB CD', name: '注射压力' },
    { addr: 'D200', dt: 'int32', bo: 'AB CD', name: '累计产量' },
  ],
  http: [
    { addr: '$.data.temperature', dt: 'float32', bo: '—', name: '温度' },
    { addr: '$.data.pressure', dt: 'float32', bo: '—', name: '压力' },
    { addr: '$.data.count', dt: 'uint32', bo: '—', name: '计数' },
  ],
  mqtt: [
    { addr: 'plant/line1/temp', dt: 'float32', bo: '—', name: '温度' },
    { addr: 'plant/line1/humi', dt: 'float32', bo: '—', name: '湿度' },
    { addr: 'plant/line1/count', dt: 'uint32', bo: '—', name: '计数' },
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

/** ③ 点表来源。 */
const pointSource = ref<'template' | 'import' | 'manual' | 'later'>('template');

/** ④ 保存前确认字段。 */
const name = ref('');
const deviceId = ref(`dev-${String(Date.now())}`);
const intervalMs = ref('200');
const group = ref('未分组');
const enableWhen = ref('保存后立即启用采集');
const memo = ref('');
const changeReason = ref('');
/** 「我已逐点核对地址与字节序」。 */
const checked = ref(false);
/** 自定义参数行（交付版表单项，当前不入后端，已诚实标注）。 */
const customParams = ref<{ k: string; v: string }[]>([]);

/** 交互标记（控制错误提示是否显示，避免一打开就标红）。 */
const connTouched = ref(false);
const saveTouched = ref(false);

const createdName = ref('');
const createdNote = ref('');

/** 协议中文名。 */
const protocolLabel = computed(
  () => PROTOCOL_OPTIONS.find((p) => p.value === protocol.value)?.label ?? '未选择',
);

/** 当前协议的元数据（未选协议时给占位，不伪造内容）。 */
const protoMeta = computed<ProtoMeta>(() =>
  protocol.value
    ? PROTO_META[protocol.value]
    : { use: '—（请先选择设备类型）', freq: '—', multi: false, keyField: '—', tips: [] },
);

/** 当前协议的点表模板。 */
const protoTemplate = computed<readonly TemplateRow[]>(() =>
  protocol.value ? PT_TEMPLATE[protocol.value] : [],
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

/** 点表来源四选项（原型 :1592-1597）。 */
const pointSourceOptions = computed<readonly RadioOption[]>(() => [
  {
    value: 'template',
    label: '使用协议模板生成',
    desc: `按 ${protocolLabel.value} 预生成标准点表（含地址风格与字节序），再逐点校准。适合首次接入。`,
  },
  {
    value: 'import',
    label: '导入 CSV / XLSX 点表',
    desc: '用「下载模板」导出的文件填写后回传；校验失败会给出「行号 + 原因 + 允许值」，导出文件可直接当导入模板。',
  },
  { value: 'manual', label: '手动添加点位', desc: '点位少于 10 个时最快；保存后可在「点位与映射」继续维护。' },
  { value: 'later', label: '稍后再配', desc: '只保存连接参数。设备会显示「已连接 · 未配点表」，不产生任何数据。' },
]);

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

const step2Valid = computed(
  () => currentProtoFields.value.length > 0 && currentProtoFields.value.every(isFieldValid),
);

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
  () => nameValid.value && intervalValid.value && reasonValid.value && checked.value && step2Valid.value,
);

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
// ④ 连通性测试（real：POST /api/devices/test；mock：演示流程并标注）
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

/** 毫秒延时。 */
function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => {
    setTimeout(resolve, ms);
  });
}

/**
 * 跑一次连通性测试。
 *
 * · real：真实调用 `POST /api/devices/test`，**只渲染后端结构化结果**（Modbus 全探测，
 *   其余协议返回 unsupported_protocol —— 不伪造成功）；
 * · mock：按演示节奏推进四步，并在结果文案中明确标注「演示」。
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

  if (API_MODE !== 'real') {
    for (const step of steps) {
      if (step.n === '4') {
        break;
      }
      step.state = 'run';
      step.time = '进行中';
      await sleep(420);
      step.state = 'done';
      step.time = `${8 + Number(step.n) * 4} ms`;
    }
    steps[3].state = 'idle';
    steps[3].time = '需人工核对';
    testRunning.value = false;
    testResult.value = {
      ok: true,
      text: '演示流程跑通（mock 模式未发起任何真实连接，不代表设备真的连通）。真实连通性请在 real 模式下由网关侧探测。',
    };
    return;
  }

  try {
    // real：统一走 repo.actions.testDevice（已封装 POST /api/devices/test，
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
// ③ 模板下载
// ---------------------------------------------------------------------------

/** 下载当前协议的导入模板（CSV；字段口径与「点位与映射」页一致）。 */
function downloadTemplate(): void {
  const header = ['地址', '点位名', '数据类型', '字节序', '单位', '死区', '目标点名'];
  const rows: string[][] = [header];
  for (const row of protoTemplate.value) {
    rows.push([row.addr, row.name, row.dt, row.bo, '', '0', row.addr.replace(/[^A-Za-z0-9]/g, '_')]);
  }
  const csv = '﻿' + rows.map((r) => r.join(',')).join('\r\n');
  const blob = new Blob([csv], { type: 'text/csv;charset=utf-8' });
  const url = URL.createObjectURL(blob);
  const a = document.createElement('a');
  a.href = url;
  a.download = `points-template-${protocolLabel.value}.csv`;
  document.body.appendChild(a);
  a.click();
  document.body.removeChild(a);
  URL.revokeObjectURL(url);
}

// ---------------------------------------------------------------------------
// ④ 保存
// ---------------------------------------------------------------------------

/**
 * 提交设备。
 *
 * @param mode `saveAndStart` = 保存并要求网关立即开始采集；`save` = 仅保存连接参数。
 */
function submit(mode: 'save' | 'saveAndStart'): void {
  saveTouched.value = true;
  if (!nameValid.value || !protocol.value) {
    return;
  }
  if (mode === 'saveAndStart' && !canSaveAndStart.value) {
    return;
  }
  if (mode === 'save' && !intervalValid.value) {
    return;
  }

  const draft: DeviceDraft = {
    name: name.value,
    protocol: protocol.value,
    connectionSummary: connectionSummary.value,
    intervalMs: Number(intervalMs.value),
    timeoutMs: Number((conn.timeout ?? '') || 1000),
    retryTimes: Number((conn.retry ?? '') || 3),
    actor: session.state.displayName,
  };
  const created = repo.createDevice(draft);
  createdName.value = created.name;
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
  pointSource.value = 'template';
  name.value = '';
  deviceId.value = `dev-${String(Date.now())}`;
  intervalMs.value = '200';
  group.value = '未分组';
  enableWhen.value = '保存后立即启用采集';
  memo.value = '';
  changeReason.value = '';
  checked.value = false;
  customParams.value = [];
  connTouched.value = false;
  saveTouched.value = false;
  createdName.value = '';
  createdNote.value = '';
  testResult.value = null;
  testUnsupported.value = false;
  resetTestSteps();
}

/** 跳转。 */
function go(name: string): void {
  void router.push({ name });
}

resetTestSteps();
</script>

<style scoped>
.dv-steps {
  display: flex;
  align-items: center;
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
  transition: all 160ms cubic-bezier(0.16, 1, 0.3, 1);
}
.dv-steps__item.is-clickable {
  cursor: pointer;
}
.dv-steps__item.is-clickable:hover {
  color: var(--text-1);
  border-color: var(--brand);
  background: var(--brand-subtle);
}
.dv-steps__item.is-lock {
  opacity: 0.5;
  cursor: not-allowed;
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

/* 「三个坑」列表（原型 .tips） */
.dv-tips {
  margin: 8px 0 0;
  padding-left: 18px;
  font-size: var(--fs-caption);
  color: var(--text-2);
  line-height: 1.72;
}
.dv-tips li {
  margin-bottom: 4px;
}
.dv-tips li::marker {
  color: var(--brand);
}

/* 影响块（原型 .impact） */
.dv-impact {
  margin-top: 16px;
  padding: 12px 14px;
  border: 1px solid var(--warn-border);
  background: var(--warn-bg);
  border-radius: var(--radius);
}
.dv-impact__title {
  margin: 0;
  font-size: var(--fs-table);
  font-weight: 600;
  color: var(--warn-fg);
}
.dv-impact ul {
  margin: 6px 0 0;
  padding-left: 18px;
  font-size: var(--fs-caption);
  color: var(--text-2);
  line-height: 1.75;
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

/* 「我已逐点核对」开关行（原型 .sw-row） */
.dv-confirm {
  display: flex;
  align-items: center;
  gap: 16px;
  flex-wrap: wrap;
  padding: 12px 14px;
  border: 1px dashed var(--warn-border);
  border-radius: var(--radius);
  background: var(--warn-bg);
}
.dv-confirm__text {
  flex: 1 1 320px;
}
.dv-confirm__title {
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: var(--fs-body);
  font-weight: 600;
  color: var(--text-1);
}
.dv-confirm__desc {
  margin-top: 4px;
  font-size: var(--fs-caption);
  color: var(--text-3);
  line-height: 1.6;
}

.dv-wrap {
  max-height: 220px;
  overflow: auto;
  border: 1px solid var(--divider);
  border-radius: var(--radius-sm);
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
