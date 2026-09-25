<template>
  <!--
    OverviewPage —— 总览 / 仪表盘（页面清单第 2 项）。
    结构照 ui-admin-console.md §3.1：异常横幅 → KPI → 趋势/回执健康度 → 待处理。
  -->
  <PageHeader crumb="运营 / 总览" title="总览" desc="厂商侧运营全景：授权规模、回执健康度与待处理事项（B 档下回执是唯一在线证据）">
    <template #actions>
      <button type="button" class="ac-btn" @click="notify('日报已导出（演示）')">导出日报</button>
    </template>
  </PageHeader>

  <div class="ac-content">
    <!-- 异常横幅：高优先级问题必须显著可见，不藏在二级页面 -->
    <div class="ac-banner ac-banner--warn">
      <div>
        <b>待处理：{{ data.receiptGap }} 台设备回执跳空 · {{ data.receiptMissing }} 台超 24h 无回执 · 2 张激活码在宽限期 · {{ data.currentKid }} 将在 {{ data.kidRetireInDays }} 天后到期</b>
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
        sub="本月新激活 46"
        clickable
        @click="go('devices')"
      />
      <StatCard label="试用中" :value="String(data.trialDevices)" sub="7 天内到期 9" tone="warn" />
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
      <!-- 新增激活趋势 -->
      <section class="ac-card">
        <div class="ac-card__head">
          <h3>激活趋势（近 30 天）</h3>
          <span class="ac-card__sub">发放 / 绑定 / 废弃</span>
        </div>
        <div class="ac-card__body">
          <BarChart :labels="trendLabels" :series="trendSeries" :height="180" />
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
              <div class="ac-list__desc">平均处理时长 2.1h · 目标 ≤3 次点击完成</div>
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
                {{ data.currentKid }} 计划退役 2026-10-01；存量旧公钥集客户端 {{ data.legacyClientCount }} 台
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
 * @description 总览页。数据来自 mock 仓库的 `overview()` 聚合（真实环境为 GET /admin/overview）。
 */
import { computed } from 'vue';
import { useRouter } from 'vue-router';
import { PageHeader, StatCard, StatusTag, SEMANTIC, type PageId } from '@ui-kit';
import { repo } from '../api/repo';
import BarChart, { type BarSeries } from '../components/BarChart.vue';

const router = useRouter();

/** 聚合数据（只读快照）。 */
const data = repo.overview();

/** 趋势 X 轴标签。 */
const trendLabels: readonly string[] = [
  '08-25', '08-28', '08-31', '09-03', '09-06', '09-09', '09-12', '09-15', '09-18', '09-21', '09-23',
];

/** 趋势序列（发放 / 绑定 / 废弃）。 */
const trendSeries = computed<BarSeries[]>(() => [
  { name: '发放', color: SEMANTIC.info, data: [12, 8, 15, 11, 9, 18, 22, 14, 17, 12, 9] },
  { name: '绑定', color: SEMANTIC.ok, data: [10, 7, 14, 10, 8, 16, 20, 13, 15, 11, 8] },
  { name: '废弃', color: SEMANTIC.danger, data: [1, 0, 2, 1, 1, 0, 3, 1, 2, 1, 1] },
]);

/** 跳转。 */
function go(id: PageId): void {
  void router.push({ name: id });
}

/** 轻提示（原型用原生 alert，避免依赖 Arco 全局 API 注入顺序）。 */
function notify(message: string): void {
  window.alert(message);
}
</script>
