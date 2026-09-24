<template>
  <!--
    OverviewPage —— 总览 / 仪表盘（监控分组第 1 页，路由 `/overview`）。

    结构对齐 `docs/design/prototype/gateway-v2a-glacier.html` 的 `overview`（:1384 起）：
      ① 授权横幅（条件显示，§4.6 优先级）
      ② KPI 卡片行（在线设备 / 采集点位 / 今日上行 / 端到端延迟）
      ③ 2:1 栅格：采集吞吐（面积折线，内联 SVG）+ 系统状态（KV）
      ④ 3 列栅格：设备健康 / 北向出口 / 待处理
      ⑤ 最近告警列表（分页，UiPager 单一口径）

    硬性约定遵守情况：
      · 只用 ui-kit 组件（`StatCard` / `PageHeader` / `UiTable` / `UiPager` / `StatusTag` / `EmptyState`）；
      · 图表为内联 SVG，**未引入任何新图表库**；
      · 实时数值 1s 节流渲染（`setInterval` + mock 推流），界面标注「演示数据」；
      · 列表条数只有 UiPager 一个口径（表格 `footer` 不重复写「共 N 条」）；
      · 无解绑 / 重置试用 / revoke 任何入口。
  -->
  <PageHeader
    crumb="运行监控 / 总览"
    title="总览"
    desc="本机网关运行全景：采集吞吐、设备健康、北向出口与授权状态。聚合数值按 1s 节流刷新，避免高频重绘打满浏览器。"
  >
    <template #actions>
      <span class="wc-tag wc-tag--info" title="当前为内嵌演示数据源，未接入真实 WebSocket">演示数据</span>
      <button type="button" class="wc-btn" @click="refreshAll">立即刷新</button>
    </template>
  </PageHeader>

  <div class="wc-content">
    <!-- ① 授权横幅：同一时刻只显示最高优先级一条（§4.6） -->
    <div v-if="banner" class="wc-banner" :class="`wc-banner--${banner.tone}`">
      <div>
        <b>{{ banner.title }}</b>
        <div>{{ banner.detail }}</div>
      </div>
      <div class="wc-banner__ops">
        <button v-for="act in banner.actions" :key="act.label" type="button" class="wc-btn wc-btn--sm" @click="go(act.page)">
          {{ act.label }}
        </button>
      </div>
    </div>

    <!-- ② KPI 卡片行：数值来自 mock 聚合 + 1s 节流推流 -->
    <div class="wc-grid wc-grid--4">
      <StatCard
        label="设备在线"
        :value="`${live.onlineCount}`"
        :unit="`/ ${gateway.deviceCount}`"
        :sub="onlineSub"
        :tone="live.onlineCount === gateway.deviceCount ? 'ok' : 'warn'"
        clickable
        @click="go('devices')"
      />
      <StatCard
        label="采集点位"
        :value="formatInt(gateway.pointCount)"
        :sub="`${gateway.failedPointCount} 个失败点位`"
        :tone="gateway.failedPointCount > 0 ? 'warn' : 'default'"
        clickable
        @click="go('points')"
      />
      <StatCard
        label="今日上行"
        :value="formatMillion(live.forwardRatePerSec)"
        unit="条/秒"
        :delta="8.4"
        :sub="`累计 ${formatBig(gateway.totalForwardedRecords)} 条`"
      />
      <StatCard
        label="端到端延迟"
        :value="`${live.latencyMs}`"
        unit="ms"
        :delta="-12"
        sub="采集 → Broker"
      />
    </div>

    <!-- ③ 采集吞吐 + 系统状态 -->
    <div class="wc-grid wc-grid--2-1">
      <section class="wc-card">
        <div class="wc-card__head">
          <h3>采集吞吐</h3>
          <span class="wc-card__sub">最近 24 小时 · 条/分钟</span>
          <div class="wc-card__ops">
            <button
              v-for="range in RANGES"
              :key="range.id"
              type="button"
              class="wc-btn wc-btn--sm"
              :class="{ 'wc-btn--primary': range.id === activeRange }"
              @click="activeRange = range.id"
            >
              {{ range.label }}
            </button>
          </div>
        </div>
        <div class="wc-card__body">
          <!-- 面积折线（内联 SVG，零图表依赖） -->
          <svg
            class="ov-chart"
            :viewBox="`0 0 ${CHART_W} ${CHART_H}`"
            preserveAspectRatio="none"
            role="img"
            aria-label="采集吞吐面积折线图：成功采集与补发两条序列"
          >
            <g v-for="g in 4" :key="`grid-${g}`">
              <line
                :x1="0"
                :y1="gridY(g - 1)"
                :x2="CHART_W"
                :y2="gridY(g - 1)"
                stroke="#F2F3F5"
                stroke-width="1"
              />
              <text :x="4" :y="gridY(g - 1) - 4" font-size="9" fill="#86909C">{{ gridLabel(g - 1) }}</text>
            </g>

            <!-- 成功采集：面积 + 折线 -->
            <path :d="successArea" fill="rgba(0,168,112,0.12)" stroke="none" />
            <path :d="successLine" fill="none" stroke="#00A870" stroke-width="1.8" />
            <!-- 补发：折线（同为「条/分钟」量纲，共用 Y 轴） -->
            <path :d="replayLine" fill="none" stroke="#7A5AF8" stroke-width="1.4" stroke-dasharray="4 3" />

            <circle
              v-for="(pt, i) in successPoints"
              :key="`dot-${i}`"
              :cx="pt.x"
              :cy="pt.y"
              r="2"
              fill="#00A870"
            />
          </svg>

          <div class="ov-legend">
            <span><i style="background: #00a870" />成功采集（条/分钟）</span>
            <span><i style="background: #7a5af8" />补发（条/分钟）</span>
            <span class="wc-mono">峰值 {{ peakText }}</span>
          </div>
        </div>
      </section>

      <section class="wc-card">
        <div class="wc-card__head">
          <h3>系统状态</h3>
          <span class="wc-card__sub">本机采样</span>
        </div>
        <div class="wc-card__body">
          <dl class="wc-kv">
            <dt>CPU 占用</dt>
            <dd class="wc-mono">{{ live.cpuPct.toFixed(1) }} %</dd>
            <dt>内存占用</dt>
            <dd class="wc-mono">{{ live.memUsedMb }} MB / 2048 MB</dd>
            <dt>数据目录占用</dt>
            <dd class="wc-mono">{{ gateway.queueUsedGb }} GB / {{ gateway.queueCapacityGb }} GB</dd>
            <dt>离线队列</dt>
            <dd class="wc-mono">{{ formatInt(live.queueDepth) }} 条</dd>
            <dt>连续运行</dt>
            <dd class="wc-mono">{{ gateway.uptimeText }}</dd>
            <dt>版本</dt>
            <dd class="wc-mono">{{ gateway.version }}</dd>
          </dl>
          <div class="ov-meter">
            <div class="ov-meter__head">
              <span>磁盘队列水位</span>
              <span class="wc-mono">{{ queuePct }}% · 可续传 ≈{{ gateway.queueDrainDays }} 天</span>
            </div>
            <div class="wc-bar">
              <div class="wc-bar__fill" :class="queueFillClass" :style="{ width: `${queuePct}%` }" />
            </div>
          </div>
        </div>
      </section>
    </div>

    <!-- ④ 设备健康 / 北向出口 / 待处理 -->
    <div class="wc-grid wc-grid--3">
      <section class="wc-card">
        <div class="wc-card__head">
          <h3>设备健康</h3>
          <span class="wc-card__sub">按严重度排序</span>
        </div>
        <div class="wc-card__body wc-card__body--flush">
          <UiTable :columns="healthColumns" :rows="healthRows">
            <template #cell-name="{ row }">
              <span>{{ row.name }}</span>
              <span class="wc-card__sub"> · {{ row.protocolLabel }}</span>
            </template>
            <template #cell-status="{ row }">
              <StatusTag :status="row.status" />
            </template>
          </UiTable>
        </div>
      </section>

      <section class="wc-card">
        <div class="wc-card__head">
          <h3>北向出口</h3>
          <span class="wc-card__sub">每路编码独立</span>
        </div>
        <div class="wc-card__body wc-card__body--flush">
          <UiTable :columns="forwarderColumns" :rows="forwarders">
            <template #cell-name="{ row }">{{ row.name }}</template>
            <template #cell-encoding="{ row }">
              <span class="wc-tag" :class="row.encoding === 'protobuf' ? 'wc-tag--ok' : 'wc-tag--info'">
                {{ row.encoding }}
              </span>
            </template>
            <template #cell-status="{ row }">
              <StatusTag :status="forwarderStatusKey(row.status)" />
            </template>
          </UiTable>
          <p class="ov-foot-note">断网自动转存本地队列，恢复后按序补发。</p>
        </div>
      </section>

      <section class="wc-card">
        <div class="wc-card__head">
          <h3>待处理</h3>
          <span class="wc-card__sub">按优先级</span>
        </div>
        <div class="wc-card__body">
          <div class="wc-list">
            <div v-for="item in todos" :key="item.title" class="wc-list__item">
              <div>
                <div class="wc-list__title">
                  <span class="ov-dot" :class="`ov-dot--${item.tone}`" aria-hidden="true" />{{ item.title }}
                </div>
                <div class="wc-list__desc">{{ item.desc }}</div>
              </div>
              <div class="wc-list__ops">
                <button type="button" class="wc-btn wc-btn--sm" @click="go(item.page)">{{ item.action }}</button>
              </div>
            </div>
          </div>
        </div>
      </section>
    </div>

    <!-- ⑤ 最近告警（列表页必须分页；条数只有 UiPager 一个口径） -->
    <section class="wc-card">
      <div class="wc-card__head">
        <h3>最近告警</h3>
        <span class="wc-card__sub">按最近触发时间倒序 · 5 分钟自动刷新</span>
        <div class="wc-card__ops">
          <button type="button" class="wc-btn wc-btn--sm" @click="go('alarms')">前往告警中心</button>
        </div>
      </div>

      <EmptyState
        v-if="paged.total === 0"
        title="暂无告警"
        desc="当前没有产生任何告警记录。若刚完成设备接入，可到「实时监控」确认点位质量，或先配置告警规则以便异常自动上报。"
      >
        <template #actions>
          <button type="button" class="wc-btn" @click="go('monitor')">前往实时监控</button>
          <button type="button" class="wc-btn wc-btn--primary" @click="go('alarms')">配置告警规则</button>
        </template>
      </EmptyState>

      <template v-else>
        <UiTable :columns="alarmColumns" :rows="paged.items" row-key-field="id">
          <template #cell-level="{ row }">
            <span class="wc-tag" :class="levelTagClass(row.level)">{{ row.levelLabel }}</span>
          </template>
          <template #cell-sourceLabel="{ row }">{{ row.sourceLabel }}</template>
          <template #cell-title="{ row }">{{ row.title }}</template>
          <template #cell-lastSeenAt="{ row }">
            <span class="wc-mono">{{ row.lastSeenAt }}</span>
          </template>
          <template #cell-stateLabel="{ row }">
            <span class="wc-tag" :class="stateTagClass(row.state)">{{ row.stateLabel }}</span>
          </template>
        </UiTable>
        <UiPager :page="paged.page" :total="paged.total" :page-size="ALARM_PAGE_SIZE" @update:page="onAlarmPage" />
      </template>
    </section>

    <p class="wc-note">
      <span class="wc-note__icon">ⓘ</span>
      <span>
        本页所有数值来自内嵌演示数据源并按 1s 节流刷新；真实环境由 <span class="wc-mono">GET /api/overview</span> 与
        <span class="wc-mono">GET /api/stream</span>（WebSocket）提供。授权状态一律由网关侧（Rust）判定，前端仅展示。
      </span>
    </p>
  </div>
