<template>
  <!--
    StartupPage —— 启动与自启（运维分组，路由 `/startup`）。

    结构（`docs/design/prototype/gateway-v2a-glacier.html` :1985-2045 + :3345-3358 + :3131-3143）：
      KPI 四卡 → 启动策略 / 启动方式识别 → 计划重启 / 危险操作。

    危险操作契约（本次修复重点）：
      · 「立即重启服务」走 ui-kit `DangerConfirmModal`：影响清单 + 原因必填（枚举 + 补充说明）+
        **服务名二次校验**；real 模式调真实 `POST /api/ops/restart`，body `{actor, confirm, reason}`，
        `confirm` 由页面自动回显网关标识（/api/overview 的 `name` = gateway_id）；
      · 「停止服务（不自动启动）」同样走该弹窗并追加**服务名二次校验**；
        后端当前未提供停止端点（实测 POST /api/ops/stop → 404），确认后诚实告知并给宿主机命令；
      · 确认后的动作：mock 模式执行既有演示行为（`repo.ops.restart`），结果区标注「未产生真实动作」；
        real 模式后端返回的结果（含 403 / 400 confirm_mismatch）原样呈现。

    其它硬性约定：
      · 部署形态「自动识别」：二者并列展示并高亮当前形态，绝不让用户手动二选一；
      · 系统级自启变更受 RoleGate 控制（仅管理员可改）；
      · 所有系统操作只给真实结果或明确标注的演示反馈，不伪造后端落地。
  -->
  <PageHeader
    crumb="运维 / 启动与自启"
    title="启动与自启"
    desc="服务生命周期：开机自启、崩溃重启、看门狗与计划重启。部署形态自动识别；重启与停止服务为高危操作，需填原因并写入审计。"
  />

  <div class="wc-content">
    <!-- KPI 四卡（原型 :1991-1996） -->
    <div class="wc-grid wc-grid--4">
      <StatCard label="服务状态" :value="serviceStatusText" :sub="serviceStatusSub" icon-tone="teal">
        <template #icon>▶</template>
      </StatCard>
      <StatCard label="已运行" :value="gateway.uptimeText" sub="来自网关自检" icon-tone="ink">
        <template #icon>◷</template>
      </StatCard>
      <StatCard label="上次启动" :value="gateway.startedAt" sub="随网关进程启动" icon-tone="amber">
        <template #icon>↻</template>
      </StatCard>
      <StatCard label="异常重启" value="—" unit="次" sub="后端未统计（不伪造 0）" tone="warn" icon-tone="violet">
        <template #icon>!</template>
      </StatCard>
    </div>

    <p class="su-kpi-note">
      <span class="wc-note__icon" aria-hidden="true">ⓘ</span>
      <span>
        口径说明：「已运行」与「上次启动」取自网关自检（real 模式为 <code>/api/overview</code>，
        mock 模式为演示数据）；「服务状态」以<b>最近一次网关应答是否正常</b>为准；
        <b>异常重启次数后端尚未提供统计字段</b>，因此显示「—」而不是假的「0」。
      </span>
    </p>

    <!-- 识别结果横幅 -->
    <section class="wc-card su-banner">
      <div class="su-banner__main">
        <div class="su-banner__label">当前部署形态（自动识别）</div>
        <div class="su-banner__form">
          <StatusTag :status="detectedForm" />
          <span class="su-banner__form-cn">{{ formCn }}</span>
        </div>
      </div>
      <button type="button" class="wc-btn wc-btn--sm" @click="showDetect = !showDetect">
        {{ showDetect ? '收起识别依据' : '识别依据' }}
      </button>
      <p v-if="showDetect" class="su-banner__note">
        识别逻辑：运行体启动后探测 <code>/.dockerenv</code> 是否存在，并判断 PID 1 是否由
        <code>systemd</code> 托管（<code>/run/systemd/system</code> 存在且被接管）。命中容器特征即判定为
        <b>Docker</b> 形态，否则判定为 <b>systemd</b> 原生服务形态。前端仅展示该识别结果，真实判定在 Rust 侧。
      </p>
    </section>

    <div class="wc-grid wc-grid--2">
      <!-- 启动策略（原型 :1998-2005） -->
      <section class="wc-card">
        <div class="wc-card__head">
          <h3>启动策略</h3>
          <span class="wc-card__sub">本地界面状态 · 网关未开放写端点</span>
        </div>
        <div class="wc-card__body">
          <div class="su-row">
            <div class="su-row__text">
              <div class="su-row__title">开机自启</div>
              <div class="su-row__desc">
                {{
                  detectedForm === 'docker'
                    ? '对应 compose 的 restart: unless-stopped，宿主重启/断电后自动拉起。'
                    : '对应 systemctl enable，宿主开机时由 systemd 自动拉起。'
                }}
              </div>
            </div>
            <RoleGate :allowed="canManage">
              <UiSwitch v-model="autostartEnabled" on-text="已启用" off-text="已关闭" @update:model-value="onAutostart" />
            </RoleGate>
            <span v-if="!canManage" class="wc-tag wc-tag--neutral">仅管理员可改</span>
          </div>

          <div class="su-row">
            <div class="su-row__text">
              <div class="su-row__title">崩溃自动重启</div>
              <div class="su-row__desc">进程异常退出后自动重启，指数退避（1s→2s→4s，上限 60s）。</div>
            </div>
            <UiSwitch v-model="crashRestart" on-text="已启用" off-text="已关闭" />
          </div>

          <div class="su-row">
            <div class="su-row__text">
              <div class="su-row__title">
                看门狗
                <span class="su-badge">需重启生效</span>
              </div>
              <div class="su-row__desc">心跳超时 90s 判定为假死，自动重启进程（panic 隔离）。</div>
            </div>
            <UiSwitch v-model="watchdog" on-text="已启用" off-text="已关闭" />
          </div>

          <div class="su-row">
            <div class="su-row__text">
              <div class="su-row__title">启动失败保护</div>
              <div class="su-row__desc">连续 5 次启动失败后自动暂停自启并触发告警，避免反复重启拖垮主机。</div>
            </div>
            <UiSwitch v-model="bootFailureGuard" on-text="已启用" off-text="已关闭" />
          </div>

          <p class="wc-note wc-note--warn">
            <span class="wc-note__icon" aria-hidden="true">⚠</span>
            <span>
              「启动失败保护」是必须项：配置写错导致启动即崩时，若无此保护会形成无限重启循环，
              在工控机上表现为整机变卡。上述开关当前<b>只改本机界面状态</b> —— 网关未开放对应写端点，
              真实生效依赖后续版本与部署侧 unit 文件。
            </span>
          </p>
        </div>
      </section>

      <!-- 启动方式识别（原型 :2007-2017） -->
      <section class="wc-card">
        <div class="wc-card__head">
          <h3>启动方式识别</h3>
          <span class="wc-card__sub">随部署形态自动判定</span>
        </div>
        <div class="wc-card__body">
          <dl class="wc-kv">
            <dt>部署形态</dt>
            <dd>
              <span class="wc-tag wc-tag--info">{{ formCn }}</span>
            </dd>
            <template v-if="detectedForm === 'docker'">
              <dt>容器名</dt><dd class="wc-mono">{{ runtime.docker.containerName }}</dd>
              <dt>镜像</dt><dd class="wc-mono">{{ runtime.docker.imageDigest }}</dd>
              <dt>自启机制</dt><dd class="wc-mono">restart: {{ runtime.docker.restartPolicy }}</dd>
              <dt>健康检查</dt><dd><StatusTag :status="runtime.docker.health" /></dd>
              <dt>编排文件</dt><dd class="wc-mono">{{ runtime.docker.composeFile }}</dd>
            </template>
            <template v-else>
              <dt>服务单元</dt><dd class="wc-mono">{{ runtime.native.serviceName }}</dd>
              <dt>运行状态</dt><dd class="wc-mono">{{ runtime.native.active }}</dd>
              <dt>开机自启</dt><dd class="wc-mono">{{ runtime.native.bootEnable ? 'enabled' : 'disabled' }}</dd>
              <dt>重启策略</dt><dd class="wc-mono">on-failure · 退避 5s</dd>
              <dt>资源限制</dt><dd class="wc-mono">CPU 400% · 内存 2G</dd>
            </template>
            <dt>数据目录</dt><dd class="wc-mono">/var/lib/iot-daq</dd>
          </dl>
          <p class="wc-note">
            <span class="wc-note__icon" aria-hidden="true">ⓘ</span>
            <span>
              两种形态的识别与命令均为<b>并列展示</b>，互不干扰；当前形态由网关自检高亮，另一形态标注「未采用」。
            </span>
          </p>
        </div>
      </section>
    </div>

    <!-- 双形态卡片：配置与命令并列 -->
    <div class="wc-grid wc-grid--2">
      <section class="wc-card su-form" :class="{ 'is-active': detectedForm === 'docker' }">
        <div class="wc-card__head">
          <h3>Docker 容器形态</h3>
          <span v-if="detectedForm === 'docker'" class="wc-tag wc-tag--info">当前形态</span>
          <span v-else class="wc-tag wc-tag--neutral">未采用</span>
        </div>
        <div class="wc-card__body">
          <div class="su-cmd">
            <span class="su-cmd__title">常用命令</span>
            <pre class="wc-mono">docker compose -f {{ runtime.docker.composeFile }} ps
