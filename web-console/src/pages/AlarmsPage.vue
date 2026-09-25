<template>
  <!--
    AlarmsPage —— 告警中心（监控分组第 3 页，路由 `/alarms`）。

    结构对齐原型 `alarms`（:1475 起）与设计 §3（告警相关线框）：
      ① KPI 卡片行（严重 / 重要 / 24h 新增 / 已确认）
      ② 筛选栏（级别 / 处置状态 / 时间范围 / 关键字）
      ③ 告警列表（分页，UiPager 单一口径）+ 行内确认
      ④ 告警详情抽屉
      ⑤ 告警规则配置区（阈值 / 持续时间 / 抑制；危险操作走 DangerConfirmModal 四要素）

    硬性约定遵守情况：
      · 列表页必须分页，且条数只有 UiPager 一个口径（表格无 footer「共 N 条」）；
      · 空态给下一步动作（EmptyState）；
      · 危险操作（删除规则 / 关闭总开关）= 二次确认 + 原因必填 + 对象名二次校验；
      · 无解绑 / 重置试用 / revoke 任何入口；无 emoji。
  -->
  <PageHeader
    crumb="运行监控 / 告警中心"
    title="告警中心"
    desc="由阈值与通讯异常产生的告警，支持确认与静默。规则变更与删除为高危操作，需二次确认并填写原因。"
  >
    <template #actions>
      <button type="button" class="wc-btn" @click="exportCsv">导出 CSV</button>
      <button type="button" class="wc-btn wc-btn--primary" @click="batchAck">批量确认当前页</button>
    </template>
  </PageHeader>

  <div class="wc-content">
    <!-- KPI：数值由当前告警快照实时推导 -->
    <div class="wc-grid wc-grid--4">
      <StatCard
        label="严重"
        :value="String(kpi.critical)"
        :delta="kpi.critical > 0 ? 1 : 0"
        sub="需立即处理"
        :tone="kpi.critical > 0 ? 'danger' : 'default'"
        icon-tone="rose"
      >
        <template #icon>✕</template>
      </StatCard>
      <StatCard
        label="警告"
        :value="String(kpi.warning)"
        :sub="`含 ${kpi.info} 条提示`"
        :tone="kpi.warning > 0 ? 'warn' : 'default'"
        icon-tone="amber"
      >
        <template #icon>!</template>
      </StatCard>
      <StatCard label="24h 新增" :value="String(kpi.total)" sub="近 24 小时产生的告警" icon-tone="ink">
        <template #icon>◷</template>
      </StatCard>
      <StatCard label="已确认 / 已恢复" :value="String(kpi.closed)" :sub="`待处理 ${kpi.open} 条`" tone="ok" icon-tone="teal">
        <template #icon>✓</template>
      </StatCard>
    </div>

    <!-- 筛选栏 -->
    <div class="wc-card">
      <div class="wc-card__body">
        <div class="wc-filters">
          <div class="wc-filters__item">
            <label for="al-level">级别</label>
            <UiSelect v-model="filters.level" :options="levelOptions" />
          </div>
          <div class="wc-filters__item">
            <label for="al-state">处理状态</label>
            <UiSelect v-model="filters.state" :options="stateOptions" />
          </div>
          <div class="wc-filters__item">
            <label for="al-range">时间范围</label>
            <UiSelect v-model="filters.range" :options="rangeOptions" />
          </div>
          <div class="wc-filters__item wc-filters__grow">
            <label for="al-keyword">搜索（对象 / 标题 / 详情）</label>
            <UiInput v-model="filters.keyword" placeholder="输入关键字后自动筛选" />
          </div>
        </div>
      </div>
    </div>

    <!-- 告警列表 -->
    <section class="wc-card">
      <div class="wc-card__head">
        <h3>告警列表</h3>
        <span class="wc-card__sub">按最近触发时间倒序</span>
        <div class="wc-card__ops">
          <span v-if="selectedIds.length > 0" class="wc-tag wc-tag--info">已选 {{ selectedIds.length }} 条</span>
        </div>
      </div>

      <!-- real 模式：后端无告警引擎（`GET /api/alerts` → source=unsupported）→ 可解释空态 -->
      <div
        v-if="alertsUnsupported"
        class="wc-banner wc-banner--warn wc-banner--block al-unsupported"
        data-testid="alerts-unsupported"
      >
        <span aria-hidden="true">!</span>
        <span class="wc-banner__stack">
          <span class="wc-banner__line"><b>当前没有告警数据 · 后端告警引擎未落地</b></span>
          <span class="wc-banner__line">原因：{{ alertsApiNotice }}</span>
          <span class="wc-banner__line">
            恢复路径：告警引擎落地后本页自动展示；在此之前请用「实时监控」观察通讯异常、
            或用「诊断与自检」排查连通性。本页<b>绝不伪造告警数据</b>。
          </span>
        </span>
      </div>

      <EmptyState
        v-if="filtered.length === 0"
        title="没有符合条件的告警"
        desc="可能是筛选条件过窄（级别 / 状态 / 时间范围 / 关键字）。你可以清空筛选查看全部，或检查阈值规则是否需要调整。"
      >
        <template #actions>
          <button type="button" class="wc-btn" @click="resetFilters">清空筛选</button>
          <button type="button" class="wc-btn wc-btn--primary" @click="scrollToRules">调整告警规则</button>
        </template>
      </EmptyState>

      <template v-else>
        <UiTable :columns="columns" :rows="paged.items" row-key-field="id">
          <template #cell-_select="{ row }">
            <input
              type="checkbox"
              :checked="selectedIds.includes(row.id)"
              :aria-label="`选择告警 ${row.title}`"
              @change="toggleSelect(row.id)"
            />
          </template>
          <template #cell-level="{ row }">
            <span class="wc-tag" :class="levelTagClass(row.level)">{{ levelTextOf(row.level) }}</span>
          </template>
          <template #cell-lastSeenAt="{ row }">
            <span class="wc-mono">{{ row.lastSeenAt }}</span>
          </template>
          <template #cell-sourceLabel="{ row }">
            <span>{{ row.sourceLabel }}</span>
            <span class="al-src">{{ sourceTypeLabel(row.sourceType) }}</span>
          </template>
          <template #cell-title="{ row }">
            <div class="al-title">{{ row.title }}</div>
            <div class="al-detail">{{ row.detail }}</div>
          </template>
          <template #cell-count="{ row }">
            <span class="wc-mono">{{ row.count }}</span>
          </template>
          <template #cell-stateLabel="{ row }">
            <span class="wc-tag" :class="stateTagClass(row.state)">{{ row.stateLabel }}</span>
          </template>
          <template #actions="{ row }">
            <button type="button" class="wc-btn wc-btn--sm" @click="openDetail(row.id)">详情</button>
            <RoleGate :allowed="canAck" mode="disable" deny-text="当前角色只读，无「确认告警」权限">
              <button
                v-if="row.state === 'open'"
                type="button"
                class="wc-btn wc-btn--sm wc-btn--primary"
                @click="confirmAlarm(row.id)"
              >
                确认
              </button>
            </RoleGate>
          </template>
        </UiTable>

        <UiPager :page="page" :total="filtered.length" :page-size="PAGE_SIZE" @update:page="onPage" />
      </template>
    </section>

    <!-- 告警规则配置区 -->
    <section ref="rulesRef" class="wc-card">
      <div class="wc-card__head">
        <h3>告警规则</h3>
        <span class="wc-card__sub">阈值 / 持续时间 / 抑制</span>
      </div>

      <div class="wc-card__body">
        <div class="wc-grid wc-grid--3">
          <UiField label="阈值条件" required hint="引用点位用 [ ] 包裹，如 [T_Barrel1] > 240">
            <UiInput v-model="ruleDraft.threshold" placeholder="[T_Barrel1] > 240" />
          </UiField>
          <UiField label="持续时间" required hint="条件持续满足多久才产生告警（秒）">
            <UiInput v-model="ruleDraft.durationSec" type="number" placeholder="10" />
          </UiField>
          <UiField label="抑制窗口" required hint="同对象在此窗口内不重复告警（分钟）">
            <UiInput v-model="ruleDraft.suppressMin" type="number" placeholder="5" />
          </UiField>
        </div>

        <UiField label="适用对象" required hint="留空表示全部设备">
          <UiSelect v-model="ruleDraft.target" :options="targetOptions" />
        </UiField>

        <p v-if="ruleError" class="al-error" role="alert">{{ ruleError }}</p>

        <p v-if="ruleMessage" class="wc-hint" data-testid="rule-message">{{ ruleMessage }}</p>

        <!-- real 模式：告警规则写接口当前返回 501 —— 诚实告知，不静默吞错 -->
        <p v-if="IS_REAL" class="wc-note">
          <span class="wc-note__icon" aria-hidden="true">ⓘ</span>
          <span>
            规则写入走 <span class="wc-mono">PUT /api/alerts/rules</span>；后端当前返回
            <b>501 not_implemented</b>（告警引擎未落地，无规则可写）。保存时本页会原样呈现原因与恢复路径，
            不会显示「已保存」。
          </span>
        </p>

        <div class="al-rule-actions">
          <button type="button" class="wc-btn" @click="resetRuleDraft">重置草稿</button>
          <button type="button" class="wc-btn wc-btn--primary" :disabled="ruleError.length > 0" @click="saveRule">
            保存规则
          </button>
        </div>

        <!-- 已保存规则 -->
        <div v-if="rules.length === 0" class="al-hint-block">
          <EmptyState
            title="尚未配置告警规则"
            desc="没有规则时，只有驱动层通讯异常会产生告警（如离线、采集失败）。建议先为关键点位配置阈值规则。"
          >
            <template #actions>
              <button type="button" class="wc-btn wc-btn--primary" @click="applyPreset">载入推荐模板</button>
            </template>
          </EmptyState>
        </div>

        <div v-else class="wc-table-wrap">
          <table class="wc-table">
            <thead>
              <tr>
                <th>规则</th>
                <th>条件</th>
                <th>持续时间</th>
                <th>抑制</th>
                <th>触发</th>
                <th>状态</th>
                <th class="is-right">操作</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="rule in rules" :key="rule.id">
                <td>{{ rule.name }}</td>
                <td class="wc-mono">{{ rule.condition }}</td>
                <td class="wc-mono">{{ rule.durationSec }} s</td>
                <td class="wc-mono">{{ rule.suppressMin }} min</td>
                <td class="wc-mono">{{ rule.hitCount }}</td>
                <td>
                  <span class="wc-tag" :class="rule.enabled ? 'wc-tag--ok' : 'wc-tag--unknown'">
                    {{ rule.enabled ? '已启用' : '已停用' }}
                  </span>
                </td>
                <td class="is-right">
                  <div class="al-ops">
                    <RoleGate :allowed="canEditRules" mode="disable" deny-text="当前角色只读，无「规则编辑」权限">
                      <button type="button" class="wc-btn wc-btn--sm" @click="toggleRule(rule.id)">
                        {{ rule.enabled ? '停用' : '启用' }}
                      </button>
                    </RoleGate>
                    <RoleGate :allowed="canEditRules" mode="disable" deny-text="当前角色只读，无「删除规则」权限">
                      <button type="button" class="wc-btn wc-btn--sm wc-btn--danger" @click="askDelete(rule)">
                        删除
                      </button>
                    </RoleGate>
                  </div>
                </td>
              </tr>
            </tbody>
          </table>
        </div>

        <p class="wc-note">
          <span class="wc-note__icon">ⓘ</span>
          <span>
            规则变更会写入审计日志。删除规则<b>不影响历史告警</b>，但此后不再产生新告警；恢复路径为重新创建同名规则。
          </span>
        </p>
      </div>
    </section>

    <!-- 危险操作二次确认（删除规则）：四要素 —— 影响清单 + 原因必填 + 对象名二次校验 -->
    <DangerConfirmModal
      :open="deleteOpen"
      :title="`删除告警规则「${deleteTarget?.name ?? ''}」`"
      :impacts="[
        `该规则将立即停止评估，此后不再对「${deleteTarget?.condition ?? ''}」产生新告警。`,
        '已产生的历史告警与处置记录保留，不受影响。',
        '本操作不可撤销。恢复路径：按相同条件重新创建规则。',
      ]"
      :facts="deleteFacts"
      :reasons="DELETE_REASONS"
      :confirm-value="deleteTarget?.name ?? ''"
      confirm-label="风险二次确认（输入规则名称）"
      confirm-placeholder="完整输入规则名称以防误删"
      confirm-text="删除告警规则"
      @close="closeDelete"
      @submit="submitDelete"
    />

    <!-- 告警详情抽屉 -->
    <Teleport to="body">
      <div v-if="detailOpen && detail" class="al-mask" @click.self="closeDetail">
        <aside class="al-drawer" role="dialog" aria-modal="true" aria-label="告警详情">
          <div class="al-drawer__head">
            <h3>告警详情</h3>
            <button type="button" class="wc-btn wc-btn--sm" @click="closeDetail">关闭</button>
          </div>
          <div class="al-drawer__body">
            <dl class="wc-kv">
              <dt>级别</dt>
              <dd><span class="wc-tag" :class="levelTagClass(detail.level)">{{ levelTextOf(detail.level) }}</span></dd>
              <dt>对象</dt>
              <dd>{{ detail.sourceLabel }}（{{ sourceTypeLabel(detail.sourceType) }}）</dd>
              <dt>标题</dt>
              <dd>{{ detail.title }}</dd>
              <dt>详情</dt>
              <dd>{{ detail.detail }}</dd>
              <dt>首次触发</dt>
              <dd class="wc-mono">{{ detail.firstSeenAt }}</dd>
              <dt>最近触发</dt>
              <dd class="wc-mono">{{ detail.lastSeenAt }}</dd>
              <dt>触发次数</dt>
              <dd class="wc-mono">{{ detail.count }}</dd>
              <dt>处置状态</dt>
              <dd><span class="wc-tag" :class="stateTagClass(detail.state)">{{ detail.stateLabel }}</span></dd>
              <dt>处置人</dt>
              <dd>{{ detail.ackedBy || '—' }}</dd>
              <dt>处置说明</dt>
              <dd>{{ detail.note || '—' }}</dd>
            </dl>

            <UiField label="处置说明" required hint="必填，将随处置动作写入审计日志">
              <UiInput v-model="ackNote" placeholder="如：已联系电气班检查通讯线" />
            </UiField>

            <div class="al-drawer__ops">
              <RoleGate :allowed="canAck" mode="disable" deny-text="当前角色只读，无「处置告警」权限">
                <button type="button" class="wc-btn wc-btn--primary" :disabled="ackNote.trim().length === 0" @click="ackFromDetail">
                  标记已确认
                </button>
              </RoleGate>
              <button type="button" class="wc-btn" @click="go('audit')">查看相关审计</button>
            </div>
          </div>
        </aside>
      </div>
    </Teleport>

    <p class="wc-note">
      <span class="wc-note__icon">ⓘ</span>
      <span>
        本页数据来自内嵌演示数据源（真实环境为 <span class="wc-mono">GET /api/alerts</span> 与
        <span class="wc-mono">PUT /api/alerts/rules</span>）。处置告警只改变告警状态，
        <b>不影响任何授权能力</b>；授权判定一律在网关侧（Rust）完成。
      </span>
    </p>
  </div>
