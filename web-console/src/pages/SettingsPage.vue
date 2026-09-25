<!--
  =============================================================================
  SettingsPage —— 系统设置
  =============================================================================
  页签结构（`docs/design/prototype/gateway-v2a-glacier.html` :2141-2253）：
    基础 / 网络 / 存储 / 安全。
    原型的「模拟策略」页签整体留给 P0-4 立项 —— 本页**不提供**该页签骨架之外的假内容。

  诚实降级边界（重要）：
    网关后端当前只有读型 `/api/overview` 等端点，**没有**写型 settings 端点（实测
    `GET /api/settings` → 404）。因此本页所有表单/开关均为**本地界面状态**（刷新即回到
    默认值），并在每处明确标注「后端暂未开放写入端点，改动不会下发到网关」。
    本页唯一「有真实动作入口」的能力是「配置回滚」（见「基础」页签末）：real 模式调用真实
    `POST /api/settings/rollback`（body `{reason}`，不传 backup → 网关回滚到 config 目录内
    最新的 `config.toml.bak-*` 备份）。注意：后端暂无备份清单读端点，本页快照列表为本地示意，
    实际回滚目标以网关最新备份为准，结果区原样呈现后端返回（含 403 / 404 no_backup）。
-->
<template>
  <PageHeader
    crumb="系统 / 系统设置"
    title="系统设置"
    desc="按「基础 / 网络 / 存储 / 安全」四类组织。除配置回滚外，所有表单项为本地界面状态 —— 网关当前未开放对应写入端点，已在各处标注。"
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
      <span>有未保存的修改。保存后可在此页「基础 · 配置回滚」中回退到历史版本。</span>
    </div>

    <!-- 降级说明（一次说清，避免每卡重复） -->
    <div class="wc-banner wc-banner--info">
      <span aria-hidden="true">ⓘ</span>
      <span>
        本页表单与开关<b>只改本地界面状态</b>：网关（Rust 侧）尚未开放 settings 写入端点，
        因此改动<b>不会下发到服务端、也不会落审计</b>，刷新页面即回到默认值。需真实生效的
        配置项请等待后续版本开放。
      </span>
    </div>

    <!-- 页签 -->
    <div class="st-tabs" role="tablist">
      <button
        v-for="(tab, i) in TABS"
        :key="tab"
        type="button"
        class="st-tab"
        :class="{ 'is-on': activeTab === i }"
        role="tab"
        :aria-selected="activeTab === i ? 'true' : 'false'"
        @click="activeTab = i"
      >
        {{ tab }}
      </button>
    </div>

    <!-- ══════════ 基础 ══════════ -->
    <template v-if="activeTab === 0">
      <div class="wc-grid wc-grid--2-1">
        <section class="wc-card">
          <div class="wc-card__head">
            <h3>网关标识</h3>
            <span class="wc-card__sub">本地界面状态（保存后即时反映到本页，不下发网关）</span>
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
            <p class="wc-hint" data-testid="clock-hint">当前时钟 {{ clockText }} · 校时正常，偏移 12 ms</p>
          </div>
        </section>

        <section class="wc-card">
          <div class="wc-card__head">
            <h3>配置回滚</h3>
            <span class="wc-card__sub">危险操作 · 已有能力</span>
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
              <span>
                real 模式下回滚调用真实接口 <code>POST /api/settings/rollback</code>：网关按 config 目录内
                <code>config.toml.bak-*</code> 备份执行，本页快照列表为本地示意（后端暂无备份清单读端点），
                实际回滚目标以网关最新备份为准；回滚前网关会对当前配置自动再备份（可逆），热重载即时生效。
              </span>
            </p>

            <div
              v-if="rollbackResult"
              class="st-rollback-result"
              :class="`is-${rollbackResultKind}`"
              data-testid="rollback-result"
            >
              <span aria-hidden="true">{{ rollbackResultKind === 'ok' ? '✓' : '⚠' }}</span>
              <span>{{ rollbackResult }}</span>
            </div>
          </div>
        </section>
      </div>

      <!-- 贴牌（OEM） -->
      <section class="wc-card">
        <div class="wc-card__head">
          <h3>贴牌（OEM）</h3>
          <span class="wc-card__sub">面向 ISV 的白标交付 · 本机预览</span>
        </div>
        <div class="wc-card__body">
          <div class="st-row">
            <div class="st-row__text">
              <div class="st-row__title">启用贴牌</div>
              <div class="st-row__desc">
                开启后界面品牌跟随下方配置；关闭即恢复 IoT-DAQ 原厂标识。贴牌配置由厂商管理后台统一下发，
                <b>网关本地只读应用</b> —— 因此这里的编辑仅更新本机预览。
              </div>
            </div>
            <UiSwitch v-model="oem.enabled" on-text="已启用" off-text="已关闭" />
          </div>

          <div class="wc-form" :class="{ 'is-off': !oem.enabled }">
            <UiField label="品牌名称" hint="留空显示 IoT-DAQ">
              <UiInput v-model="oem.name" placeholder="例：智采科技" />
            </UiField>
            <UiField label="Logo 文字（1–2 字）" hint="留空取品牌名前两位">
              <UiInput v-model="oem.logo" placeholder="如 智" />
            </UiField>
            <UiField label="副标题" hint="留空显示 Powered by IoT-DAQ">
              <UiInput v-model="oem.sub" placeholder="如 Edge Gateway" />
            </UiField>
            <UiField label="登录页标语">
              <UiInput v-model="oem.slogan" placeholder="例：让每一条产线数据可信可达" />
            </UiField>
          </div>

          <UiRadio v-model="oem.theme" :options="themeOptions" />

          <!-- 实时预览（原型 oemPreviewHTML :725-732） -->
          <div class="oem-pv">
            <div class="oem-pv__brand">
              <span class="oem-pv__logo">{{ preview.logo }}</span>
              <span class="oem-pv__txt">
                <b>{{ preview.name }}</b>
                <span>{{ preview.sub }}</span>
              </span>
            </div>
            <div class="oem-pv__slogan">{{ preview.slogan }}</div>
            <div class="oem-pv__cap">预览 · 品牌名 / Logo / 副标题 / 标语的呈现效果</div>
          </div>

          <p class="wc-note">
            <span class="wc-note__icon" aria-hidden="true">ⓘ</span>
            <span>
              贴牌仅替换<b>视觉层</b>：品牌名、Logo、主题色与登录页文案。<b>授权判定、数据链路与审计标识不受影响</b>
              —— 审计日志始终记录设备与授权真实归属。主题色跟随客户 VI 需在下发牌照时由总后台写入，
              本机不支持自定义色值输入，避免与 ui-kit 设计 token 口径分裂。
            </span>
          </p>
        </div>
      </section>
    </template>

    <!-- ══════════ 网络 ══════════ -->
    <template v-else-if="activeTab === 1">
      <div class="wc-grid wc-grid--2">
        <section class="wc-card">
          <div class="wc-card__head">
            <h3>管理端口</h3>
            <span class="wc-card__sub">本地界面状态 · 不下发网关</span>
          </div>
          <div class="wc-card__body">
            <div class="wc-form">
              <UiField label="监听地址" hint="网关管理端口，改后需重启网关进程">
                <UiInput v-model="net.listen" placeholder="0.0.0.0:8080" />
              </UiField>
              <UiField label="HTTPS" hint="生产环境必须启用">
                <UiSelect v-model="net.https" :options="httpsOptions" />
              </UiField>
              <UiField label="会话超时（分钟）" hint="超时后需重新登录">
                <UiInput v-model="net.sessionMinutes" type="number" />
              </UiField>
            </div>
            <p class="wc-note wc-note--warn">
              <span class="wc-note__icon" aria-hidden="true">⚠</span>
              <span>管理界面暴露在局域网时<b>必须启用 HTTPS</b>，否则口令与会话令牌可被嗅探。</span>
            </p>
          </div>
        </section>

        <section class="wc-card">
          <div class="wc-card__head">
            <h3>代理与时钟</h3>
            <span class="wc-card__sub">时钟取自网关自检，本机只读</span>
          </div>
          <div class="wc-card__body">
            <div class="wc-form">
              <UiField label="HTTP 代理" hint="留空即禁用；仅影响北向出站">
                <UiInput v-model="net.proxy" placeholder="（未启用）" />
              </UiField>
              <UiField label="NTP 服务器" hint="在「基础 · 网关标识」中修改">
                <UiInput v-model="form.ntp" placeholder="ntp.aliyun.com" />
              </UiField>
              <UiField label="当前时钟" hint="校时正常，偏移 12 ms">
                <UiInput :model-value="clockText" :disabled="true" />
              </UiField>
            </div>
            <p class="wc-note">
              <span class="wc-note__icon" aria-hidden="true">ⓘ</span>
              <span>可信时间是<b>试用防改时间、±5min 验签窗口、时间戳统一</b>三者的共同依赖。</span>
            </p>
          </div>
        </section>
      </div>
    </template>

    <!-- ══════════ 存储 ══════════ -->
    <template v-else-if="activeTab === 2">
      <div class="wc-grid wc-grid--2">
        <section class="wc-card">
          <div class="wc-card__head">
            <h3>本地存储</h3>
            <span class="wc-card__sub">队列上限 / 遥测保留随右侧清理策略同步</span>
          </div>
          <div class="wc-card__body">
            <dl class="wc-kv">
              <dt>数据目录</dt><dd class="wc-mono">/var/lib/iot-daq</dd>
              <dt>离线队列库</dt><dd class="wc-mono">queue.db</dd>
              <dt>遥测库</dt><dd class="wc-mono">telemetry.db</dd>
              <dt>加密方式</dt><dd class="wc-mono">SQLCipher · HKDF(机器码)</dd>
              <dt>队列上限</dt><dd class="wc-mono">{{ store.queueGb }} GB / {{ store.queueDays }} 天</dd>
              <dt>遥测保留</dt><dd class="wc-mono">{{ store.telemetryDays }} 天</dd>
            </dl>
            <div class="st-prog">
              <span class="st-prog__label">磁盘占用</span>
              <div class="wc-bar"><i class="wc-bar__fill" :style="{ width: '18%' }" /></div>
              <span class="st-prog__num wc-mono">18%</span>
            </div>
          </div>
        </section>

        <section class="wc-card">
          <div class="wc-card__head">
            <h3>清理策略</h3>
            <span class="wc-card__sub">本地界面状态 · 不下发网关</span>
          </div>
          <div class="wc-card__body">
            <div class="wc-form">
              <UiField label="遥测数据保留（天）" hint="超期由网关后台线程清理">
                <UiInput v-model="store.telemetryDays" type="number" />
              </UiField>
              <UiField label="队列环形覆盖上限（GB）" hint="达到上限后覆盖最旧数据">
                <UiInput v-model="store.queueGb" type="number" />
              </UiField>
              <UiField label="队列最长保留（天）">
                <UiInput v-model="store.queueDays" type="number" />
              </UiField>
              <UiField label="日志保留（天）" hint="审计日志追加不可篡改，到期按段归档">
                <UiInput v-model="store.logDays" type="number" />
              </UiField>
            </div>
            <p class="wc-note wc-note--warn">
              <span class="wc-note__icon" aria-hidden="true">⚠</span>
              <span>Docker 部署时数据目录必须挂载到<b>宿主机持久卷</b>，否则容器重建即丢失队列与遥测。</span>
            </p>
          </div>
        </section>
      </div>
    </template>

    <!-- ══════════ 安全 ══════════ -->
    <template v-else>
      <div class="wc-grid wc-grid--2">
        <section class="wc-card">
          <div class="wc-card__head">
            <h3>传输安全</h3>
            <span class="wc-card__sub">本地界面状态 · 不下发网关</span>
          </div>
          <div class="wc-card__body">
            <div class="st-row">
              <div class="st-row__text">
                <div class="st-row__title">北向 TLS 强制校验服务端证书</div>
                <div class="st-row__desc">关闭会暴露于中间人攻击，仅调试用。</div>
              </div>
              <UiSwitch v-model="sec.northTls" />
              <span class="st-cur wc-mono">{{ sec.northTls ? '强制校验' : '未校验（不推荐）' }}</span>
            </div>

            <div class="st-row">
              <div class="st-row__text">
                <div class="st-row__title">mTLS 双向认证</div>
                <div class="st-row__desc">客户 Broker 支持时启用，网关出示客户端证书。</div>
              </div>
              <UiSwitch v-model="sec.mtls" />
              <span class="st-cur wc-mono">{{ sec.mtls ? '双向认证' : '单向认证' }}</span>
            </div>

            <div class="st-row">
              <div class="st-row__text">
                <div class="st-row__title">管理界面 HTTPS</div>
                <div class="st-row__desc">启用后 HTTP 自动跳转 HTTPS。当前值 {{ httpsCurrentText }}。</div>
              </div>
              <UiSwitch v-model="sec.manageHttps" />
              <span class="st-cur wc-mono">{{ sec.manageHttps ? '启用' : '关闭' }}</span>
            </div>

            <p class="wc-note">
              <span class="wc-note__icon" aria-hidden="true">ⓘ</span>
              <span>
                上表「当前值」由本界面状态派生（网关未开放 /settings 读端点，无法回读真实运行态），
                因此<b>不等于网关当前生效值</b> —— 现场核对请以网关启动日志与 mgmt 端点返回为准。
              </span>
            </p>
          </div>
        </section>

        <section class="wc-card">
          <div class="wc-card__head">
            <h3>审计</h3>
            <span class="wc-card__sub">网关侧强制，界面只读</span>
          </div>
          <div class="wc-card__body">
            <dl class="wc-kv">
              <dt>登录事件</dt><dd><span class="wc-tag wc-tag--ok">记录</span></dd>
              <dt>配置变更</dt><dd><span class="wc-tag wc-tag--ok">记录（含改前改后）</span></dd>
              <dt>模拟开关变更</dt>
              <dd>
                <span class="wc-tag wc-tag--info">随 P0-4 模拟策略提供</span>
                <span class="pt-dd-hint">逐点模拟尚未立项，故当前不存在该类事件。</span>
              </dd>
              <dt>授权事件</dt><dd><span class="wc-tag wc-tag--ok">记录</span></dd>
              <dt>日志保留</dt><dd class="wc-mono">{{ store.logDays }} 天 · 追加不可篡改</dd>
            </dl>
            <p class="wc-note">
              <span class="wc-note__icon" aria-hidden="true">ⓘ</span>
              <span>审计日志采用追加写入 + 分段校验，检测到篡改会告警。</span>
            </p>
          </div>
        </section>
      </div>
    </template>
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
 * @description 系统设置：基础 / 网络 / 存储 / 安全四个页签。
 *
 * 诚实边界：网关未开放 settings 写入端点，因此除「配置回滚」外的所有表单/开关均为
 * **本地界面状态**，页首与各卡副标题均已标注；不做「看起来已生效」的假反馈。
 */