docker compose -f {{ runtime.docker.composeFile }} logs -f
docker compose -f {{ runtime.docker.composeFile }} restart</pre>
          </div>
        </div>
      </section>

      <section class="wc-card su-form" :class="{ 'is-active': detectedForm === 'native' }">
        <div class="wc-card__head">
          <h3>systemd 服务形态</h3>
          <span v-if="detectedForm === 'native'" class="wc-tag wc-tag--info">当前形态</span>
          <span v-else class="wc-tag wc-tag--neutral">未采用</span>
        </div>
        <div class="wc-card__body">
          <div class="su-cmd">
            <span class="su-cmd__title">常用命令</span>
            <pre class="wc-mono">systemctl status {{ runtime.native.serviceName }}
journalctl -u {{ runtime.native.serviceName }} -f
systemctl enable --now {{ runtime.native.serviceName }}</pre>
          </div>
        </div>
      </section>
    </div>

    <div class="wc-grid wc-grid--2-1">
      <!-- 计划重启（原型 :2020-2031） -->
      <section class="wc-card">
        <div class="wc-card__head">
          <h3>计划重启</h3>
          <span class="wc-card__sub">避开产线时段</span>
        </div>
        <div class="wc-card__body">
          <div class="su-form-grid">
            <UiField label="启用计划重启">
              <UiSelect v-model="plan.enabled" :options="planEnabledOptions" />
            </UiField>
            <UiField label="执行时间" hint="建议避开产线交接班时段">
              <UiInput v-model="plan.at" placeholder="03:00" />
            </UiField>
            <UiField label="执行周期">
              <UiSelect v-model="plan.cycle" :options="cycleOptions" />
            </UiField>
            <UiField label="具体星期" hint="仅「每周」时生效">
              <UiSelect v-model="plan.weekday" :options="weekdayOptions" :disabled="plan.cycle !== '每周'" />
            </UiField>
          </div>
          <dl class="wc-kv">
            <dt>下次执行</dt>
            <dd class="wc-mono">{{ planNextText }}</dd>
            <dt>上次执行</dt>
            <dd class="wc-mono">—（网关未提供计划重启端点）</dd>
          </dl>
          <p class="wc-note wc-note--warn">
            <span class="wc-note__icon" aria-hidden="true">⚠</span>
            <span>
              网关当前<b>没有</b>计划重启端点：本表单只保存本机界面状态，<b>不会真的按时重启</b>，
              因此不给推算出来的「下次执行时间」（那是假数据）。现场需要定时重启请用宿主机 cron / systemd timer。
            </span>
          </p>
        </div>
      </section>

      <!-- 危险操作（原型 :2032-2042） -->
      <section class="wc-card">
        <div class="wc-card__head">
          <h3>危险操作</h3>
          <span class="wc-card__sub">需二次确认 + 原因</span>
        </div>
        <div class="wc-card__body">
          <div class="su-impact">
            <p class="su-impact__title"><span aria-hidden="true">⚠</span>重启与停止服务的影响</p>
            <ul>
              <li>重启期间<b>采集暂停约 5–15 秒</b>；已入队数据不会丢失，恢复后自动补发。</li>
              <li>停止服务后<b>北向转发中断</b>，且不再自动启动（除非自启开启）。</li>
              <li>操作需二次确认并填写原因，同时写入审计日志。</li>
            </ul>
          </div>

          <button type="button" class="wc-btn wc-btn--danger su-wide" @click="openRestart">立即重启服务</button>
          <button type="button" class="wc-btn wc-btn--danger su-wide" @click="openStop">停止服务（不自动启动）</button>
          <button type="button" class="wc-btn su-wide" @click="onLogs">查看日志</button>

          <div v-if="lastAction" class="su-result" :class="`is-${lastKind}`">
            <span aria-hidden="true">{{ lastKind === 'ok' ? '✓' : '⚠' }}</span>
            <span>{{ lastAction }}</span>
          </div>
        </div>
      </section>
    </div>
  </div>

  <!-- 重启服务：影响清单 + 原因必填 + 服务名二次校验（原型 confirmRestart :3345-3358） -->
  <DangerConfirmModal
    :open="restartModal.open"
    :title="`重启服务 · ${formCn}`"
    :impacts="restartModal.impacts"
    :facts="restartModal.facts"
    :reasons="RESTART_REASONS"
    :min-note-length="10"
    :confirm-value="SERVICE_KEY"
    confirm-label="风险二次确认（输入服务名）"
    :confirm-placeholder="`输入 ${SERVICE_KEY} 以确认`"
    confirm-text="确认重启"
    @close="restartModal.open = false"
    @submit="confirmRestart"
  />

  <!-- 停止服务：原因必填 + 服务名二次校验（原型 stopService :3131-3143） -->
  <DangerConfirmModal
    :open="stopModal.open"
    :title="`停止服务 · ${formCn}`"
    :impacts="stopModal.impacts"
    :facts="stopModal.facts"
    :reasons="STOP_REASONS"
    :min-note-length="10"
    :confirm-value="SERVICE_KEY"
    confirm-label="风险二次确认（输入服务名）"
    :confirm-placeholder="`输入 ${SERVICE_KEY} 以确认`"
    confirm-text="确认停止"
    @close="stopModal.open = false"
    @submit="confirmStop"
  />