</template>

<script setup lang="ts">
/**
 * @file AlarmsPage.vue
 * @module web-console/pages/AlarmsPage
 * @description 告警中心：列表（分页）+ 级别/状态/时间/关键字筛选 + 详情 + 规则配置。
 *
 * ── 草稿隔离 ────────────────────────────────────────────────────────────────
 * 规则草稿（`ruleDraft`）与处置说明草稿（`ackNote`）都**只存在于页面本地**，
 * 绝不可直接写入 `repo` 返回的记录对象；保存时才提交。
 */
import { computed, onMounted, reactive, ref, watch } from 'vue';
import { useRouter } from 'vue-router';
import {
  DangerConfirmModal,
  EmptyState,
  PageHeader,
  RoleGate,
  StatCard,
  UiField,
  UiInput,
  UiPager,
  UiSelect,
  UiTable,
  type DangerFact,
  type SelectOption,
  type TableColumn,
} from '@ui-kit';
import { API_MODE, repo, type AlarmLevel, type AlarmRecord, type AlarmState } from '@/api/repo';
import { apiRequest, ApiError } from '@/api/client';
import { session } from '../store/session';

const router = useRouter();

/** 是否接入真实后端（`VITE_API_MODE=real`）；mock 模式行为保持与原版一致。 */
const IS_REAL = API_MODE === 'real';

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/** 每页条数。 */
const PAGE_SIZE = 6;