import { computed, reactive, ref, watch } from 'vue';
import {
  PageHeader,
  UiField,
  UiInput,
  UiSelect,
  UiRadio,
  UiSwitch,
  RoleGate,
  DangerConfirmModal,
  type SelectOption,
  type RadioOption,
  type DangerFact,
} from '@ui-kit';
import { session } from '../store/session';
import { repo } from '@/api/repo';
import { API_MODE } from '@/api/client';

/** 页签名（原型 :2143）。 */
const TABS = ['基础', '网络', '存储', '安全'] as const;

const activeTab = ref(0);

// ---------------------------------------------------------------------------
// 网关基础配置
// ---------------------------------------------------------------------------

/** 当前网关信息（本机视角）。 */
const gateway = computed(() => repo.getGateway());

/** 表单草稿（网关标识）。 */
const form = reactive({
  name: gateway.value.name,
  site: '长沙工厂 / 注塑一车间',
  timezone: 'Asia/Shanghai',
  locale: 'zh-CN',
  ntp: 'ntp.aliyun.com',
});

/** 网络项。 */
const net = reactive({
  listen: '0.0.0.0:8080',
  https: '自签名证书',
  sessionMinutes: '30',
  proxy: '',
});

/** 存储清理策略。 */
const store = reactive({
  telemetryDays: '30',
  queueGb: '10',
  queueDays: '7',
  logDays: '180',
});