</template>

<script setup lang="ts">
/**
 * @file StartupPage.vue
 * @module web-console/pages/StartupPage
 * @description 启动与自启：部署形态识别 / 启动策略 / 计划重启 / 高危运维动作。
 *
 * 危险动作一律经 `DangerConfirmModal`（影响清单 + 原因必填 + 服务名二次校验），
 * 确认后：mock 走既有演示行为并在结果区注明「未产生真实动作」；real 调用 `repo.ops.restart`。
 * 未实现的能力（计划重启、异常重启计数）一律显式留空说明，**不给假数据**。
 */
import { computed, reactive, ref } from 'vue';
import {
  PageHeader,
  StatCard,
  StatusTag,
  RoleGate,
  UiSwitch,
  UiSelect,
  UiField,
  UiInput,
  DangerConfirmModal,
  type SelectOption,
} from '@ui-kit';
import { API_MODE } from '@/api/client';
import { repo } from '@/api/repo';
import { session } from '../store/session';

/** 部署形态。 */
type RuntimeForm = 'docker' | 'native';

/** 二次校验用的服务名关键字（语原型 :3140「输入 iot-daq 以确认」）。 */
const SERVICE_KEY = 'iot-daq';

/** 自动识别出的当前形态（mock：默认 Docker，与 Linux 交付主推形态一致）。 */
const detectedForm = ref<RuntimeForm>('docker');