</template>

<script setup lang="ts">
/**
 * @file OverviewPage.vue
 * @module web-console/pages/OverviewPage
 * @description 总览页。数据来自 `repo`（mock 仓库）+ 1s 节流推流模拟。
 *
 * ── 节流契约 ────────────────────────────────────────────────────────────────
 * 真实 WS 会以远高于 1s 的频率推送。这里用一个 **1s 的 `setInterval`** 代表
 * 「节流后的渲染节拍」：无论上游多快，Vue 的响应式更新每秒至多一次，
 * 保证 200 设备 × 100ms 场景下浏览器不被重绘打满（设计系统 §4.2 硬约束）。
 */
import { computed, onBeforeUnmount, onMounted, reactive, ref } from 'vue';
import { useRouter } from 'vue-router';
import {
  EmptyState,
  PageHeader,
  StatCard,
  StatusTag,
  UiPager,
  UiTable,
  type TableColumn,
} from '@ui-kit';
import {
  repo,
  type AlarmLevel,
  type AlarmRecord,
  type AlarmState,
  type DeviceRecord,
  type ForwarderRecord,
} from '../mock/mock-data';
import { session } from '../store/session';

const router = useRouter();

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/** 最近告警每页条数。 */
const ALARM_PAGE_SIZE = 5;

