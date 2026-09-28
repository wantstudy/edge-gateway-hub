<template>
  <!--
    DiagnosePage —— 诊断与自检（运维分组，路由 `/diagnose`）。

    真实能力边界（不许造数）：
      · 自检清单走 `GET /api/diagnostics/selfcheck`（后端已落地：config_writable / scheduler /
        alarm_engine / license / audit_logger / machine_code / clock 七项），结果如实上屏并分页；
      · ⚠️ `ok` 只代表「该检查项可跑通」，**不等于业务就绪** —— 例如授权未激活时
        `license.ok=true` 而 `detail.north_forward_allowed=false`，本页按后者降级为「警告」并渲染详情；
      · 诊断包导出后端**仍未提供端点**，点击如实告知，绝不伪造下载。
  -->
  <div class="wc-content">
    <!-- 工具条：自检数据源 + 重新自检 / 导出诊断包 -->
    <div class="pg-toolbar">
      <span class="wc-tag wc-tag--ok">实时数据</span>
      <span class="wc-spacer" />
      <button
        type="button"
        class="wc-btn wc-btn--primary wc-btn--sm"
        data-testid="diagnose-rerun"
        :disabled="running"
        @click="rerun"
      >
        {{ running ? '自检中…' : '重新自检' }}
      </button>
      <button type="button" class="wc-btn wc-btn--sm" data-testid="diagnose-export" @click="exportPack">导出诊断包</button>
    </div>

    <p v-if="lastRunText" class="wc-hint" data-testid="diagnose-lastrun">{{ lastRunText }}</p>

    <!-- KPI：四项均来自真实自检清单 -->
    <div class="wc-grid wc-grid--4">
      <div class="wc-kpi">
        <span class="wc-kpi__label">自检项</span>
        <span class="wc-kpi__value" data-testid="kpi-total">{{ checks.length }}</span>
        <span class="wc-kpi__sub">网关实时上报</span>
      </div>
      <div class="wc-kpi">
        <span class="wc-kpi__label">通过</span>
        <span class="wc-kpi__value" data-testid="kpi-pass">{{ passedCount }}</span>
        <span class="wc-kpi__sub wc-kpi__sub--ok">{{ passRateText }}</span>
      </div>
      <div class="wc-kpi">
        <span class="wc-kpi__label">警告</span>
        <span class="wc-kpi__value" data-testid="kpi-warn">{{ warnCount }}</span>
        <span class="wc-kpi__sub">检查项跑通但业务未就绪</span>
      </div>
      <div class="wc-kpi">
        <span class="wc-kpi__label">错误</span>
        <span class="wc-kpi__value" data-testid="kpi-error">{{ errorCount }}</span>
        <span class="wc-kpi__sub">检查项执行失败</span>
      </div>
    </div>

    <!-- 自检结果 -->
    <section class="wc-card">
      <div class="wc-card__head">
        <h3>自检结果</h3>
        <span class="wc-card__sub">
          网关实时自检<span v-if="checkedAtText"> · 自检时刻 {{ checkedAtText }}</span>
        </span>
      </div>

      <EmptyState
        v-if="checks.length === 0"
        :title="loadError ? '自检未拿到结果' : '尚未自检'"
        :desc="loadError || '点「重新自检」拉取网关自检清单。'"
      >
        <template #actions>
          <button type="button" class="wc-btn wc-btn--primary" data-testid="diagnose-rerun-empty" @click="rerun">
            重新自检
          </button>
        </template>
      </EmptyState>

      <UiTable
        v-else
        :columns="columns"
        :rows="pagedChecks"
        row-key-field="key"
        footer="结果由网关实时自检上报；是否就绪以详情列为准。"
      >
        <template #cell-category="{ row }">
          <span class="wc-tag wc-tag--unknown">{{ row.category }}</span>
        </template>
        <template #cell-result="{ row }">
          <StatusTag :status="row.result" :text="resultLabel(row.result)" />
        </template>
        <template #cell-detail="{ row }">
          <span class="wc-mono" :title="row.detail">{{ clip(row.detail) }}</span>
        </template>
      </UiTable>

      <UiPager
        v-if="checks.length > CHECK_PAGE_SIZE"
        :page="checkPage"
        :total="checks.length"
        :page-size="CHECK_PAGE_SIZE"
        @update:page="onCheckPage"
      />
    </section>

    <!-- 诊断包内容（只读说明；无导出端点） -->
    <section class="wc-card">
      <div class="wc-card__head">
        <h3>诊断包内容</h3>
        <span class="wc-tag wc-tag--warn">不含私钥 / 激活码原文 / 业务数据值</span>
      </div>
      <div class="wc-card__body">
        <dl class="wc-kv">
          <dt>配置快照</dt>
          <dd class="wc-mono">已脱敏（去掉密钥与口令）</dd>
          <dt>队列与数据库状态</dt>
          <dd class="wc-mono">仅表结构与计数</dd>
          <dt>近 1 小时日志</dt>
          <dd class="wc-mono">tracing 结构化日志</dd>
          <dt>驱动统计</dt>
          <dd class="wc-mono">重连次数 / 耗时分布</dd>
          <dt>授权状态</dt>
          <dd class="wc-mono">机器码哈希 + 租约状态</dd>
        </dl>
      </div>
    </section>
  </div>