/** 形态中文名。 */
const formCn = computed(() => (detectedForm.value === 'docker' ? 'Docker 容器' : 'systemd 服务'));

/** 两种形态的运行态快照（只读展示）。 */
const runtime = {
  docker: {
    containerName: 'iot-daq-gateway',
    imageDigest: 'sha256:9f2c4b…a31e',
    restartPolicy: 'unless-stopped',
    health: 'healthy',
    composeFile: 'deploy/docker/docker-compose.yml',
  },
  native: {
    serviceName: 'iot-daq-gateway.service',
    active: 'active (running)',
    bootEnable: true,
  },
} as const;

/** 网关自检信息（real 模式来自 /api/overview）。 */
const gateway = computed(() => repo.getGateway());

/** 服务状态文案：以「是否拿到网关自检数据」为准，不假装知道进程状态。 */
const serviceStatusText = computed(() => (gateway.value.uptimeText && gateway.value.uptimeText !== '—' ? '运行中' : '未知'));

const serviceStatusSub = computed(() =>
  API_MODE === 'real'
    ? '最近一次 /api/overview 应答正常'
    : 'mock 演示态（未发真实请求）',
);

/** 是否可管理系统级自启（仅管理员）。 */
const canManage = computed(() => session.state.role === 'admin');

// ---------------------------------------------------------------------------
// 启动策略（本地界面状态；网关未开放写端点）
// ---------------------------------------------------------------------------