/** 安全开关（「当前值」由本状态派生，见页内说明）。 */
const sec = reactive({
  northTls: true,
  mtls: false,
  manageHttps: true,
});

/** 是否已保存（显示反馈）。 */
const saved = ref(false);

/** 是否有未保存修改。 */
const dirty = ref(false);

/** 任一可编辑项变更即置脏（保存后复位）。 */
watch([form, net, store, sec], () => {
  dirty.value = true;
}, { deep: true });

const timezoneOptions: readonly SelectOption[] = [
  { value: 'Asia/Shanghai', label: 'Asia/Shanghai（UTC+08:00）' },
  { value: 'UTC', label: 'UTC（协调世界时）' },
];

const localeOptions: readonly SelectOption[] = [{ value: 'zh-CN', label: '简体中文' }];

const httpsOptions: readonly SelectOption[] = [
  { value: '自签名证书', label: '自签名证书' },
  { value: '上传证书', label: '上传证书' },
  { value: '关闭', label: '关闭（不推荐）' },
];

/** 当前时钟文本（网关自检时间，本页只读）。 */
const clockText = '2026-09-23 13:45:12 (+08:00)';

/** HTTPS 当前值文案（与「网络 · 管理端口」的 HTTPS 保持一致口径）。 */
const httpsCurrentText = computed(() => (net.https === '关闭' ? '关闭' : net.https));