/** 删除规则的必选原因枚举（写清原因才能解释「谁在何时为何做了此事」）。 */
const DELETE_REASONS: readonly string[] = [
  '规则条件已过时（阈值/点位变更）',
  '与其它规则重复',
  '误配置',
  '业务调整，不再需要',
];

/** 告警列表列定义（`_select` 为勾选列）。 */
const columns: readonly TableColumn[] = [
  { key: '_select', label: '选' },
  { key: 'level', label: '级别' },
  { key: 'lastSeenAt', label: '最近触发', mono: true },
  { key: 'sourceLabel', label: '对象' },
  { key: 'title', label: '内容' },
  { key: 'count', label: '次数', align: 'right', mono: true },
  { key: 'stateLabel', label: '状态' },
];

// ---------------------------------------------------------------------------
// 权限（RoleGate 仅控制可见性；判定在网关侧）
// ---------------------------------------------------------------------------

/** 是否可处置告警。 */
const canAck = computed<boolean>(() => session.state.role !== 'viewer');

/** 是否可编辑规则。 */
const canEditRules = computed<boolean>(() => session.state.role === 'admin' || session.state.role === 'engineer');

// ---------------------------------------------------------------------------
// 告警数据
// ---------------------------------------------------------------------------

/** 全部告警快照。 */
const alarms = ref<AlarmRecord[]>(repo.allAlarms());

