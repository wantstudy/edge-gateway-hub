<template>
  <!--
    OverviewPage —— 总览 / 仪表盘（路由 `/overview`）。

    结构（无页头块：顶部只有一条工具条）：
      ① 工具条：数据源指示 + 真实降级状态标签（chip）+ 立即刷新
      ② KPI 卡片行（在线设备 / 采集点位 / 当前上行 / 端到端延迟）
      ③ 2:1 栅格：采集吞吐（历史序列不可得 → 诚实空态）+ 系统状态（KV）
      ④ 3 列栅格：设备健康 / 北向出口 / 待处理
      ⑤ 最近告警列表（分页，UiPager 单一口径）

    硬性约定遵守情况：
      · 只用 ui-kit 组件（`StatCard` / `UiTable` / `UiPager` / `StatusTag` / `EmptyState`）；
      · **无 mock**：所有数值来自 `GET /api/overview`（5s 低频轮询）与各真实清单接口，
        后端未提供的指标一律诚实留空（`—`），绝不回退演示数据；
      · 降级 / 异常以**紧凑状态标签**呈现（点击进对应页），不占整行说明块；
      · 列表条数只有 UiPager 一个口径（表格 `footer` 不重复写「共 N 条」）。
  -->
  <div class="wc-content">
    <!-- ① 工具条：数据源指示 + 降级状态标签 + 立即刷新（原页头按钮迁入，功能不丢） -->
    <div class="ov-toolbar">
      <span class="wc-tag wc-tag--ok">实时数据</span>

      <span
        v-for="chip in statusChips"
        :key="chip.label"
        class="wc-tag ov-chip"
        :class="`wc-tag--${chip.tone}`"
        :title="chip.reason"
        role="link"
        tabindex="0"
        @click="go(chip.page)"
        @keydown.enter="go(chip.page)"
      >
        <span class="ov-chip__dot" aria-hidden="true">●</span>{{ chip.label }}
      </span>

      <span class="wc-spacer" />
      <button type="button" class="wc-btn wc-btn--sm" @click="refreshAll">立即刷新</button>
    </div>

    <!-- ② KPI 卡片行（数值来自 GET /api/overview；无数据源者诚实留空） -->
    <div class="wc-grid wc-grid--4">
      <StatCard
        label="设备在线"
        :value="`${gateway.onlineCount}`"
        :unit="`/ ${gateway.deviceCount}`"
        :sub="onlineSub"
        :tone="gateway.deviceCount > 0 && gateway.onlineCount === gateway.deviceCount ? 'ok' : 'warn'"
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
        label="当前上行"
        :value="formatInt(gateway.forwardRatePerSec)"
        unit="条/秒"
        :sub="`累计 ${gateway.totalForwardedRecords} 条`"
      />
      <StatCard label="端到端延迟" value="—" sub="网关未提供该指标" />
    </div>

    <!-- ③ 采集吞吐 + 系统状态 -->
    <div class="wc-grid wc-grid--2-1">
      <section class="wc-card">
        <div class="wc-card__head">
          <h3>采集吞吐</h3>
        </div>
        <div class="wc-card__body">
          <EmptyState
            title="暂无吞吐历史数据"
            desc="网关未提供吞吐历史时间序列（/api/overview 仅返回实时速率）。"
          >
            <template #actions>
              <button type="button" class="wc-btn wc-btn--sm" @click="go('live')">前往实时数据</button>
            </template>
          </EmptyState>
        </div>
      </section>

      <section class="wc-card">
        <div class="wc-card__head">
          <h3>系统状态</h3>
        </div>
        <div class="wc-card__body">
          <dl class="wc-kv">
            <dt>采集速率</dt>
            <dd class="wc-mono">{{ formatInt(gateway.sampleRatePerSec) }} 点/秒</dd>
            <dt>CPU 占用</dt>
            <dd class="wc-mono">—</dd>
            <dt>内存占用</dt>
            <dd class="wc-mono">—</dd>
            <dt>数据目录占用</dt>
            <dd class="wc-mono">{{ hasQueueCapacity ? `${gateway.queueUsedGb} GB / ${gateway.queueCapacityGb} GB` : '—' }}</dd>
            <dt>连续运行</dt>
            <dd class="wc-mono">{{ gateway.uptimeText }}</dd>
            <dt>版本</dt>
            <dd class="wc-mono">{{ gateway.version }}</dd>
          </dl>
          <div v-if="hasQueueCapacity" class="ov-meter">
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
        </div>
        <div class="wc-card__body wc-card__body--flush">
          <EmptyState
            v-if="healthRows.length === 0"
            title="暂无设备"
            :desc="notices.devices || '网关未返回任何设备（GET /api/devices 为空）。'"
          >
            <template #actions>
              <button type="button" class="wc-btn wc-btn--sm" @click="go('device-new')">新增设备</button>
            </template>
          </EmptyState>
          <UiTable v-else :columns="healthColumns" :rows="healthRows">
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
        </div>
        <div class="wc-card__body wc-card__body--flush">
          <EmptyState
            v-if="forwarders.length === 0"
            title="暂无北向出口"
            :desc="notices.forwarders || '网关未返回任何北向出口（GET /api/forwarders 为空）。'"
          >
            <template #actions>
              <button type="button" class="wc-btn wc-btn--sm" @click="go('northbound')">配置北向转发</button>
            </template>
          </EmptyState>
          <UiTable v-else :columns="forwarderColumns" :rows="forwarders">
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
        </div>
      </section>

      <section class="wc-card">
        <div class="wc-card__head">
          <h3>待处理</h3>
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
        <span class="wc-card__sub">5 分钟自动刷新</span>
        <div class="wc-card__ops">
          <button type="button" class="wc-btn wc-btn--sm" @click="go('alarms')">前往告警中心</button>
        </div>
      </div>

      <EmptyState
        v-if="paged.total === 0"
        title="暂无告警"
        :desc="notices.alerts || '当前没有告警记录。'"
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
        <UiPager
          :page="paged.page"
          :total="paged.total"
          :page-size="ALARM_PAGE_SIZE"
          numeric
          jump
          @update:page="onAlarmPage"
        />
      </template>
    </section>
  </div>
