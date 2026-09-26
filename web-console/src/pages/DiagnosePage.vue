<template>
  <!--
    DiagnosePage —— 诊断与自检（运维分组，路由 `/diagnose`）。

    真实能力边界（不许造数）：
      · 后端仅提供 `GET /api/health`（网关健康检查），**没有自检清单 / 诊断包端点**；
      · 因此本页只跑这一项真实检查，结果如实上屏；缺失的能力诚实留空（`—`）并给一句原因。
  -->
  <div class="wc-content">
    <!-- 工具条：自检数据源 + 重新自检 / 导出诊断包 -->
    <div class="pg-toolbar">
      <span class="wc-tag wc-tag--ok">实时数据</span>
      <span class="wc-spacer" />
      <button type="button" class="wc-btn wc-btn--primary wc-btn--sm" data-testid="diagnose-rerun" @click="rerun">
        重新自检
      </button>
      <button type="button" class="wc-btn wc-btn--sm" data-testid="diagnose-export" @click="exportPack">导出诊断包</button>
    </div>

    <p v-if="lastRunText" class="wc-hint" data-testid="diagnose-lastrun">{{ lastRunText }}</p>

    <!-- KPI：仅「网关健康检查」为真实项，其余显式留空 -->
    <div class="wc-grid wc-grid--4">
      <div class="wc-kpi">
        <span class="wc-kpi__label">自检项</span>
        <span class="wc-kpi__value">{{ checks.length }}</span>
        <span class="wc-kpi__sub">仅网关健康检查</span>
      </div>
      <div class="wc-kpi">
        <span class="wc-kpi__label">通过</span>
        <span class="wc-kpi__value" data-testid="kpi-pass">{{ passedCount }}</span>
        <span class="wc-kpi__sub wc-kpi__sub--ok">{{ passRateText }}</span>
      </div>
      <div class="wc-kpi">
        <span class="wc-kpi__label">警告</span>
        <span class="wc-kpi__value" data-testid="kpi-warn">{{ warnCount }}</span>
        <span class="wc-kpi__sub">—</span>
      </div>
      <div class="wc-kpi">
        <span class="wc-kpi__label">错误</span>
        <span class="wc-kpi__value" data-testid="kpi-error">{{ errorCount }}</span>
        <span class="wc-kpi__sub">—</span>
      </div>
    </div>

    <!-- 自检结果 -->
    <section class="wc-card">
      <div class="wc-card__head">
        <h3>自检结果</h3>
        <span class="wc-card__sub">GET /api/health</span>
      </div>

      <EmptyState
        v-if="checks.length === 0"
        title="尚未自检"
        desc="点「重新自检」执行网关健康检查。"
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
        row-key-field="name"
        footer="后端未提供自检清单接口：除网关健康检查外的检查项无法上报，不做显示。"
      >
        <template #cell-category="{ row }">
          <span class="wc-tag wc-tag--unknown">{{ row.category }}</span>
        </template>
        <template #cell-result="{ row }">
          <StatusTag :status="row.result" :text="resultLabel(row.result)" />
        </template>
        <template #cell-detail="{ row }">
          <span class="wc-mono">{{ row.detail }}</span>
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
 * @description 诊断与自检：仅驱动真实 `GET /api/health`，其余能力诚实留空。
 */
import { computed, onMounted, ref } from 'vue';
import { EmptyState, StatusTag, UiTable, UiPager, type TableColumn } from '@ui-kit';
import { repo } from '@/api/repo';

/** 自检项。 */
interface CheckItem {
  /** 检查项名称 */
  name: string;
  /** 分类 */
  category: string;
  /** 结果（pass / warn / error） */
  result: string;
  /** 详情（后端原文） */
  detail: string;
}

/** 自检结果（来自真实健康检查；未执行时为空）。 */
const checks = ref<CheckItem[]>([]);

/**
 * 自检结果分页（切片留在页面级 computed；UiTable 纯展示，不在组件内做局部 slice）。
 * 当前仅网关健康检查一项，机制仍按真实分页接好——若后端未来补全多检查项，换页即生效。
 */
const CHECK_PAGE_SIZE = 5;
const checkPage = ref(1);

/** 当前页自检项（由 `checkPage` 驱动的真实切片）。 */
const pagedChecks = computed<CheckItem[]>(() => {
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

/** 重新自检：真实调用 `GET /api/health`。 */
async function rerun(): Promise<void> {
  const result = await repo.ops.health();
  checks.value = [
    {
      name: '网关健康检查',
      category: '网关',
      result: result.ok ? 'pass' : 'error',
      detail: result.message,
    },
  ];
  lastRunText.value = `自检完成（${nowText()}）：网关健康检查${result.ok ? '通过' : '未通过'}。`;
}

/** 导出诊断包：后端无对应端点，如实告知。 */
function exportPack(): void {
  lastRunText.value = `导出未执行（${nowText()}）：网关未提供诊断包导出接口。`;
}

/** 时间短文本。 */
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