/**
 * 告警数据源不可得 / 不支持的说明（real 模式：`GET /api/alerts` 的 `reason` 原文）。
 *
 * 非空即表示「后端当前没有真实告警数据源」，页面据此给出可解释空态。
 */
const alertsApiNotice = ref('');

/** real 模式且后端无告警数据源 → 展示可解释空态（绝不伪造告警）。 */
const alertsUnsupported = computed<boolean>(() => IS_REAL && alarms.value.length === 0 && alertsApiNotice.value !== '');

/** 取字符串字段（数字按字符串透传）。 */
function pickText(src: Record<string, unknown>, key: string, dflt: string): string {
  const v = src[key];
  if (typeof v === 'string') {
    return v;
  }
  if (typeof v === 'number' && Number.isFinite(v)) {
    return String(v);
  }
  return dflt;
}

/** `/api/alerts` 条目 → 告警记录（后端当前恒为空数组；落地后按契约宽容映射）。 */
function mapAlarmRow(raw: Record<string, unknown>, idx: number): AlarmRecord {
  const level = pickText(raw, 'level', 'minor');
  const state = pickText(raw, 'state', 'open');
  return {
    id: pickText(raw, 'id', `al-${idx}`),
    level: (['critical', 'major', 'minor', 'warning'].includes(level) ? level : 'minor') as AlarmLevel,
    levelLabel: levelTextOf((['critical', 'major', 'minor', 'warning'].includes(level) ? level : 'minor') as AlarmLevel),
    sourceType: pickText(raw, 'sourceType', pickText(raw, 'source_type', 'system')),
    sourceLabel: pickText(raw, 'sourceLabel', pickText(raw, 'source', '—')),
    title: pickText(raw, 'title', '—'),
    detail: pickText(raw, 'detail', ''),
    firstSeenAt: pickText(raw, 'firstSeenAt', pickText(raw, 'first_seen_at', '—')),
    lastSeenAt: pickText(raw, 'lastSeenAt', pickText(raw, 'last_seen_at', '—')),
    count: 1,
    state: (['open', 'acking', 'resolved'].includes(state) ? state : 'open') as AlarmState,
    stateLabel: STATE_TEXT[state] ?? state,
    ackedBy: pickText(raw, 'ackedBy', pickText(raw, 'acked_by', '')),
    note: pickText(raw, 'note', ''),
  };
}

/** 处置状态 → 中文（与数据层口径一致）。 */
const STATE_TEXT: Record<string, string> = { open: '待处理', acking: '已确认', resolved: '已恢复' };

/**
 * 加载告警。
 *
 * real：以 `GET /api/alerts` 为**唯一**告警数据源（后端无告警引擎 →
 * `{items:[],source:"unsupported",reason}`，页面给出可解释空态，**绝不回退 mock 造假**）；
 * 接口不可得（网络 / 5xx）时沿用现有来源并如实说明。
 * mock：保持原演示数据源。
 */