const autostartEnabled = ref(true);
const crashRestart = ref(true);
const watchdog = ref(true);
const bootFailureGuard = ref(true);

/** 识别依据展开态。 */
const showDetect = ref(false);

/** 最近一次操作反馈。 */
const lastAction = ref('');
const lastKind = ref<'ok' | 'warn'>('ok');

/** 结果区提示。 */
function note(text: string, kind: 'ok' | 'warn' = 'ok'): void {
  lastAction.value = text;
  lastKind.value = kind;
}

/** 自启开关变更（沿用既有演示行为：记录反馈，不伪造系统级落地）。 */
function onAutostart(next: boolean): void {
  autostartEnabled.value = next;
  const mechanism = detectedForm.value === 'docker' ? 'compose restart 策略' : 'systemd enable/disable';
  note(
    `已${next ? '启用' : '关闭'}开机自启（作用于 ${mechanism}）—— 仅更新本机界面状态与演示反馈，` +
      '网关未开放 systemd / compose 写端点，未产生任何系统级变更。',
    'warn',
  );
}

/** 查看日志（演示：给真实命令，不伪造日志内容）。 */
function onLogs(): void {
  const cmd =
    detectedForm.value === 'docker'
      ? `docker compose -f ${runtime.docker.composeFile} logs -f`
      : `journalctl -u ${runtime.native.serviceName} -f`;
  note('网关日志拉取受权限限制（ops 侧日志需更高权限），请在宿主机直接查看：' + cmd, 'warn');
}

// ---------------------------------------------------------------------------
// 计划重启（本机场无端点 → 表单只保存界面状态，不推算下次执行）
// ---------------------------------------------------------------------------

const plan = reactive({
  enabled: '停用',
  at: '03:00',
  cycle: '每周',
  weekday: '星期日',
});

const planEnabledOptions: readonly SelectOption[] = [
  { value: '启用', label: '启用' },
  { value: '停用', label: '停用' },
];

const cycleOptions: readonly SelectOption[] = [
  { value: '每周', label: '每周' },
  { value: '每日', label: '每日' },
  { value: '每月', label: '每月' },
];

const weekdayOptions: readonly SelectOption[] = ['星期一', '星期二', '星期三', '星期四', '星期五', '星期六', '星期日'].map(
  (d) => ({ value: d, label: d }),
);

/** 下次执行：无端点支撑时给诚实说明，而不是算一个假时间。 */
const planNextText = computed(() =>
  plan.enabled === '启用'
    ? `—（网关无计划重启端点；界面设定为 ${plan.cycle} ${plan.at}）`
    : '—（已停用）',
);

// ---------------------------------------------------------------------------
// 危险操作：重启 / 停止（DangerConfirmModal 契约）
// ---------------------------------------------------------------------------