/** 采集吞吐图表逻辑坐标系（`preserveAspectRatio="none"` 由外层拉伸）。 */
const CHART_W = 620;
const CHART_H = 170;

/** 时间范围切换（仅切换演示数据集，真实环境为查询参数）。 */
const RANGES: readonly { id: '24h' | '7d'; label: string }[] = [
  { id: '24h', label: '24h' },
  { id: '7d', label: '7d' },
];

/** 设备健康列定义。 */
const healthColumns: readonly TableColumn[] = [
  { key: 'name', label: '设备' },
  { key: 'pointCount', label: '点位', align: 'right' },
  { key: 'status', label: '状态' },
];

/** 北向出口列定义。 */
const forwarderColumns: readonly TableColumn[] = [
  { key: 'name', label: '出口' },
  { key: 'encoding', label: '编码' },
  { key: 'status', label: '状态' },
];

/** 最近告警列定义。 */
const alarmColumns: readonly TableColumn[] = [
  { key: 'level', label: '级别' },
  { key: 'lastSeenAt', label: '最近触发', mono: true },
  { key: 'sourceLabel', label: '对象' },
  { key: 'title', label: '内容' },
  { key: 'stateLabel', label: '状态' },
];

// ---------------------------------------------------------------------------
// 静态快照（来自 mock 聚合，非硬编码重复值）
// ---------------------------------------------------------------------------

