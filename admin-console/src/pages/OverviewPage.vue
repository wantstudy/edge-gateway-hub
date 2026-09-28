<template>
  <!--
    OverviewPage —— 总览 / 仪表盘（页面清单第 2 项）。
    结构照 ui-admin-console.md §3.1：异常横幅 → KPI → 趋势/回执健康度 → 待处理。
  -->
  <PageHeader crumb="运营 / 总览" title="总览" desc="厂商侧运营全景：授权规模、回执健康度与待处理事项（B 档下回执是唯一在线证据）">
    <template #actions>
      <button type="button" class="ac-btn" @click="exportCsv">导出日报</button>
    </template>
  </PageHeader>

  <div class="ac-content">
    <!-- 异常横幅：高优先级问题必须显著可见，不藏在二级页面 -->
    <div class="ac-banner ac-banner--warn">
      <div>
        <b>待处理：{{ data.receiptGap }} 台设备回执跳空 · {{ data.receiptMissing }} 台超 24h 无回执 · {{ data.currentKid }} 将在 {{ data.kidRetireInDays }} 天后到期</b>
      </div>
      <div class="ac-banner__ops">
        <button type="button" class="ac-btn ac-btn--primary" @click="go('receipts')">去处理</button>
        <button type="button" class="ac-btn" @click="go('keys')">密钥轮换</button>
      </div>
    </div>

    <!-- KPI -->
    <div class="ac-grid ac-grid--4">
      <StatCard label="租户" :value="String(data.tenantCount)" :delta="data.tenantDelta" sub="本月新增租户" />
      <StatCard
        label="已授权设备"
        :value="String(data.licensedDevices)"
        :sub="`本月新激活 ${data.newActivationsThisMonth}`"
        clickable
        @click="go('devices')"
      />
      <StatCard label="试用中" :value="String(data.trialDevices)" :sub="`7 天内到期 ${data.expiringIn7Days}`" tone="warn" />
      <StatCard
        label="在线设备"
        :value="String(data.onlineDevices)"
        :unit="`/ ${data.licensedDevices}`"
        :sub="`离线 ${data.offlineDevices}`"
        clickable
        @click="go('devices')"
      />
    </div>

    <div class="ac-grid ac-grid--2-1">
      <!-- 新增激活趋势（真实数据：GET /admin/stats/activations，来自审计日志按日聚合） -->
      <section class="ac-card">
        <div class="ac-card__head">
          <h3>激活趋势（近 {{ TREND_DAYS }} 天）</h3>
          <span class="ac-card__sub">发放 / 绑定 / 废弃</span>
        </div>
        <div class="ac-card__body">
          <!-- 加载中 -->
          <p v-if="trendLoading" class="trend-empty">趋势数据加载中…</p>
          <!-- 诚实空态：无数据即说无数据（绝不回退演示曲线） -->
          <div v-else-if="trendEmpty" class="trend-empty">
            <p>暂无激活趋势数据。</p>
            <p class="trend-empty__reason">
              近 {{ TREND_DAYS }} 天内服务端没有发放 / 绑定 / 废弃记录（或趋势端点不可用，原因见页面顶部提示）。
              发放激活码并完成绑定后，这里会出现真实曲线。
            </p>
          </div>
          <BarChart v-else :labels="trendLabels" :series="trendSeries" :height="180" />
        </div>
      </section>

      <!-- 回执健康度 -->
      <section class="ac-card">
        <div class="ac-card__head">
          <h3>回执健康度</h3>
          <span class="ac-card__sub">B 档唯一在线证据</span>
        </div>
        <div class="ac-card__body">
          <dl class="ac-kv">
            <dt>正常</dt>
            <dd><StatusTag status="receipt_ok" :text="`${data.receiptOk} 台`" /></dd>
            <dt>序号跳空</dt>
            <dd><StatusTag status="receipt_gap" :text="`${data.receiptGap} 台`" /></dd>
            <dt>回执缺失 &gt;24h</dt>
            <dd><StatusTag status="receipt_missing" :text="`${data.receiptMissing} 台`" /></dd>
            <dt>签名无效</dt>
            <dd><StatusTag status="receipt_bad_sig" :text="`${data.receiptBadSig} 台`" /></dd>
          </dl>
          <p class="ac-note">
            <span class="ac-note__icon">ⓘ</span>
            <span>
              B 档下厂商不接收业务数据，回执是唯一在线证据。<b>回执异常不等于破解</b>，
              须由客服核实（检修 / 断电 / 更换硬件）后再判定。
              <a href="javascript:void(0)" @click="go('receipts')">前往核实</a>
            </span>
          </p>
        </div>
      </section>
    </div>

    <!-- 待处理 -->
    <section class="ac-card">
      <div class="ac-card__head"><h3>待处理（按优先级）</h3></div>
      <div class="ac-card__body">
        <div class="ac-list">
          <div class="ac-list__item">
            <div>
              <div class="ac-list__title">换机工单 待处理 {{ data.pendingTransfers }} 单</div>
              <div class="ac-list__desc">目标 ≤3 次点击完成</div>
            </div>
            <div class="ac-list__ops">
              <button type="button" class="ac-btn ac-btn--sm ac-btn--primary" @click="go('transfers')">去处理</button>
            </div>
          </div>
          <div class="ac-list__item">
            <div>
              <div class="ac-list__title">回执异常 待核查 {{ data.pendingAnomalies }} 台</div>
              <div class="ac-list__desc">跳空 {{ data.receiptGap }} · 缺失 {{ data.receiptMissing }}</div>
            </div>
            <div class="ac-list__ops">
              <button type="button" class="ac-btn ac-btn--sm" @click="go('receipts')">去核查</button>
            </div>
          </div>
          <div class="ac-list__item">
            <div>
              <div class="ac-list__title">密钥需求</div>
              <div class="ac-list__desc">
                {{ data.currentKid }} 将在 {{ data.kidRetireInDays }} 天后到期；存量旧公钥集客户端 {{ data.legacyClientCount }} 台
              </div>
            </div>
            <div class="ac-list__ops">
              <button type="button" class="ac-btn ac-btn--sm" @click="go('keys')">轮换</button>
            </div>
          </div>
        </div>
      </div>
    </section>
  </div>