const RESTART_REASONS: readonly string[] = [
  '配置变更需要生效',
  '进程异常需恢复',
  '例行维护窗口',
  '其它（请在补充说明中描述）',
];

const STOP_REASONS: readonly string[] = [
  '现场停机 / 检修',
  '更换硬件或迁移部署',
  '配置整改需短暂停机',
  '其它（请在补充说明中描述）',
];

const restartModal = reactive<{ open: boolean; impacts: readonly string[]; facts: readonly { label: string; value: string }[] }>({
  open: false,
  impacts: [],
  facts: [],
});

const stopModal = reactive<{ open: boolean; impacts: readonly string[]; facts: readonly { label: string; value: string }[] }>({
  open: false,
  impacts: [],
  facts: [],
});

/** 当前形态的服务单元 / 容器名，用于影响清单。 */
const unitName = computed(() =>
  detectedForm.value === 'docker' ? runtime.docker.containerName : runtime.native.serviceName,
);

function openRestart(): void {
  restartModal.impacts = [
    '采集暂停约 5–15 秒；已入队数据不丢，恢复后自动补发。',
    '北向转发短暂中断，客户 Broker 可能出现一个空档。',
    '若队列中有待补发数据，将优先 flush 后再退出（优雅停机）。',
    '操作不可撤销；原因与补充说明将写入审计日志。',
    `需输入服务名 ${SERVICE_KEY} 完成二次校验。`,
  ];
  restartModal.facts = [
    { label: '作用对象', value: unitName.value },
    { label: '网关标识', value: gateway.value.name },
    { label: '部署形态', value: formCn.value },
    { label: '操作者', value: session.state.displayName },
    { label: '执行模式', value: API_MODE === 'real' ? 'real（真实下发网关）' : 'mock（演示，无真实动作）' },
  ];
  restartModal.open = true;
}

function openStop(): void {
  stopModal.impacts = [
    '北向转发立即中断，客户 Broker 不再收到数据。',
    '本地采集也会停止；服务不会自动重新启动（除非再次手动启动）。',
    '重启主机时若开机自启开启，服务仍会被拉起。',
    '需输入服务名 ' + SERVICE_KEY + ' 完成二次校验。',
  ];
  stopModal.facts = [
    { label: '作用对象', value: unitName.value },
    { label: '部署形态', value: formCn.value },
    { label: '操作者', value: session.state.displayName },
    { label: '后端端点', value: '未提供（本次不会下发停止指令）' },
  ];
  stopModal.open = true;
}

/**
 * 确认重启：real 走 `repo.ops.restart`（真实 `POST /api/ops/restart`）；
 * 后端要求 body `{actor, confirm, reason}`，`confirm` 必须回显当前 gateway_id
 * （即 `/api/overview` 的 `name` 字段），此处由页面自动回显，用户弹窗输入的服务名
 * （`payload.tail`）作为前端侧二次校验门槛；mock 走既有演示行为。
 */
async function confirmRestart(payload: { reason: string; note: string }): Promise<void> {
  restartModal.open = false;
  const result = await repo.ops.restart({
    actor: session.state.displayName,
    confirm: gateway.value.name,
    reason: `${payload.reason} · ${payload.note}`,
  });
  const tail = `原因：${payload.reason} · ${payload.note}`;
  if (API_MODE === 'real') {
    note(result.ok ? `${result.message} ${tail}` : `重启未成功：${result.message} ${tail}`, result.ok ? 'ok' : 'warn');
    return;
  }
  note(`${result.message} ${tail} —— 演示模式未向网关发出任何重启信号，服务状态不变。`, 'warn');
}

/** 确认停止：后端无停止端点，因此诚实告知「仅记下意图 + 给宿主机命令」，不假装已停止。 */
function confirmStop(payload: { reason: string; note: string }): void {
  stopModal.open = false;
  const cmd =
    detectedForm.value === 'docker'
      ? `docker compose -f ${runtime.docker.composeFile} stop`
      : `systemctl stop ${runtime.native.serviceName}`;
  note(
    `已记录停止意图并通过二次校验（原因：${payload.reason} · ${payload.note}）。` +
      `网关当前未提供停止服务端点，因此【未执行任何真实动作】，服务仍在运行 —— 请在宿主机执行：${cmd}`,
    'warn',
  );
}
</script>