</template>

<script setup lang="ts">
/**
 * @file DiagnosePage.vue
 * @module web-console/pages/DiagnosePage
 * @description 诊断与自检：驱动真实 `GET /api/diagnostics/selfcheck`（7 项），结果分页上屏；
 *   接口失败时呈现真实原因（诚实降级，绝不退化成「尚未自检」的假空态）；
 *   诊断包导出后端无端点，如实告知。
 */
import { computed, onMounted, ref } from 'vue';
import { EmptyState, StatusTag, UiTable, UiPager, type TableColumn } from '@ui-kit';
import { repo, type SelfCheckItem } from '@/api/repo';
import { formatTimestampText } from '@/utils/time';

/** 自检项（后端原始项 → 表格行）。 */
interface CheckRow {
  /** 检查项名称（中文，未知名回落到后端原文） */
  name: string;
  /** 后端原始标识（用于 `row-key-field` 唯一性与排障） */
  key: string;
  /** 分类 */
  category: string;
  /** 结果（pass / warn / error） */
  result: 'pass' | 'warn' | 'error';
  /** 详情（后端 detail 的 kv 串，原样可读化） */
  detail: string;
}

/** 后端检查项 → 中文名。 */
const NAME_MAP: Readonly<Record<string, string>> = {
  config_writable: '配置可写',
  scheduler: '采集调度',
  alarm_engine: '告警引擎',
  license: '授权状态',
  audit_logger: '审计日志',
  machine_code: '机器码',
  clock: '系统时钟',
};

/** 后端检查项 → 分类。 */
const CATEGORY_MAP: Readonly<Record<string, string>> = {
  config_writable: '配置',
  scheduler: '采集',
  alarm_engine: '告警',
  license: '授权',
  audit_logger: '审计',
  machine_code: '授权',
  clock: '系统',
};

/**
 * `detail` 中代表「业务未就绪」的键：值为 false 即降级为警告。
 * 只列后端实际会返回的键，不做模糊匹配（避免脆弱的字符串包含判断）。
 */
const READINESS_FALSE_KEYS: readonly string[] = [
  'north_forward_allowed',
  'running',
  'mounted',
  'probe',
  'config_path_bound',
];

/** `detail` 中出现即代表存在故障描述的键后缀。 */
const ERROR_KEY_SUFFIX = '_error';

/** 自检结果（来自真实自检清单；未执行 / 失败时为空）。 */
const checks = ref<CheckRow[]>([]);

/** 是否正在自检。 */
const running = ref(false);

/** 自检失败原因（诚实降级：非空即展示真实原因，绝不冒充「尚未自检」）。 */
const loadError = ref('');

/** 后端自检时刻的可读展示文本；空值 / 非法时间统一为 `—`。 */
const checkedAtText = ref('');

/**
 * 自检结果分页（切片留在页面级 computed；UiTable 纯展示，不在组件内做局部 slice）。
 */
const CHECK_PAGE_SIZE = 5;
const checkPage = ref(1);

/** 当前页自检项（由 `checkPage` 驱动的真实切片）。 */
const pagedChecks = computed<CheckRow[]>(() => {
  const start = (checkPage.value - 1) * CHECK_PAGE_SIZE;
  return checks.value.slice(start, start + CHECK_PAGE_SIZE);
});

/** 换页（由 UiPager 驱动）。 */
function onCheckPage(next: number): void {
  checkPage.value = next;
}

/** 通过数。 */
const passedCount = computed<number>(() => checks.value.filter((c) => c.result === 'pass').length);

/** 警告数。 */
const warnCount = computed<number>(() => checks.value.filter((c) => c.result === 'warn').length);

/** 错误数。 */
const errorCount = computed<number>(() => checks.value.filter((c) => c.result === 'error').length);

/** 通过率文本。 */
const passRateText = computed<string>(() => {
  const total = checks.value.length;
  if (total === 0) {
    return '—';
  }
  return `${((passedCount.value / total) * 100).toFixed(1)}%`;
});

/** 结果文案。 */
function resultLabel(result: string): string {
  return result === 'pass' ? '通过' : result === 'warn' ? '警告' : '错误';
}

/** 列定义。 */
const columns: readonly TableColumn[] = [
  { key: 'name', label: '检查项' },
  { key: 'category', label: '分类' },
  { key: 'result', label: '结果' },
  { key: 'detail', label: '详情', mono: true },
];

/** 最近一次自检结果文案。 */
const lastRunText = ref('');