</template>

<script setup lang="ts">
/**
 * @file OverviewPage.vue
 * @module admin-console/pages/OverviewPage
 * @description 总览页。KPI 来自 `repo.overview()`；激活趋势来自
 * `GET /admin/stats/activations`（audit_log 按日真实聚合，2026-09-27 起
 * 删除硬编码演示曲线；无数据=诚实空态）。
 */
import { computed, onMounted, ref } from 'vue';
import { useRouter } from 'vue-router';
import { PageHeader, StatCard, StatusTag, SEMANTIC, formatMonthDay, type PageId } from '@ui-kit';
import { repo, type ActivationTrendPoint } from '../api/repo';
import BarChart, { type BarSeries } from '../components/BarChart.vue';

const router = useRouter();

/** 聚合数据（只读快照）。 */
const data = repo.overview();

// ---------------------------------------------------------------------------
// 激活趋势（真实聚合）
// ---------------------------------------------------------------------------
/** 聚合天数。 */
const TREND_DAYS = 30;

/** 趋势加载态。 */
const trendLoading = ref(false);
/** 趋势数据（date 为 UTC 日锚点 unix 秒 String；计数 String 原文透传）。 */
const trendPoints = ref<ActivationTrendPoint[]>([]);

onMounted(async () => {
  trendLoading.value = true;
  trendPoints.value = await repo.activationTrend(TREND_DAYS);
  trendLoading.value = false;
});

/** 空态：无任何日桶（后端无记录 / 端点不可用——真实原因见全局横幅）。 */
const trendEmpty = computed(() => trendPoints.value.length === 0);

/** unix 秒字符串 → `MM-DD` 标签（收敛到 @ui-kit/time 单一出口；UTC 日锚点避免跨时区偏一天）。 */
const dayLabel = (dateSecs: string): string => formatMonthDay(dateSecs);

/** 趋势 X 轴标签。 */
const trendLabels = computed<readonly string[]>(() => trendPoints.value.map((p) => dayLabel(p.date)));

/** 趋势序列（发放 / 绑定 / 废弃；图表渲染需数值——日计数有界，安全）。 */
const trendSeries = computed<BarSeries[]>(() => [
  { name: '发放', color: SEMANTIC.info, data: trendPoints.value.map((p) => Number(p.issue)) },
  { name: '绑定', color: SEMANTIC.ok, data: trendPoints.value.map((p) => Number(p.bind)) },
  { name: '废弃', color: SEMANTIC.danger, data: trendPoints.value.map((p) => Number(p.revoke)) },
]);

/** 跳转。 */
function go(id: PageId): void {
  void router.push({ name: id });
}

/** 导出总览快照为 CSV（前端即时生成真实文件）。
 *  real 模式后端未提供的维度导出为 `—`（与页面一致），绝不补造数字、绝不伪造导出成功。 */
function exportCsv(): void {
  const rows: readonly (readonly [string, string])[] = [
    ['租户数', data.tenantCount],
    ['已授权设备', data.licensedDevices],
    ['试用中', data.trialDevices],
    ['在线设备', data.onlineDevices],
    ['离线设备', data.offlineDevices],
    ['回执正常', data.receiptOk],
    ['回执序号跳空', data.receiptGap],
    ['回执缺失 >24h', data.receiptMissing],
    ['回执签名无效', data.receiptBadSig],
    ['待处理换机工单', data.pendingTransfers],
    ['待核查回执异常', data.pendingAnomalies],
    ['当前签署 kid', data.currentKid],
    ['kid 剩余有效期（天）', data.kidRetireInDays],
    ['存量旧公钥集客户端', data.legacyClientCount],
    ['本月新激活', data.newActivationsThisMonth],
    ['7 天内到期', data.expiringIn7Days],
  ];
  const header = '指标,数值';
  const body = rows.map(([k, v]) => `${k},${v}`).join('\n');
  const blob = new Blob([`\uFEFF${header}\n${body}`], { type: 'text/csv;charset=utf-8' });
  const url = URL.createObjectURL(blob);
  const a = document.createElement('a');
  a.href = url;
  a.download = `overview-${new Date().toISOString().slice(0, 10)}.csv`;
  a.click();
  URL.revokeObjectURL(url);
}
</script>

<style scoped>
/* 趋势空态 / 加载态（本页自用） */
.trend-empty {
  margin: 0;
  min-height: 180px;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 6px;
  color: var(--text-2);
  font-size: var(--fs-table);
}
.trend-empty__reason {
  margin: 0;
  max-width: 460px;
  text-align: center;
  font-size: var(--fs-caption);
  color: var(--text-2);
}
</style>
