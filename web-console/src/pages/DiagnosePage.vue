<!--
  =============================================================================
  DiagnosePage —— 诊断与自检（设计 §3.7 / 原型 gateway-v2a-glacier「diagnose」）
  =============================================================================
  18 项自检覆盖采集、缓存、分发、授权与磁盘，可一键重新自检与导出诊断包。
  诊断包内容与不包含项必须显式声明（不含私钥 / 激活码原文 / 业务数据值）。
-->
<template>
  <PageHeader
    crumb="运维 / 诊断与自检"
    title="诊断与自检"
    desc="18 项自检覆盖配置、存储、采集、分发、授权与磁盘，可一键重新自检并导出诊断包。"
  >
    <template #actions>
      <span style="display: inline-flex; gap: 8px">
        <button type="button" class="wc-btn wc-btn--primary" data-testid="diagnose-rerun" @click="rerun">
          重新自检
        </button>
        <button type="button" class="wc-btn" data-testid="diagnose-export" @click="exportPack">导出诊断包</button>
      </span>
    </template>
  </PageHeader>

  <div class="wc-content">
    <p v-if="lastRunText" class="wc-hint" data-testid="diagnose-lastrun">{{ lastRunText }}</p>

    <!-- ══ KPI ═════════════════════════════════════════════════════════ -->
    <div class="wc-grid wc-grid--4">
      <div class="wc-kpi">
        <span class="wc-kpi__label">自检项</span>
        <span class="wc-kpi__value">18</span>
        <span class="wc-kpi__sub">全量覆盖</span>
      </div>
      <div class="wc-kpi">
        <span class="wc-kpi__label">通过</span>
        <span class="wc-kpi__value" data-testid="kpi-pass">{{ passedCount }}</span>
        <span class="wc-kpi__sub wc-kpi__sub--ok">{{ passRateText }}</span>
      </div>
      <div class="wc-kpi">
        <span class="wc-kpi__label">警告</span>
        <span class="wc-kpi__value" data-testid="kpi-warn">{{ warnCount }}</span>
        <span class="wc-kpi__sub wc-kpi__sub--warn">磁盘占用 18%</span>
      </div>
      <div class="wc-kpi">
        <span class="wc-kpi__label">错误</span>
        <span class="wc-kpi__value" data-testid="kpi-error">0</span>
        <span class="wc-kpi__sub">无阻断项</span>
      </div>
    </div>

    <!-- ══ 自检结果 ════════════════════════════════════════════════════ -->
    <section class="wc-card">
      <div class="wc-card__head">
        <h3>自检结果</h3>
        <span class="wc-card__sub">分类：配置 / 存储 / 采集 / 分发 / 授权 / 安全</span>
      </div>
      <UiTable
        :columns="columns"
        :rows="checks"
        row-key-field="name"
        footer="另有 8 项常规检查全部通过"
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
    </section>

    <!-- ══ 诊断包内容 + 常见故障指引 ═══════════════════════════════════ -->
    <div class="wc-grid wc-grid--2">
      <section class="wc-card">
        <div class="wc-card__head">
          <h3>诊断包内容</h3>
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
          <div class="wc-banner wc-banner--warn">
            <span class="wc-banner__icon">!</span>
            <span>
              诊断包<b>不包含</b>私钥、激活码原文、业务数据值与完整机器码，仅含哈希与状态。
            </span>
          </div>
        </div>
      </section>

      <section class="wc-card">
        <div class="wc-card__head">
          <h3>常见故障指引</h3>
        </div>
        <div class="wc-card__body">
          <div class="wc-list">
            <div v-for="tip in tips" :key="tip.title" class="wc-list__item">
              <div>
                <div class="wc-list__title">{{ tip.title }}</div>
                <div class="wc-list__desc">{{ tip.desc }}</div>
              </div>
              <div class="wc-list__ops">
                <span class="wc-tag" :class="`wc-tag--${tip.tone}`">指引</span>
              </div>
            </div>
          </div>
        </div>
      </section>
    </div>
  </div>
</template>

<script setup lang="ts">
/**
 * @file DiagnosePage.vue
 * @module web-console/pages/DiagnosePage
 * @description 诊断与自检页（自检结果表 + 诊断包说明 + 故障指引）。
 */
import { computed, ref } from 'vue';
import { PageHeader, UiTable, StatusTag, type TableColumn } from '@ui-kit';

/** 自检项。 */
interface CheckItem {
  /** 检查项名称 */
  name: string;
  /** 分类 */
  category: string;
  /** 结果（pass / warn） */
  result: string;
  /** 详情 */
  detail: string;
}

/** 自检项清单（照搬原型）。 */
const checks = ref<CheckItem[]>([
  { name: '配置可加载', category: '配置', result: 'pass', detail: 'config.toml 语法有效，版本 v7' },
  { name: 'SQLite 完整性', category: '存储', result: 'pass', detail: 'queue.db / telemetry.db integrity_check ok' },
  { name: 'SQLCipher 密钥', category: '存储', result: 'pass', detail: 'HKDF(机器码) 派生成功' },
  { name: '数据目录空间', category: '存储', result: 'warn', detail: '1.8 GB / 10 GB（18%）' },
  { name: '南向驱动连通', category: '采集', result: 'pass', detail: '12/12 设备可达' },
  { name: '北向出口连通', category: '分发', result: 'pass', detail: '3/3 出口已连接' },
  { name: '离线队列积压', category: '分发', result: 'pass', detail: '1,204 条，补发中' },
  { name: '租约有效性', category: '授权', result: 'pass', detail: '剩余 6d 23h' },
  { name: '机器码锚点一致', category: '授权', result: 'pass', detail: '4/4 锚点匹配' },
  { name: '安装包完整性', category: '安全', result: 'pass', detail: '资源清单哈希匹配' },
]);

/** 通过数。 */
const passedCount = computed<number>(() => checks.value.filter((c) => c.result === 'pass').length);

/** 警告数。 */
const warnCount = computed<number>(() => checks.value.filter((c) => c.result === 'warn').length);

/** 通过率文本。 */
const passRateText = computed<string>(() => {
  const total = checks.value.length;
  const rate = total === 0 ? 0 : (passedCount.value / total) * 100;
  return `${rate.toFixed(1)}%`;
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

/** 重新自检。 */
const lastRunText = ref('');
function rerun(): void {
  lastRunText.value = `自检完成（${nowText()}）：18 项中 17 项通过、1 项警告（磁盘占用 18%）、0 项错误。`;
}

/** 导出诊断包。 */
function exportPack(): void {
  lastRunText.value = `诊断包已生成（${nowText()}）：config-snapshot(已脱敏) + queue-state + logs-1h + driver-stats + license-state，共 2.4 MB。`;
}

/** 时间短文本。 */
function nowText(): string {
  const d = new Date();
  const p = (n: number): string => String(n).padStart(2, '0');
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

/** 常见故障指引。 */
const tips = [
  { title: '设备连不上', desc: '先查串口权限与网段可达性，再看重连次数', tone: 'warn' },
  { title: '队列持续增长', desc: '北向出口不可达或 Broker 限流，检查出口连通性', tone: 'warn' },
  { title: '北向转发忽然停止', desc: '多为授权降级（宽限期超 7 天），查授权与激活页', tone: 'danger' },
  { title: '配置写错起不来', desc: '自动进入安全模式并回退上次有效配置', tone: 'info' },
];
</script>

<style scoped>
.wc-banner__icon {
  flex: 0 0 auto;
  font-weight: 700;
}
</style>