async function loadAlarms(): Promise<void> {
  if (!IS_REAL) {
    alarms.value = repo.allAlarms();
    return;
  }
  try {
    const raw = await apiRequest<Record<string, unknown>>('/api/alerts');
    const items = Array.isArray(raw['items']) ? raw['items'] : [];
    alarms.value = items
      .map((row, i) => (row !== null && typeof row === 'object' && !Array.isArray(row) ? mapAlarmRow(row as Record<string, unknown>, i) : null))
      .filter((a): a is AlarmRecord => a !== null);
    alertsApiNotice.value =
      raw['source'] === 'unsupported'
        ? pickText(raw, 'reason', '后端告警引擎未落地，当前无真实告警数据源')
        : '';
  } catch (cause) {
    alarms.value = repo.allAlarms();
    const code = cause instanceof ApiError ? cause.status : 0;
    alertsApiNotice.value =
      code === 0
        ? 'GET /api/alerts 不可得：网关不可达（网络层失败）；当前展示的是现有来源数据。'
        : `GET /api/alerts 不可得：HTTP ${code}；当前展示的是现有来源数据。`;
  }
}

onMounted(() => {
  void loadAlarms();
});

/** 筛选草稿（页面级，非持久）。 */
const filters = reactive({
  level: '' as LevelBucket,
  state: '',
  range: '24h',
  keyword: '',
});

// ---------------------------------------------------------------------------
// 级别枚举（原型 :1480-1483 / :1493-1504：**严重 / 警告 / 提示** 三档）
// ---------------------------------------------------------------------------

/** 告警级别桶（原型三档）。 */
type LevelBucket = '' | 'critical' | 'warning' | 'info';

/** 桶 → 展示文案（原型用字，不另行造同义词）。 */
const LEVEL_TEXT: Record<'critical' | 'warning' | 'info', string> = {
  critical: '严重',
  warning: '警告',
  info: '提示',
};

/**
 * 记录级别 → 桶。
 *
 * 数据层沿用 `critical / major / minor / warning` 四值；页面按原型收敛为三档展示：
 * `major`（重要）与 `minor`（次要）同属「警告」，`warning` 归「提示」。
 */
function bucketOf(level: AlarmLevel): 'critical' | 'warning' | 'info' {
  if (level === 'critical') {
    return 'critical';
  }
  if (level === 'warning') {
    return 'info';
  }
  return 'warning';
}

/** 记录 → 级别展示文案（覆盖数据层 levelLabel，保证枚举与原型一致）。 */
function levelTextOf(level: AlarmLevel): string {
  return LEVEL_TEXT[bucketOf(level)];
}

/** 级别选项（原型三档 + 全部）。 */
const levelOptions: readonly SelectOption[] = [
  { value: '', label: '全部级别' },
  { value: 'critical', label: '严重' },
  { value: 'warning', label: '警告' },
  { value: 'info', label: '提示' },
];

/** 级别 tag 色调：严重 danger / 警告 warn / 提示 中性。 */
function levelTagClass(level: AlarmLevel): string {
  const bucket = bucketOf(level);
  if (bucket === 'critical') {
    return 'wc-tag--danger';
  }
  if (bucket === 'warning') {
    return 'wc-tag--warn';
  }
  return 'wc-tag--unknown';
}

/** 处理状态选项。 */
const stateOptions: readonly SelectOption[] = [
  { value: '', label: '全部状态' },
  { value: 'open', label: '待处理' },
  { value: 'acking', label: '已确认' },
  { value: 'resolved', label: '已恢复' },
];

/** 时间范围选项。 */
const rangeOptions: readonly SelectOption[] = [
  { value: '1h', label: '近 1 小时' },
  { value: '24h', label: '近 24 小时' },
  { value: '7d', label: '近 7 天' },
  { value: 'all', label: '全部时间' },
];

/**
 * 时间范围 → 起始时刻（`YYYY-MM-DD HH:mm:ss`）。
 *
 * 演示数据的时间戳固定在 2026-09-23，因此以数据集内的**最大时间戳**为「现在」，
 * 而不是真实系统时间 —— 否则所有记录都会被 24h 窗口过滤掉（现场演示会看到空列表）。
 */
function rangeStart(range: string): string {
  if (range === 'all') {
    return '';
  }
  const timestamps = alarms.value.map((a) => a.lastSeenAt).sort();
  const latest = timestamps.length > 0 ? timestamps[timestamps.length - 1] : '';
  if (!latest) {
    return '';
  }
  const parts = latest.replace(' ', 'T');
  const base = new Date(parts);
  if (Number.isNaN(base.getTime())) {
    return '';
  }
  const windowMs = range === '1h' ? 3_600_000 : range === '24h' ? 86_400_000 : 7 * 86_400_000;
  const start = new Date(base.getTime() - windowMs);
  const p = (n: number): string => String(n).padStart(2, '0');
  return `${start.getFullYear()}-${p(start.getMonth() + 1)}-${p(start.getDate())} ${p(start.getHours())}:${p(start.getMinutes())}:${p(start.getSeconds())}`;
}