/** 本机网关信息（`GET /api/overview` 镜像）。 */
const gateway = repo.getGateway();

/** 全量设备（用于 KPI 与设备健康排序）。 */
const devices: DeviceRecord[] = repo.allDevices();

/** 全量北向出口。 */
const forwarders: ForwarderRecord[] = repo.allForwarders();

/** 全部告警（页面内分页）。 */
const allAlarms: AlarmRecord[] = repo.allAlarms();

// ---------------------------------------------------------------------------
// 1s 节流推流（模拟 WS）
// ---------------------------------------------------------------------------

/**
 * 节流后的实时快照。**每秒最多写一次**，避免每帧 setState。
 */
const live = reactive({
  /** 采集频率（点/秒），围绕 gateway.sampleRatePerSec 抖动 */
  sampleRatePerSec: gateway.sampleRatePerSec,
  /** 北向转发速率（条/秒） */
  forwardRatePerSec: gateway.forwardRatePerSec,
  /** 端到端延迟（ms） */
  latencyMs: 86,
  /** CPU 占用（%） */
  cpuPct: 18.4,
  /** 内存占用（MB） */
  memUsedMb: 412,
  /** 离线队列深度（条） */
  queueDepth: 1204,
  /** 在线设备数（围绕 gateway.onlineCount 抖动） */
  onlineCount: gateway.onlineCount,
});

/** 上次节拍时间（用于计算「最后更新」与抖动相位）。 */
const lastTickAt = ref<number>(Date.now());

/** 节流定时器句柄（组件卸载必须清理）。 */
let timer: ReturnType<typeof setInterval> | null = null;