<style scoped>
.su-kpi-note {
  display: flex;
  align-items: flex-start;
  gap: 8px;
  margin: 0 0 16px;
  font-size: var(--fs-caption);
  color: var(--text-3);
  line-height: 1.6;
}

.su-banner {
  display: flex;
  align-items: center;
  gap: 16px;
  flex-wrap: wrap;
}
.su-banner__main {
  flex: 1 1 auto;
  min-width: 220px;
}
.su-banner__label {
  font-size: var(--fs-caption);
  color: var(--text-3);
  margin-bottom: 6px;
}
.su-banner__form {
  display: flex;
  align-items: center;
  gap: 10px;
}
.su-banner__form-cn {
  font-size: var(--fs-h3);
  font-weight: 600;
  color: var(--text-1);
}
.su-banner__note {
  flex-basis: 100%;
  margin: 4px 0 0;
  padding-top: 12px;
  border-top: 1px dashed var(--border);
  font-size: var(--fs-caption);
  color: var(--text-3);
  line-height: 1.7;
}
.su-banner__note code {
  font-family: var(--font-mono);
  background: var(--bg-hover);
  padding: 1px 5px;
  border-radius: var(--radius-sm);
  color: var(--text-2);
}

/* 开关行 */
.su-row {
  display: flex;
  align-items: center;
  gap: 16px;
  flex-wrap: wrap;
  padding: 12px 0;
  border-bottom: 1px solid var(--divider);
}
.su-row:last-of-type {
  border-bottom: 0;
}
.su-row__text {
  flex: 1 1 auto;
  min-width: 220px;
}
.su-row__title {
  font-size: var(--fs-body);
  font-weight: 600;
  color: var(--text-1);
  display: flex;
  align-items: center;
  gap: 8px;
}
.su-row__desc {
  font-size: var(--fs-caption);
  color: var(--text-3);
  margin-top: 4px;
  line-height: 1.6;
}
.su-badge {
  font-size: var(--fs-caption);
  padding: 1px 8px;
  border-radius: var(--radius-pill);
  border: 1px solid var(--warn-border);
  background: var(--warn-bg);
  color: var(--warn-fg);
  font-weight: 600;
}

/* 双形态卡片 */
.su-form {
  transition: border-color 0.16s ease, box-shadow 0.16s ease;
}
.su-form.is-active {
  border-color: var(--brand);
  box-shadow: 0 0 0 1px var(--brand) inset;
}
.su-form-grid {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: 14px 16px;
  margin-bottom: 14px;
}
.su-cmd__title {
  display: block;
  font-size: var(--fs-caption);
  color: var(--text-3);
  margin-bottom: 6px;
}
.su-cmd pre {
  margin: 0;
  background: var(--bg-hover);
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  padding: 10px 12px;
  font-size: 12px;
  line-height: 1.6;
  color: var(--text-2);
  overflow-x: auto;
  white-space: pre;
}

/* 影响块 */
.su-impact {
  padding: 12px 14px;
  border: 1px solid var(--warn-border);
  background: var(--warn-bg);
  border-radius: var(--radius);
  margin-bottom: 12px;
}
.su-impact__title {
  margin: 0;
  font-size: var(--fs-table);
  font-weight: 600;
  color: var(--warn-fg);
}
.su-impact ul {
  margin: 6px 0 0;
  padding-left: 18px;
  font-size: var(--fs-caption);
  color: var(--text-2);
  line-height: 1.75;
}

/* 宽按钮 + 结果区 */
.su-wide {
  width: 100%;
  margin-bottom: 8px;
}
.su-result {
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
.su-result.is-warn {
  border-color: var(--warn-border);
  background: var(--warn-bg);
  color: var(--warn-fg);
}
.wc-note--warn .wc-note__icon {
  color: var(--warn);
}
</style>