/** 筛选后的告警（按最近触发时间倒序）。 */
const filtered = computed<readonly AlarmRecord[]>(() => {
  const kw = filters.keyword.trim().toLowerCase();
  const start = rangeStart(filters.range);
  return alarms.value
    .filter((a) => {
      // 级别按原型三档（严重 / 警告 / 提示）筛选
      if (filters.level && bucketOf(a.level) !== filters.level) {
        return false;
      }
      if (filters.state && a.state !== filters.state) {
        return false;
      }
      if (start && a.lastSeenAt < start) {
        return false;
      }
      if (kw) {
        const haystack = `${a.sourceLabel} ${a.title} ${a.detail} ${levelTextOf(a.level)}`.toLowerCase();
        if (!haystack.includes(kw)) {
          return false;
        }
      }
      return true;
    })
    .sort((a, b) => b.lastSeenAt.localeCompare(a.lastSeenAt));
});

/** KPI（基于**全部**告警，不受筛选影响；级别按原型三档统计）。 */
const kpi = computed<{ critical: number; warning: number; info: number; total: number; open: number; closed: number }>(
  () => {
    const list = alarms.value;
    return {
      critical: list.filter((a) => bucketOf(a.level) === 'critical').length,
      warning: list.filter((a) => bucketOf(a.level) === 'warning').length,
      info: list.filter((a) => bucketOf(a.level) === 'info').length,
      total: list.length,
      open: list.filter((a) => a.state === 'open').length,
      closed: list.filter((a) => a.state !== 'open').length,
    };
  },
);

/** 当前页。 */
const page = ref<number>(1);

/** 筛选变化 → 回到第 1 页。 */
watch(
  () => [filters.level, filters.state, filters.range, filters.keyword],
  () => {
    page.value = 1;
  },
);

/** 当前页数据。 */
const paged = computed<{ items: readonly AlarmRecord[]; page: number }>(() => {
  const start = (page.value - 1) * PAGE_SIZE;
  return { items: filtered.value.slice(start, start + PAGE_SIZE), page: page.value };
});

/** 换页。 */
function onPage(next: number): void {
  page.value = next;
}

/** 清空筛选。 */
function resetFilters(): void {
  filters.level = '';
  filters.state = '';
  filters.range = '24h';
  filters.keyword = '';
}

/** 勾选状态（**只存 id**，避免持有对象引用后脏写）。 */
const selectedIds = ref<string[]>([]);

/** 勾选 / 取消勾选。 */
function toggleSelect(id: string): void {
  selectedIds.value = selectedIds.value.includes(id)
    ? selectedIds.value.filter((x) => x !== id)
    : [...selectedIds.value, id];
}

/** 批量确认当前页中处于「待处理」的告警。 */
function batchAck(): void {
  const targets = paged.value.items.filter((a) => a.state === 'open');
  if (targets.length === 0) {
    window.alert('当前页没有「待处理」的告警。');
    return;
  }
  const ok = window.confirm(`将确认当前页 ${targets.length} 条待处理告警，并写入审计日志。确认继续？`);
  if (!ok) {
    return;
  }
  for (const alarm of targets) {
    repo.resolveAlarm({
      id: alarm.id,
      state: 'acking',
      note: '批量确认（告警中心）',
      actor: session.state.displayName,
    });
  }
  selectedIds.value = [];
  alarms.value = repo.allAlarms();
}

/** 单条确认。 */
function confirmAlarm(id: string): void {
  repo.resolveAlarm({ id, state: 'acking', note: '确认告警（告警中心）', actor: session.state.displayName });
  alarms.value = repo.allAlarms();
}

// ---------------------------------------------------------------------------
// 详情抽屉
// ---------------------------------------------------------------------------

/** 详情开关。 */
const detailOpen = ref<boolean>(false);

/** 详情目标 id。 */
const detailId = ref<string>('');

/** 处置说明草稿。 */
const ackNote = ref<string>('');

/** 当前详情记录（按 id 取快照）。 */
const detail = computed<AlarmRecord | null>(() => (detailId.value ? (alarms.value.find((a) => a.id === detailId.value) ?? null) : null));

/** 打开详情（重置处置草稿）。 */
function openDetail(id: string): void {
  detailId.value = id;
  ackNote.value = '';
  detailOpen.value = true;
}

/** 关闭详情（清草稿）。 */
function closeDetail(): void {
  detailOpen.value = false;
  detailId.value = '';
  ackNote.value = '';
}

/** 从详情处置。 */
function ackFromDetail(): void {
  if (!detailId.value || ackNote.value.trim().length === 0) {
    return;
  }
  repo.resolveAlarm({ id: detailId.value, state: 'acking', note: ackNote.value.trim(), actor: session.state.displayName });
  alarms.value = repo.allAlarms();
  closeDetail();
}

// ---------------------------------------------------------------------------
// 规则配置（阈值 / 持续时间 / 抑制）
// ---------------------------------------------------------------------------

/** 已保存规则（页面本地）。 */
interface AlarmRule {
  /** 主键 */
  readonly id: string;
  /** 规则名 */
  readonly name: string;
  /** 条件表达式 */
  readonly condition: string;
  /** 持续时间（秒） */
  readonly durationSec: number;
  /** 抑制窗口（分钟） */
  readonly suppressMin: number;
  /** 触发次数 */
  hitCount: number;
  /** 是否启用 */
  enabled: boolean;
}

/** 规则表（初始为空 —— 由用户在页面上创建，或载入推荐模板）。 */
const rules = ref<AlarmRule[]>([]);

/** 规则草稿（页面本地，保存时才提交）。 */
const ruleDraft = reactive({
  threshold: '',
  durationSec: '',
  suppressMin: '',
  target: '',
});