/** 有界随机抖动：围绕基准值 ±range，并夹取到 [min, +∞)。 */
function jitter(base: number, range: number, min: number): number {
  return Math.max(min, base + (Math.random() * 2 - 1) * range);
}

/**
 * 单个节拍：更新 `live` 快照。
 *
 * 该函数由 1s 定时器调用 —— 这**就是**节流点：上游再怎么高频，
 * 组件每秒只重渲染一次。
 */
function tick(): void {
  live.sampleRatePerSec = Math.round(jitter(gateway.sampleRatePerSec, 24, 1));
  live.forwardRatePerSec = Math.round(jitter(gateway.forwardRatePerSec, 30, 0));
  live.latencyMs = Math.round(jitter(86, 9, 12));
  live.cpuPct = Number(jitter(18.4, 3.2, 1).toFixed(1));
  live.memUsedMb = Math.round(jitter(412, 14, 128));
  live.queueDepth = Math.round(jitter(1204, 60, 0));
  live.onlineCount = Math.min(gateway.deviceCount, Math.max(0, Math.round(jitter(gateway.onlineCount, 0.6, 0))));
  lastTickAt.value = Date.now();
}

/** 手动刷新（页头按钮）：立即执行一次节拍并复位视图状态。 */
function refreshAll(): void {
  tick();
  activeRange.value = '24h';
}

onMounted(() => {
  tick();
  timer = setInterval(tick, 1000);
});

onBeforeUnmount(() => {
  if (timer !== null) {
    clearInterval(timer);
    timer = null;
  }
});

/** 最后更新时刻（`HH:mm:ss`，供无障碍与排障）。 */
const lastTickText = computed<string>(() => {
  const d = new Date(lastTickAt.value);
  const p = (n: number): string => String(n).padStart(2, '0');
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
});

// ---------------------------------------------------------------------------
// 授权横幅（§4.6 优先级：废弃 > 心跳超期 > 试用 ≤2 天 > 队列高位 > 通道断开 > 安全模式）
// ---------------------------------------------------------------------------

/** 横幅视图模型。 */
interface BannerView {
  /** 色调 */
  readonly tone: 'warn' | 'danger' | 'info';
  /** 标题（一句话结论） */
  readonly title: string;
  /** 明细（原因 + 恢复路径） */
  readonly detail: string;
  /** 下一步动作 */
  readonly actions: readonly { label: string; page: string }[];
}

/** 队列水位百分比。 */
const queuePct = computed<number>(() => Math.round((gateway.queueUsedGb / gateway.queueCapacityGb) * 100));

/**
 * 计算当前应显示的横幅。
 *
 * 同屏只显示一条（最高优先级）；无异常时返回 `null`（不占位）。
 */
const banner = computed<BannerView | null>(() => {
  const lic = session.state.license;

  // 优先级 1：授权已停用（心跳超期 / 被后台废弃）
  if (lic.status === 'stopped' || lic.status === 'grace') {
    return {
      tone: 'danger',
      title: lic.degradeReason || '云端心跳超期，北向转发已停用；本地采集继续。',
      detail: `恢复路径：${lic.onExpireText} 当前版本 ${gateway.version}，授权判定在网关侧完成。`,
      actions: [
        { label: '前往授权与激活', page: 'license' },
        { label: '查看日志', page: 'audit' },
      ],
    };
  }

  // 优先级 2：试用剩余 ≤ 2 天
  if (lic.status === 'trial' && lic.remainingDays <= 2) {
    return {
      tone: 'warn',
      title: `试用剩余 ${lic.remainingText}，到期后${lic.onExpireText}`,
      detail: '本地采集不受影响；北向转发到期后停用。可提前输入激活码，激活后能力立即恢复。',
      actions: [
        { label: '输入激活码', page: 'license' },
        { label: '了解差异', page: 'license' },
      ],
    };
  }

  // 优先级 3：磁盘队列高位（≥ 60%）
  if (queuePct.value >= 60) {
    return {
      tone: 'warn',
      title: `磁盘队列 ${gateway.queueUsedGb} GB / ${gateway.queueCapacityGb} GB，按当前速率可续传 ≈${gateway.queueDrainDays} 天`,
      detail: '北向出口不可达时数据转入磁盘队列，恢复后按序补发。请检查出口联通性与补发进度。',
      actions: [
        { label: '查看北向转发', page: 'northbound' },
        { label: '查看告警', page: 'alarms' },
      ],
    };
  }

  // 优先级 4：实时通道断开
  if (session.state.connection !== 'connected') {
    return {
      tone: 'warn',
      title: `实时通道已降级（${session.state.connection === 'disconnected' ? '已断开' : '链路降级'}），页面数值可能停止刷新`,
      detail: `最后更新 ${lastTickText.value}。本地采集继续运行；请检查网关侧实时通道与网络。`,
      actions: [{ label: '查看诊断', page: 'diagnose' }],
    };
  }

  return null;
});