</template>

<script setup lang="ts">
/**
 * @file OverviewPage.vue
 * @module web-console/pages/OverviewPage
 * @description 总览页。全部数值来自真实接口（`/api/overview` + 各清单接口），无 mock。
 *
 * ── 节流契约 ────────────────────────────────────────────────────────────────
 * 真实遥测会以远高于 1s 的频率推送。这里用一个 **1s 的 `setInterval`** 作为
 * 「节流后的渲染节拍」：无论上游多快，Vue 的响应式更新每秒至多一次，
 * 保证 200 设备 × 100ms 场景下浏览器不被重绘打满（设计系统 §4.2 硬约束）。
 * 累计计数器（`totalForwardedRecords` 等）不在遥测流内，按 5s 低频轮询
 * `/api/overview` 刷新。
 */
import { computed, onBeforeUnmount, onMounted, reactive, ref, watch } from 'vue';
import { useRouter } from 'vue-router';
import {
  EmptyState,
  StatCard,
  StatusTag,
  UiPager,
  UiTable,
  type TableColumn,
} from '@ui-kit';
import {
  dataVersion,
  repo,
  refreshOverview,
  type AlarmLevel,
  type AlarmRecord,
  type AlarmState,
  type DeviceRecord,
  type ForwarderRecord,
  type GatewayInfo,
  type NoticeKey,
} from '@/api/repo';
import { session } from '../store/session';

const router = useRouter();

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/** 最近告警每页条数。 */
const ALARM_PAGE_SIZE = 5;

/** `/api/overview` 轮询间隔（节拍数）：5s 一次，计数器 / 水位类统计低频即可。 */
const OVERVIEW_POLL_EVERY_TICKS = 5;

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
// 真实快照（`reactive` 数组：real 缓存晚于挂载填充时也能驱动重算）
// ---------------------------------------------------------------------------