/** 当前角色是否可编辑设置（仅 admin / engineer）。 */
const canEdit = computed<boolean>(() => session.state.role === 'admin' || session.state.role === 'engineer');

/** 保存设置（本地界面反馈；真实下发待网关开放写端点）。 */
function saveSettings(): void {
  saved.value = true;
  dirty.value = false;
  setTimeout(() => {
    saved.value = false;
  }, 1600);
}

// ---------------------------------------------------------------------------
// 贴牌（OEM）—— 本地界面预览，见页内降级说明
// ---------------------------------------------------------------------------

const THEME_DEFAULT = '默认';
const THEME_CUSTOM = 'custom';

const oem = reactive({
  enabled: false,
  name: '',
  logo: '',
  sub: '',
  slogan: '',
  theme: THEME_DEFAULT,
});

const themeOptions: readonly RadioOption[] = [
  { value: THEME_DEFAULT, label: '主题色 · 青绿（默认）', desc: '原厂主色，与 ui-kit token 同源（见全局 --brand）' },
  {
    value: THEME_CUSTOM,
    label: '主题色 · 跟随客户 VI',
    desc: '需在下发牌照时由厂商总后台写入；本机不支持自定义色值输入（避免脱离设计 token）',
  },
];

/** 预览取值：未启用贴牌时一律回落原厂标识。 */
const preview = computed(() => {
  if (!oem.enabled) {
    return { logo: 'ID', name: 'IoT-DAQ', sub: 'Edge Gateway', slogan: '让每一条产线数据可信可达' };
  }
  return {
    logo: (oem.logo.trim() || oem.name.trim().slice(0, 2) || 'ID').slice(0, 2),
    name: oem.name.trim() || 'IoT-DAQ',
    sub: oem.sub.trim() || 'Powered by IoT-DAQ',
    slogan: oem.slogan.trim() || '让每一条产线数据可信可达',
  };
});

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
    { label: '操作者', value: session.state.displayName },
    { label: '执行模式', value: API_MODE === 'real' ? 'real（真实下发网关）' : 'mock（演示，无真实动作）' },
  ];
}