// ---------------------------------------------------------------------------
// KPI 派生
// ---------------------------------------------------------------------------

/** 在线设备副标题。 */
const onlineSub = computed<string>(() => {
  const offline = gateway.deviceCount - live.onlineCount;
  return offline > 0 ? `离线 ${offline} 台` : '全部在线';
});

/** 磁盘队列进度条色调（逼近上限转 danger）。 */
const queueFillClass = computed<string>(() => {
  if (queuePct.value >= 85) {
    return 'wc-bar__fill--danger';
  }
  if (queuePct.value >= 60) {
    return 'wc-bar__fill--warn';
  }
  return 'wc-bar__fill--ok';
});

// ---------------------------------------------------------------------------
// 采集吞吐图表（内联 SVG，24h / 7d 两套演示数据）
// ---------------------------------------------------------------------------

/** 当前时间范围。 */
const activeRange = ref<'24h' | '7d'>('24h');

/** 24h 数据集（12 个两小时桶，单位：条/分钟）。 */
const RANGE_24H: readonly { label: string; success: number; replay: number }[] = [
  { label: '00', success: 8200, replay: 200 },
  { label: '02', success: 8800, replay: 180 },
  { label: '04', success: 9100, replay: 260 },
  { label: '06', success: 8700, replay: 340 },
  { label: '08', success: 9400, replay: 220 },
  { label: '10', success: 10200, replay: 180 },
  { label: '12', success: 11800, replay: 300 },
  { label: '14', success: 11200, replay: 420 },
  { label: '16', success: 10900, replay: 280 },
  { label: '18', success: 11600, replay: 200 },
  { label: '20', success: 12400, replay: 240 },
  { label: '22', success: 13000, replay: 320 },
];

/** 7d 数据集（7 个日桶，单位：条/分钟）。 */
const RANGE_7D: readonly { label: string; success: number; replay: number }[] = [
  { label: '09-17', success: 10100, replay: 260 },
  { label: '09-18', success: 11200, replay: 320 },
  { label: '09-19', success: 10800, replay: 280 },
  { label: '09-20', success: 11900, replay: 240 },
  { label: '09-21', success: 12400, replay: 300 },
  { label: '09-22', success: 11700, replay: 360 },
  { label: '09-23', success: 13000, replay: 320 },
];

/** 当前数据集。 */
const chartData = computed<readonly { label: string; success: number; replay: number }[]>(() =>
  activeRange.value === '24h' ? RANGE_24H : RANGE_7D,
);

/** Y 轴上界（取成功序列峰值 × 1.15，留顶部余量）。 */
const chartMax = computed<number>(() => {
  const peak = Math.max(...chartData.value.map((d) => d.success));
  return Math.max(peak * 1.15, 1);
});

/** 第 i 个点的 x 坐标。 */
function xOf(index: number): number {
  const n = chartData.value.length;
  return n <= 1 ? 0 : (index / (n - 1)) * CHART_W;
}

/** 数值 → y 坐标（自底向上）。 */
function yOf(value: number): number {
  return CHART_H - (Math.min(value, chartMax.value) / chartMax.value) * CHART_H;
}

/** 折线路径（`M` + `L`）。 */
function linePath(pick: (d: { label: string; success: number; replay: number }) => number): string {
  return chartData.value.map((d, i) => `${i === 0 ? 'M' : 'L'}${xOf(i).toFixed(1)},${yOf(pick(d)).toFixed(1)}`).join(' ');
}