/** 本机网关信息（`GET /api/overview` 镜像；随低频轮询刷新）。 */
const gateway = reactive<GatewayInfo>(repo.getGateway());

/** 从 repo 缓存同步网关信息（字段级覆盖保持响应式引用稳定）。 */
function syncGateway(): void {
  Object.assign(gateway, repo.getGateway());
}

/** 全量设备（真实缓存；用于 KPI 与设备健康排序）。 */
const devices = reactive<DeviceRecord[]>(repo.allDevices());

/** 全量北向出口。 */
const forwarders = reactive<ForwarderRecord[]>(repo.allForwarders());

/** 全部告警（页面内分页）。 */
const allAlarms = reactive<AlarmRecord[]>(repo.allAlarms());

/** 各数据源的「不可得原因」（诚实空态文案来源）。 */
const notices = ref<Record<NoticeKey, string>>(repo.actions.notices());

/** 把 source 内容覆盖写入 target（原地变更，保持 target 引用 / 响应式不变）。 */
function applyInto<T>(target: T[], source: readonly T[]): void {
  target.length = 0;
  target.push(...source);
}

/**
 * 缓存填充完成（dataVersion 自增）→ 原地刷新全部快照。
 *
 * real 模式下 preload 在后台跑、可能晚于本页挂载：此处由响应式驱动刷新，
 * 保证「设备 / 出口 / 告警 / 网关信息」在缓存就绪后立即上屏（无 mock 回退）。
 */
watch(dataVersion, () => {
  applyInto(devices, repo.allDevices());
  applyInto(forwarders, repo.allForwarders());
  applyInto(allAlarms, repo.allAlarms());
  syncGateway();
  notices.value = repo.actions.notices();
});

// ---------------------------------------------------------------------------
// 1s 节流节拍（渲染节拍 + 低频轮询）
// ---------------------------------------------------------------------------

/** 上次节拍时间（用于降级状态标签里的「最后更新」）。 */
const lastTickAt = ref<number>(Date.now());

/** 节流定时器句柄（组件卸载必须清理）。 */
let timer: ReturnType<typeof setInterval> | null = null;

/** 节拍计数（用于 5 拍一次的低频轮询 `/api/overview`）。 */
let tickCount = 0;

/**
 * 单个节拍：上游再怎么高频，组件每秒只重渲染一次。
 *
 * 每 5 拍触发一次 `/api/overview` 低频刷新（累计计数器 / 磁盘水位），
 * 然后从 repo 缓存同步网关信息；本机指标（CPU / 内存 / 延迟）后端未提供，
 * 一律不渲染数值（页面留 `—`），绝不伪造。
 */
function tick(): void {
  if (tickCount % OVERVIEW_POLL_EVERY_TICKS === 0) {
    void refreshOverview();
  }
  tickCount += 1;
  syncGateway();
  lastTickAt.value = Date.now();
}

/** 手动刷新（工具条按钮）：立即拉取一次 `/api/overview` 并同步。 */
function refreshAll(): void {
  void refreshOverview().then(() => {
    syncGateway();
    lastTickAt.value = Date.now();
  });
}

onMounted(() => {
  notices.value = repo.actions.notices();
  tick();
  timer = setInterval(tick, 1000);
});

onBeforeUnmount(() => {
  if (timer !== null) {
    clearInterval(timer);
    timer = null;
  }
});

/** 最后更新时刻（`HH:mm:ss`，供降级状态标签给出真实依据）。 */
const lastTickText = computed<string>(() => {
  const d = new Date(lastTickAt.value);
  const p = (n: number): string => String(n).padStart(2, '0');
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
});

// ---------------------------------------------------------------------------
// 降级 / 异常状态标签（紧凑 chip；点击进对应处理页）
// ---------------------------------------------------------------------------