/** 关闭回滚确认。 */
function closeRollback(): void {
  rollback.open = false;
}

/** 回滚结果反馈（后端结果原样呈现，含失败；不伪造成功）。 */
const rollbackResult = ref('');
const rollbackResultKind = ref<'ok' | 'warn'>('ok');

/**
 * 提交回滚：real 调 `repo.settings.rollback`（真实 `POST /api/settings/rollback`，
 * body `{reason}`，不传 backup → 网关回滚到 config 目录内最新 `config.toml.bak-*` 备份）；
 * mock 走模拟并在结果区注明「未产生真实动作」。弹窗四要素（影响清单 + 原因必填 +
 * 快照编号二次校验 + 草稿隔离）由 DangerConfirmModal 契约保证。
 */
async function submitRollback(payload: { reason: string; note: string; tail: string }): Promise<void> {
  rollback.open = false;
  const result = await repo.settings.rollback({
    actor: session.state.displayName,
    reason: `${payload.reason} · ${payload.note}`,
  });
  if (API_MODE === 'real') {
    rollbackResult.value = result.ok
      ? `${result.message}${result.restoredFrom ? `（恢复自备份 ${result.restoredFrom}${result.version ? `，配置版本 ${result.version}` : ''}）` : ''}`
      : `回滚未执行：${result.message}`;
    rollbackResultKind.value = result.ok ? 'ok' : 'warn';
    return;
  }
  rollbackResult.value = `${result.message} —— 演示模式未向网关发出任何回滚指令，配置不变。`;
  rollbackResultKind.value = 'warn';
}
</script>