/** 成功采集折线路径。 */
const successLine = computed<string>(() => linePath((d) => d.success));

/** 补发折线路径。 */
const replayLine = computed<string>(() => linePath((d) => d.replay));

/** 成功采集面积路径（折线 + 回落到基线闭合）。 */
const successArea = computed<string>(() => {
  const top = successLine.value;
  const lastX = xOf(chartData.value.length - 1).toFixed(1);
  return `${top} L${lastX},${CHART_H} L0,${CHART_H} Z`;
});

/** 成功序列折线顶点（用于打点）。 */
const successPoints = computed<readonly { x: number; y: number }[]>(() =>
  chartData.value.map((d, i) => ({ x: xOf(i), y: yOf(d.success) })),
);

/** Y 轴网格线 y 坐标（t: 0 = 顶部）。 */
function gridY(t: number): number {
  return (CHART_H * t) / 3;
}

/** Y 轴刻度文案（k 缩写）。 */
function gridLabel(t: number): string {
  const value = chartMax.value * (1 - t / 3);
  return value >= 1000 ? `${(value / 1000).toFixed(1)}k` : String(Math.round(value));
}

/** 峰值文案。 */
const peakText = computed<string>(() => {
  const peak = Math.max(...chartData.value.map((d) => d.success));
  return `${(peak / 1000).toFixed(1)}k 条/分钟`;
});

// ---------------------------------------------------------------------------
// 设备健康（按严重度排序：离线 → 采集失败 → 在线）
// ---------------------------------------------------------------------------

/** 严重度权重（数值越小越靠前）。 */
const SEVERITY: Readonly<Record<string, number>> = Object.freeze({ offline: 0, error: 1, online: 2 });

/** 设备健康行（按严重度排序后取前 6 条）。 */
const healthRows = computed<readonly DeviceRecord[]>(() =>
  [...devices]
    .sort((a, b) => {
      const sa = SEVERITY[a.status] ?? 9;
      const sb = SEVERITY[b.status] ?? 9;
      if (sa !== sb) {
        return sa - sb;
      }
      return a.name.localeCompare(b.name, 'zh-Hans-CN');
    })
    .slice(0, 6),
);

// ---------------------------------------------------------------------------
// 待处理清单（来自真实快照，非硬编码）
// ---------------------------------------------------------------------------

/** 待处理项。 */
interface TodoItem {
  /** 标题 */
  readonly title: string;
  /** 说明 */
  readonly desc: string;
  /** 色调 */
  readonly tone: 'ok' | 'warn' | 'danger';
  /** 动作文案 */
  readonly action: string;
  /** 目标路由 name */
  readonly page: string;
}

/** 待处理清单（由 mock 快照实时推导）。 */
const todos = computed<readonly TodoItem[]>(() => {
  const items: TodoItem[] = [];
  const lic = session.state.license;

  for (const dev of devices) {
    if (dev.status === 'offline') {
      items.push({
        title: `${dev.name} 已离线`,
        desc: `${dev.protocolLabel} · ${dev.connectionSummary} · ${dev.offlineText || '通讯中断'}`,
        tone: 'danger',
        action: '去排查',
        page: 'devices',
      });
    } else if (dev.status === 'error') {
      items.push({
        title: `${dev.name} 采集失败 ${dev.failStreak} 次`,
        desc: `连接成功率 ${dev.successRate}%。建议检查从站通讯与采集参数。`,
        tone: 'warn',
        action: '查看错误',
        page: 'points',
      });
    }
  }

  if (gateway.failedPointCount > 0) {
    items.push({
      title: `${gateway.failedPointCount} 个点位质量异常`,
      desc: '点位质量非 Good，确认是否影响北向转发的完整性。',
      tone: 'warn',
      action: '查看点位',
      page: 'points',
    });
  }

  items.push({
    title: `授权 ${lic.status === 'active' ? '有效' : '需关注'}：${lic.tierName}`,
    desc: `租约有效至 ${lic.validUntil} · 上次心跳 ${lic.lastHeartbeatAt}`,
    tone: lic.status === 'active' ? 'ok' : 'warn',
    action: '查看授权',
    page: 'license',
  });

  return items.slice(0, 4);
});