/** 适用对象选项（'' = 全部设备）。 */
const targetOptions = computed<readonly SelectOption[]>(() => [
  { value: '', label: '全部设备' },
  ...repo.allDevices().map((d) => ({ value: d.id, label: d.name })),
]);

/** 规则草稿校验错误（非空则禁止保存）。 */
const ruleError = computed<string>(() => {
  const threshold = ruleDraft.threshold.trim();
  if (threshold.length === 0) {
    return '阈值条件不能为空，例如 [T_Barrel1] > 240';
  }
  if (!/[<>=]/.test(threshold)) {
    return '阈值条件必须包含比较运算符（<、>、=），例如 [T_Barrel1] > 240';
  }
  const duration = Number(ruleDraft.durationSec);
  if (!Number.isFinite(duration) || duration <= 0) {
    return '持续时间必须为正整数（秒），例如 10';
  }
  const suppress = Number(ruleDraft.suppressMin);
  if (!Number.isFinite(suppress) || suppress < 0) {
    return '抑制窗口不能为负数（分钟），0 表示不抑制';
  }
  return '';
});

/** 重置规则草稿。 */
function resetRuleDraft(): void {
  ruleDraft.threshold = '';
  ruleDraft.durationSec = '';
  ruleDraft.suppressMin = '';
  ruleDraft.target = '';
}

/** 规则保存结果提示（real 模式的 501 / 403 等结构化结果写在这里）。 */
const ruleMessage = ref('');

/** 告警规则写失败的「原因 + 恢复路径」（后端 `PUT /api/alerts/rules` 当前返回 501）。 */
function ruleWriteFailureText(cause: unknown): string {
  const code = cause instanceof ApiError ? cause.status : 0;
  if (code === 501) {
    return (
      '规则未保存：后端告警引擎未落地（HTTP 501 not_implemented），无规则可写。' +
      '恢复路径：告警引擎落地后本表单即可直接保存；在此之前告警只来自驱动层通讯异常。'
    );
  }
  if (code === 403) {
    return '规则未保存：当前账号无 device.write 权限（HTTP 403）。恢复路径：改用具备该权限的账号登录。';
  }
  if (code === 0) {
    return '规则未保存：网关不可达（网络层失败）。恢复路径：确认网关进程在监听 8080 端口后重试。';
  }
  return `规则未保存：HTTP ${code}。恢复路径：查看网关日志定位后重试。`;
}

/**
 * 保存规则（危险操作之外的一般写操作；仍需审计）。
 *
 * real：`PUT /api/alerts/rules` —— 后端当前返回 501，页面**原样呈现原因与恢复路径**，
 * 绝不把 501 吞成「已保存」；
 * mock：保持原本地演示行为。
 */
async function saveRule(): Promise<void> {
  if (ruleError.value.length > 0) {
    return;
  }
  const target = ruleDraft.target ? repo.getDevice(ruleDraft.target)?.name ?? '指定设备' : '全部设备';
  if (IS_REAL) {
    ruleMessage.value = '';
    try {
      await apiRequest<unknown>('/api/alerts/rules', {
        method: 'PUT',
        body: JSON.stringify({
          threshold: ruleDraft.threshold.trim(),
          duration_sec: ruleDraft.durationSec,
          suppress_min: ruleDraft.suppressMin,
          target: ruleDraft.target,
        }),
      });
      ruleMessage.value = `规则「${target} · ${ruleDraft.threshold.trim()}」已保存。`;
      resetRuleDraft();
    } catch (cause) {
      ruleMessage.value = ruleWriteFailureText(cause);
    }
    return;
  }
  rules.value = [
    ...rules.value,
    {
      id: `rule-${Date.now()}`,
      name: `${target} · ${ruleDraft.threshold.trim()}`,
      condition: ruleDraft.threshold.trim(),
      durationSec: Number(ruleDraft.durationSec),
      suppressMin: Number(ruleDraft.suppressMin),
      hitCount: 0,
      enabled: true,
    },
  ];
  resetRuleDraft();
}

/** 载入推荐模板（空态动作）。 */
function applyPreset(): void {
  rules.value = [
    {
      id: `rule-${Date.now()}-1`,
      name: '全部设备 · 点位质量 Bad 连续出现',
      condition: 'quality = Bad',
      durationSec: 10,
      suppressMin: 5,
      hitCount: 0,
      enabled: true,
    },
    {
      id: `rule-${Date.now()}-2`,
      name: '全部设备 · 北向出口不可达',
      condition: 'forwarder.status != connected',
      durationSec: 30,
      suppressMin: 10,
      hitCount: 0,
      enabled: true,
    },
  ];
}

/** 启用 / 停用规则。 */
function toggleRule(id: string): void {
  rules.value = rules.value.map((r) => (r.id === id ? { ...r, enabled: !r.enabled } : r));
}

/** 滚动到规则区（空态动作）。 */
function scrollToRules(): void {
  rulesRef.value?.scrollIntoView({ behavior: 'smooth', block: 'start' });
}

/** 规则区 DOM 引用。 */
const rulesRef = ref<HTMLElement | null>(null);

// ---------------------------------------------------------------------------
// 删除规则（★ 危险操作：二次确认 + 原因必填 + 对象名二次校验）
// ---------------------------------------------------------------------------

/** 删除确认弹窗开关。 */
const deleteOpen = ref<boolean>(false);

/** 删除目标 id（只存 id，不持有对象引用）。 */
const deleteTargetId = ref<string>('');