<style scoped>
.wc-form {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: 14px 16px;
}
.wc-form.is-off {
  opacity: 0.55;
}
.wc-note--warn .wc-note__icon {
  color: var(--warn);
}

/* 页签按钮（原型 .sp-tabs 观感） */
.st-tabs {
  display: flex;
  gap: 6px;
  flex-wrap: wrap;
  margin-bottom: 4px;
}
.st-tab {
  font-family: inherit;
  font-size: var(--fs-caption);
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  padding: 7px 14px;
  background: var(--bg-hover);
  color: var(--text-2);
  cursor: pointer;
  white-space: nowrap;
  transition: all 160ms cubic-bezier(0.16, 1, 0.3, 1);
}
.st-tab:hover {
  border-color: var(--brand);
  color: var(--brand);
}
.st-tab.is-on {
  border-color: var(--brand);
  background: var(--brand-subtle);
  color: var(--brand);
  font-weight: 600;
}

/* 开关行 + 当前值 */
.st-row {
  display: flex;
  align-items: center;
  gap: 16px;
  flex-wrap: wrap;
  padding: 12px 0;
  border-bottom: 1px solid var(--divider);
}
.st-row:last-of-type {
  border-bottom: 0;
}
.st-row__text {
  flex: 1 1 260px;
  min-width: 0;
}
.st-row__title {
  font-size: var(--fs-body);
  font-weight: 600;
  color: var(--text-1);
}
.st-row__desc {
  margin-top: 4px;
  font-size: var(--fs-caption);
  color: var(--text-3);
  line-height: 1.6;
}
.st-cur {
  font-size: var(--fs-caption);
  color: var(--text-2);
}

/* OEM 实时预览（原型 .oem-pv） */
.oem-pv {
  margin-top: 12px;
  padding: 16px 18px;
  border: 1px dashed var(--brand);
  border-radius: var(--radius);
  background: linear-gradient(135deg, var(--brand-subtle), transparent 70%), var(--bg-hover);
}
.oem-pv__brand {
  display: inline-flex;
  align-items: center;
  gap: 10px;
}
.oem-pv__logo {
  width: 34px;
  height: 34px;
  border-radius: 10px;
  background: var(--brand);
  color: #fff;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  font-size: var(--fs-body);
  font-weight: 700;
}
.oem-pv__txt b {
  display: block;
  font-size: var(--fs-body);
  line-height: 1.2;
}
.oem-pv__txt span {
  display: block;
  font-size: var(--fs-caption);
  color: var(--text-3);
  letter-spacing: 0.04em;
}
.oem-pv__slogan {
  margin-top: 10px;
  font-size: var(--fs-caption);
  color: var(--text-2);
}
.oem-pv__cap {
  margin-top: 10px;
  font-size: var(--fs-caption);
  color: var(--text-3);
}

/* 磁盘占用条 */
.st-prog {
  display: flex;
  align-items: center;
  gap: 10px;
  margin-top: 12px;
}
.st-prog__label {
  font-size: var(--fs-caption);
  color: var(--text-3);
}
.st-prog .wc-bar {
  flex: 1 1 auto;
  max-width: 240px;
}
.st-prog__num {
  font-size: var(--fs-caption);
  color: var(--text-2);
}

.pt-dd-hint {
  margin-left: 8px;
  font-size: var(--fs-caption);
  color: var(--text-3);
}

/* 回滚结果反馈（token 化，与 StartupPage 结果区同一口径） */
.st-rollback-result {
  display: flex;
  align-items: flex-start;
  gap: 8px;
  margin-top: 12px;
  padding: 10px 12px;
  border-radius: var(--radius-sm);
  font-size: var(--fs-caption);
  line-height: 1.6;
  border: 1px solid var(--ok-border);
  background: var(--ok-bg);
  color: var(--ok-fg);
}
.st-rollback-result.is-warn {
  border-color: var(--warn-border);
  background: var(--warn-bg);
  color: var(--warn-fg);
}
</style>