/** 状态标签视图模型。 */
interface StatusChip {
  /** 色调 */
  readonly tone: 'warn' | 'danger' | 'info';
  /** 短标签（chip 文案） */
  readonly label: string;
  /** 真实原因（悬浮说明，含最后更新 / 恢复路径） */
  readonly reason: string;
  /** 点击目标路由 name */
  readonly page: string;
}

/** 磁盘队列水位百分比（容量缺失时按 0 处理，避免 NaN）。 */
const queuePct = computed<number>(() => {
  if (gateway.queueCapacityGb <= 0) {
    return 0;
  }
  return Math.min(100, Math.round((gateway.queueUsedGb / gateway.queueCapacityGb) * 100));
});

/** 是否具备磁盘队列容量口径（决定水位条与占用行是否可展示）。 */
const hasQueueCapacity = computed<boolean>(() => gateway.queueCapacityGb > 0);

/**
 * 当前应呈现的状态标签（可多条）。
 *
 * 与旧「整块横幅」的差异：结论压成一行 chip，点击进对应页；真实原因放在
 * chip 的 `title` 上，不占页面纵向空间。无异常时返回空数组（不占位）。
 */
const statusChips = computed<readonly StatusChip[]>(() => {
  const chips: StatusChip[] = [];
  const lic = session.state.license;

  // 实时通道降级 / 断开
  if (session.state.connection !== 'connected') {
    const disconnected = session.state.connection === 'disconnected';
    chips.push({
      tone: disconnected ? 'danger' : 'warn',
      label: disconnected ? '实时通道已断开' : '链路降级',
      reason: `${disconnected ? '实时通道已断开' : '实时通道链路降级'}，最后更新 ${lastTickText.value}；本地采集继续运行。点击进入诊断。`,
      page: 'diagnose',
    });
  }

  // 授权已停用（心跳超期 / 被后台废弃）
  if (lic.status === 'stopped' || lic.status === 'grace') {
    chips.push({
      tone: 'danger',
      label: '授权已停用',
      reason: lic.degradeReason || '云端心跳超期，北向转发已停用；本地采集继续。点击前往授权与激活。',
      page: 'license',
    });
  } else if (lic.status === 'trial' && lic.remainingDays <= 2) {
    chips.push({
      tone: 'warn',
      label: `试用剩余 ${lic.remainingText}`,
      reason: `试用到期后${lic.onExpireText}。点击输入激活码。`,
      page: 'license',
    });
  }

  // 磁盘队列高位（≥ 60%）
  if (hasQueueCapacity.value && queuePct.value >= 60) {
    chips.push({
      tone: 'warn',
      label: `磁盘队列 ${queuePct.value}%`,
      reason: `磁盘队列 ${gateway.queueUsedGb} GB / ${gateway.queueCapacityGb} GB，按当前速率可续传 ≈${gateway.queueDrainDays} 天。点击查看北向转发。`,
      page: 'northbound',
    });
  }

  return chips;
});

// ---------------------------------------------------------------------------
// KPI 派生
// ---------------------------------------------------------------------------

/** 在线设备副标题（诚实：无设备时直说，不粉饰）。 */
const onlineSub = computed<string>(() => {
  if (gateway.deviceCount === 0) {
    return '未接入设备';
  }
  const offline = gateway.deviceCount - gateway.onlineCount;
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
// 待处理清单（来自真实快照推导，非硬编码）
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

/** 待处理清单（由真实快照实时推导）。 */
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

/** 跳转（路由 name）。 */
function go(name: string): void {
  void router.push({ name });
}
</script>

<style scoped>
/* 页级工具条：无边框 / 无底色的一行，避免重新引入「顶部内容框」 */
.ov-toolbar {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: 8px;
  min-height: 28px;
}
/* 状态标签 chip（紧凑；可点击进对应页） */
.ov-chip {
  cursor: pointer;
}
.ov-chip:hover {
  border-color: var(--brand);
}
.ov-chip__dot {
  font-size: 9px;
  line-height: 1;
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