/**
 * 判定结果：`ok=false` → 错误；`ok=true` 但 detail 里出现未就绪标记或故障描述 → 警告；否则通过。
 *
 * ⚠️ 这是有意为之的语义分层：后端 `ok` 只说明「检查项自身跑通了」，
 * 直接把它当成「业务正常」会把「授权未激活、北向关闭」渲染成绿色的「通过」。
 */
function deriveResult(item: SelfCheckItem): 'pass' | 'warn' | 'error' {
  if (!item.ok) {
    return 'error';
  }
  const detail = item.detail ?? {};
  if (READINESS_FALSE_KEYS.some((k) => detail[k] === false)) {
    return 'warn';
  }
  if (Object.keys(detail).some((k) => k.endsWith(ERROR_KEY_SUFFIX) && typeof detail[k] === 'string')) {
    return 'warn';
  }
  return 'pass';
}

/** detail 中可判定为时间戳的精确键名。 */
const DETAIL_TIMESTAMP_KEYS: ReadonlySet<string> = new Set(['now_ms', 'time', 'timestamp', 'ts']);

/** detail 中可判定为时间戳的明确后缀；禁止用 `includes('time')` 模糊匹配。 */
const DETAIL_TIMESTAMP_SUFFIXES: readonly string[] = ['_ms', '_at', '_ts', '_time', '_timestamp'];

/** 判断 detail 键是否承载时间戳。 */
function isDetailTimestampKey(key: string): boolean {
  const normalized = key.trim().toLowerCase();
  return DETAIL_TIMESTAMP_KEYS.has(normalized) || DETAIL_TIMESTAMP_SUFFIXES.some((suffix) => normalized.endsWith(suffix));
}

/** detail 单值 → 可读文本；嵌套对象也按明确时间键格式化。 */
function formatDetailValue(key: string, value: unknown): string {
  if (isDetailTimestampKey(key)) {
    return formatTimestampText(typeof value === 'string' || typeof value === 'number' ? value : null);
  }
  if (value === null) {
    return 'null';
  }
  if (typeof value === 'object') {
    return JSON.stringify(value, (nestedKey, nestedValue) => {
      if (!nestedKey || !isDetailTimestampKey(nestedKey)) {
        return nestedValue;
      }
      return formatTimestampText(
        typeof nestedValue === 'string' || typeof nestedValue === 'number' ? nestedValue : null,
      );
    });
  }
  return String(value);
}

/** detail 对象 → 可读 kv 串；时间键统一格式化，其余值保持原有展示语义。 */
function formatDetail(detail: Record<string, unknown> | null): string {
  if (!detail) {
    return '—';
  }
  const entries = Object.entries(detail);
  if (entries.length === 0) {
    return '—';
  }
  return entries.map(([key, value]) => `${key}=${formatDetailValue(key, value)}`).join(' · ');
}

/** 表格内详情超长截断（全文挂 `title`，信息不丢）。 */
function clip(text: string): string {
  return text.length > 120 ? `${text.slice(0, 117)}…` : text;
}

/** 后端原始项 → 表格行。 */
function toRow(item: SelfCheckItem): CheckRow {
  return {
    name: NAME_MAP[item.name] ?? item.name,
    key: item.name,
    category: CATEGORY_MAP[item.name] ?? '其他',
    result: deriveResult(item),
    detail: formatDetail(item.detail),
  };
}

/** 最近一次自检：真实调用 `GET /api/diagnostics/selfcheck`。 */
async function rerun(): Promise<void> {
  running.value = true;
  loadError.value = '';
  try {
    const report = await repo.ops.selfCheck();
    checks.value = report.checks.map(toRow);
    checkedAtText.value = formatTimestampText(report.checkedAt);
    checkPage.value = 1;
    lastRunText.value =
      `自检完成（本地 ${nowText()}）：共 ${report.checks.length} 项，` +
      `通过 ${passedCount.value} / 警告 ${warnCount.value} / 错误 ${errorCount.value}` +
      `，网关自检时刻 ${checkedAtText.value}。`;
  } catch (cause) {
    // 诚实降级：失败就是失败，清空清单 + 展示真实原因，绝不退化成「尚未自检」的假空态。
    checks.value = [];
    checkedAtText.value = '';
    loadError.value = cause instanceof Error ? cause.message : String(cause);
    lastRunText.value = `自检失败（${nowText()}）：${loadError.value}`;
  } finally {
    running.value = false;
  }
}

/** 导出诊断包：后端无对应端点，如实告知。 */
function exportPack(): void {
  lastRunText.value = `诊断包导出暂不可用（${nowText()}），请稍后再试。`;
}

/** 时间短文本（本地时钟，与后端 `checkedAt` 无关）。 */
function nowText(): string {
  const d = new Date();
  const p = (n: number): string => String(n).padStart(2, '0');
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

onMounted(() => {
  void rerun();
});
</script>

<style scoped>
.pg-toolbar {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: 8px;
  min-height: 28px;
}
</style>