/** 当前删除目标。 */
const deleteTarget = computed<AlarmRule | null>(() => rules.value.find((r) => r.id === deleteTargetId.value) ?? null);

/** 删除弹窗的对象摘要。 */
const deleteFacts = computed<readonly DangerFact[]>(() => [
  { label: '规则名称', value: deleteTarget.value?.name ?? '—' },
  { label: '条件', value: deleteTarget.value?.condition ?? '—' },
  { label: '持续时间', value: `${deleteTarget.value?.durationSec ?? 0} 秒` },
  { label: '抑制窗口', value: `${deleteTarget.value?.suppressMin ?? 0} 分钟` },
  { label: '历史触发', value: `${deleteTarget.value?.hitCount ?? 0} 次（保留）` },
]);

/** 打开删除确认。 */
function askDelete(rule: AlarmRule): void {
  deleteTargetId.value = rule.id;
  deleteOpen.value = true;
}

/** 关闭删除确认（清目标）。 */
function closeDelete(): void {
  deleteOpen.value = false;
  deleteTargetId.value = '';
}

/** 提交删除（四要素已由 DangerConfirmModal 校验通过）。 */
function submitDelete(payload: { reason: string; note: string }): void {
  if (!deleteTargetId.value) {
    return;
  }
  rules.value = rules.value.filter((r) => r.id !== deleteTargetId.value);
  window.alert(`已删除告警规则。原因：${payload.reason}；说明：${payload.note}`);
  closeDelete();
}

// ---------------------------------------------------------------------------
// 渲染辅助 / 导出 / 导航
// ---------------------------------------------------------------------------

/** 处置状态标签色调。 */
function stateTagClass(state: AlarmState): string {
  if (state === 'resolved') {
    return 'wc-tag--ok';
  }
  if (state === 'acking') {
    return 'wc-tag--info';
  }
  return 'wc-tag--warn';
}

/** 来源类型中文。 */
function sourceTypeLabel(sourceType: string): string {
  const map: Record<string, string> = {
    device: '设备',
    forwarder: '北向出口',
    license: '授权',
    system: '系统',
  };
  return map[sourceType] ?? sourceType;
}

/** 导出当前筛选结果为 CSV（含 BOM，Excel 直接可读）。 */
function exportCsv(): void {
  const header = '级别,最近触发,对象,来源类型,标题,详情,次数,状态,处置人,处置说明';
  const escape = (value: string): string => `"${value.replace(/"/g, '""')}"`;
  const body = filtered.value
    .map((a) =>
      [
        levelTextOf(a.level),
        a.lastSeenAt,
        a.sourceLabel,
        sourceTypeLabel(a.sourceType),
        a.title,
        a.detail,
        String(a.count),
        a.stateLabel,
        a.ackedBy || '-',
        a.note || '-',
      ]
        .map(escape)
        .join(','),
    )
    .join('\n');
  const blob = new Blob([`\uFEFF${header}\n${body}`], { type: 'text/csv;charset=utf-8' });
  const url = URL.createObjectURL(blob);
  const a = document.createElement('a');
  a.href = url;
  a.download = `alarms-${Date.now()}.csv`;
  a.click();
  URL.revokeObjectURL(url);
}

/** 跳转。 */
function go(name: string): void {
  void router.push({ name });
}
</script>

<style scoped>
/* 可解释空态：后端无告警引擎时的「原因 + 恢复路径」块（与 LicensePage 同契约） */
.al-unsupported {
  align-items: flex-start;
  margin: 0 0 12px;
}
.wc-banner__stack {
  display: flex;
  flex-direction: column;
  gap: 4px;
}
.wc-banner__line {
  line-height: 1.6;
}
.al-src {
  margin-left: 6px;
  font-size: 11px;
  color: var(--text-3);
}
.al-title {
  font-weight: 600;
  color: var(--text-1);
}
.al-detail {
  font-size: var(--fs-caption);
  color: var(--text-3);
  max-width: 520px;
  white-space: normal;
  line-height: 1.5;
}
.al-ops {
  display: inline-flex;
  gap: 6px;
  justify-content: flex-end;
}
.al-rule-actions {
  display: flex;
  gap: 8px;
  justify-content: flex-end;
}
.al-error {
  margin: 0;
  font-size: var(--fs-caption);
  color: var(--danger);
}
.al-hint-block {
  border: 1px dashed var(--border);
  border-radius: var(--radius);
  background: var(--bg-app);
}
.wc-table .is-right {
  text-align: right;
}
.al-mask {
  position: fixed;
  inset: 0;
  background: rgba(29, 33, 41, 0.45);
  display: flex;
  justify-content: flex-end;
  z-index: 2000;
}
.al-drawer {
  background: #fff;
  width: 560px;
  max-width: 100%;
  height: 100%;
  display: flex;
  flex-direction: column;
  box-shadow: var(--shadow);
}
.al-drawer__head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 14px 20px;
  border-bottom: 1px solid var(--divider);
}
.al-drawer__head h3 {
  margin: 0;
  font-size: 16px;
  font-weight: 600;
}
.al-drawer__body {
  padding: 20px;
  overflow: auto;
  display: flex;
  flex-direction: column;
  gap: 16px;
}
.al-drawer__ops {
  display: flex;
  gap: 8px;
  justify-content: flex-end;
}
</style>