// ---------------------------------------------------------------------------
// 最近告警（分页）
// ---------------------------------------------------------------------------

/** 最近告警页码（从 1 开始）。 */
const alarmPage = ref<number>(1);

/** 按最近触发时间倒序的告警。 */
const sortedAlarms = computed<readonly AlarmRecord[]>(() =>
  [...allAlarms].sort((a, b) => b.lastSeenAt.localeCompare(a.lastSeenAt)),
);

/** 当前页告警（页面内分页）。 */
const paged = computed<{ items: AlarmRecord[]; total: number; page: number }>(() => {
  const total = sortedAlarms.value.length;
  const start = (alarmPage.value - 1) * ALARM_PAGE_SIZE;
  return {
    items: sortedAlarms.value.slice(start, start + ALARM_PAGE_SIZE),
    total,
    page: alarmPage.value,
  };
});

/** 告警换页。 */
function onAlarmPage(next: number): void {
  alarmPage.value = next;
}

/** 告警级别标签色调。 */
function levelTagClass(level: AlarmLevel): string {
  if (level === 'critical') {
    return 'wc-tag--danger';
  }
  if (level === 'major') {
    return 'wc-tag--warn';
  }
  if (level === 'minor') {
    return 'wc-tag--info';
  }
  return 'wc-tag--unknown';
}

/** 告警处置状态标签色调。 */
function stateTagClass(state: AlarmState): string {
  if (state === 'resolved') {
    return 'wc-tag--ok';
  }
  if (state === 'acking') {
    return 'wc-tag--info';
  }
  return 'wc-tag--warn';
}

/** 北向出口状态 → ui-kit 状态枚举键（统一文案与配色）。 */
function forwarderStatusKey(status: string): string {
  if (status === 'connected') {
    return 'online';
  }
  if (status === 'disconnected') {
    return 'inactive';
  }
  return 'reconnecting';
}

// ---------------------------------------------------------------------------
// 格式化与导航
// ---------------------------------------------------------------------------

/** 千分位整数。 */
function formatInt(value: number): string {
  return value.toLocaleString('en-US');
}

/** 大整数（string 输入）千分位展示。 */
function formatBig(value: string): string {
  const num = Number(value);
  return Number.isFinite(num) ? formatInt(num) : value;
}

/** 「M 条」量级展示（含 1s 抖动值）。 */
function formatMillion(perSec: number): string {
  const perDay = perSec * 3600;
  return `${(perDay / 1_000_000).toFixed(2)}`;
}

/** 跳转（路由 name）。 */
function go(name: string): void {
  void router.push({ name });
}
</script>

<style scoped>
/* 采集吞吐图表：随容器拉伸，最小高度保证 1366×768 下仍可读 */
.ov-chart {
  width: 100%;
  height: 190px;
  display: block;
}
.ov-legend {
  display: flex;
  align-items: center;
  gap: 16px;
  font-size: var(--fs-caption);
  color: var(--text-3);
}
.ov-legend i {
  display: inline-block;
  width: 10px;
  height: 3px;
  border-radius: 2px;
  margin-right: 6px;
  vertical-align: middle;
}
.ov-meter {
  display: flex;
  flex-direction: column;
  gap: 6px;
}
.ov-meter__head {
  display: flex;
  justify-content: space-between;
  font-size: var(--fs-caption);
  color: var(--text-3);
}
.ov-foot-note {
  margin: 0;
  padding: 10px 12px;
  font-size: var(--fs-caption);
  color: var(--text-3);
  border-top: 1px solid var(--divider);
}
.ov-dot {
  display: inline-block;
  width: 7px;
  height: 7px;
  border-radius: 50%;
  margin-right: 6px;
  vertical-align: middle;
}
.ov-dot--ok {
  background: var(--ok);
}
.ov-dot--warn {
  background: var(--warn);
}
.ov-dot--danger {
  background: var(--danger);
}
</style>
