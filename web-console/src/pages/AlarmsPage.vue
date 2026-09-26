<template>
  <!--
    AlarmsPage —— 告警中心（监控分组第 3 页，路由 `/alarms`）。

    结构（对齐原型 `alarms` :1474-1505 + 需求 7「删掉菜单顶部内容框」）：
      ① KPI 卡片行（严重 / 警告 / 告警总数 / 已确认·已恢复）
      ② 告警列表卡：卡头工具条（导出 CSV / 告警规则 / 批量确认当前页）+ 筛选行 + 分页表
      ③ 告警详情抽屉
      ④ 告警规则弹窗（表单 + 已保存规则；危险操作走 DangerConfirmModal 四要素）

    硬性约定遵守情况：
      · 无页面级标题块 / 无说明性文案块；一切数值来自真实接口（GET /api/alerts）；
      · 列表页必须分页，且条数只有 UiPager 一个口径（表格无 footer「共 N 条」）；
      · 空态给下一步动作（EmptyState 一句话说明 + 一个动作按钮）；
      · 危险操作（删除规则）= 二次确认 + 原因必填 + 对象名二次校验；
      · 规则读取 / 保存 / 启停 / 删除命中不支持端点时**原样呈现真实原因**，
        绝不静默吞错、绝不伪造「已保存 / 已删除」；
      · 无解绑 / 重置试用 / revoke 任何入口；无 emoji。
  -->
  <div class="wc-content">
    <!-- KPI：数值由当前告警快照实时推导 -->
    <div class="wc-grid wc-grid--4">
      <StatCard
        label="严重"
        :value="String(kpi.critical)"
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
      <StatCard label="告警总数" :value="String(kpi.total)" sub="当前列表全部" icon-tone="ink">
        <template #icon>◷</template>
      </StatCard>
      <StatCard label="已确认 / 已恢复" :value="String(kpi.closed)" :sub="`待处理 ${kpi.open} 条`" tone="ok" icon-tone="teal">
        <template #icon>✓</template>
      </StatCard>
    </div>

    <!-- 告警列表卡：工具条 + 搜索 + 列表 -->
    <section class="wc-card">
      <div class="wc-card__head">
        <h3>告警列表</h3>
        <span v-if="selectedIds.length > 0" class="wc-tag wc-tag--info">已选 {{ selectedIds.length }} 条</span>
        <div class="wc-card__ops">
          <button type="button" class="wc-btn wc-btn--sm" data-testid="alarms-export" @click="exportCsv">导出 CSV</button>
          <button type="button" class="wc-btn wc-btn--sm" data-testid="alarms-rules-open" @click="openRules">
            告警规则
          </button>
          <RoleGate :allowed="canAck" mode="disable" deny-text="当前角色只读，无「批量确认」权限">
            <button
              type="button"
              class="wc-btn wc-btn--sm wc-btn--primary"
              data-testid="alarms-batch-ack"
              @click="batchAck"
            >
              批量确认当前页
            </button>
          </RoleGate>
        </div>
      </div>

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

        <!-- 后端无告警引擎 / 请求失败 → 原样呈现真实原因（绝不伪造告警） -->
        <p v-if="alertsApiNotice" class="al-notice" role="alert" data-testid="alerts-unsupported">
          <span class="al-notice__text">{{ alertsApiNotice }}</span>
          <button type="button" class="wc-btn wc-btn--sm" data-testid="alerts-retry" @click="loadAlarms">重试</button>
        </p>

        <!-- 单条确认失败的真实原因（后端 400/5xx 原文，绝不静默吞错） -->
        <p v-if="ackNotice" class="al-notice" role="alert" data-testid="ack-notice">
          <span class="al-notice__text">{{ ackNotice }}</span>
        </p>

        <EmptyState
          v-if="filtered.length === 0"
          :title="alertsUnsupported ? '后端暂无告警数据' : '没有符合条件的告警'"
          :desc="alertsUnsupported ? '告警引擎落地后本页自动展示。' : '放宽级别 / 状态 / 时间范围，或清空关键字。'"
        >
          <template #actions>
            <button v-if="!alertsUnsupported" type="button" class="wc-btn" @click="resetFilters">清空筛选</button>
            <button type="button" class="wc-btn wc-btn--primary" data-testid="alarms-rules-open-empty" @click="openRules">
              配置告警规则
            </button>
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
      </div>
    </section>

    <!-- 告警规则弹窗（表单 + 已保存规则；按钮触发，非常驻区块） -->
    <Teleport to="body">
      <div v-if="rulesOpen" class="al-modal__mask" @click.self="closeRules">
        <div class="al-modal" role="dialog" aria-modal="true" aria-label="告警规则" data-testid="rules-modal">
          <div class="al-modal__head">
            <h3>告警规则</h3>
            <button type="button" class="wc-btn wc-btn--sm" data-testid="rules-close" @click="closeRules">关闭</button>
          </div>

          <div class="al-modal__body">
            <div class="wc-grid wc-grid--3">
              <UiField label="阈值条件" required>
                <UiInput v-model="ruleDraft.threshold" placeholder="[T_Barrel1] > 240" />
              </UiField>
              <UiField label="持续时间（秒）" required>
                <UiInput v-model="ruleDraft.durationSec" type="number" placeholder="10" />
              </UiField>
              <UiField label="抑制窗口（分钟）" required>
                <UiInput v-model="ruleDraft.suppressMin" type="number" placeholder="5" />
              </UiField>
            </div>

            <UiField label="适用对象" required>
              <UiSelect v-model="ruleDraft.target" :options="targetOptions" />
            </UiField>

            <p v-if="ruleTouched && ruleError" class="al-error" role="alert">{{ ruleError }}</p>
            <p v-if="ruleMessage" class="al-notice" role="alert" data-testid="rule-message">
              <span class="al-notice__text">{{ ruleMessage }}</span>
            </p>
            <p v-if="rules.length === 0 && rulesNotice" class="al-notice" role="alert" data-testid="rules-source">
              <span class="al-notice__text">{{ rulesNotice }}</span>
            </p>

            <!-- 已保存规则 -->
            <div v-if="rules.length > 0" class="wc-table-wrap">
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
                          <button type="button" class="wc-btn wc-btn--sm" @click="toggleRule">
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
          </div>

          <div class="al-modal__foot">
            <button type="button" class="wc-btn" data-testid="rule-reset" @click="resetRuleDraft">重置</button>
            <button
              type="button"
              class="wc-btn wc-btn--primary"
              data-testid="rule-save"
              :disabled="ruleError.length > 0 || !canEditRules"
              @click="saveRule"
            >
              保存规则
            </button>
          </div>
        </div>
      </div>
    </Teleport>

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

    <!-- 批量确认弹窗：展示影响条数，用户确认后逐条下发；失败聚合展示真实原因（禁静默吞错） -->
    <Teleport to="body">
      <div v-if="batchOpen" class="al-modal__mask" @click.self="closeBatchAck">
        <div class="al-modal" role="dialog" aria-modal="true" aria-label="批量确认告警" data-testid="batch-ack-modal">
          <div class="al-modal__head">
            <h3>批量确认告警</h3>
            <button type="button" class="wc-btn wc-btn--sm" data-testid="batch-ack-close" @click="closeBatchAck">关闭</button>
          </div>
          <div class="al-modal__body">
            <p v-if="batchTargets.length === 0">当前页没有「待处理」的告警。</p>
            <template v-else>
              <p>将确认当前页 <b>{{ batchTargets.length }}</b> 条「待处理」告警，并写入审计日志。</p>
              <p v-if="batchMessage" class="al-notice" role="alert" data-testid="batch-ack-result">
                <span class="al-notice__text">{{ batchMessage }}</span>
              </p>
              <ul v-if="batchFailures.length > 0" class="al-error" role="alert" data-testid="batch-ack-failures">
                <li v-for="f in batchFailures" :key="f.id">{{ f.id }}：{{ f.reason }}</li>
              </ul>
            </template>
          </div>
          <div class="al-modal__foot">
            <button type="button" class="wc-btn" data-testid="batch-ack-cancel" :disabled="batchRunning" @click="closeBatchAck">
              取消
            </button>
            <button
              type="button"
              class="wc-btn wc-btn--primary"
              data-testid="batch-ack-submit"
              :disabled="batchRunning || batchTargets.length === 0"
              @click="runBatchAck"
            >
              {{ batchRunning ? '下发中…' : '确认下发' }}
            </button>
          </div>
        </div>
      </div>
    </Teleport>

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

            <UiField label="处置说明" required>
              <UiInput v-model="ackNote" placeholder="如：已联系电气班检查通讯线" />
            </UiField>

            <!-- 处置失败的真实原因（后端 400 confirm_mismatch / reason 校验原文） -->
            <p v-if="detailAckError" class="al-error" role="alert" data-testid="detail-ack-error">{{ detailAckError }}</p>

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
  </div>
</template>

<script setup lang="ts">
/**
 * @file AlarmsPage.vue
 * @module web-console/pages/AlarmsPage
 * @description 告警中心：列表（分页）+ 级别/状态/时间/关键字筛选 + 详情 + 规则弹窗。
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
import { dataVersion, refreshAlerts, repo, type AlarmLevel, type AlarmRecord, type AlarmState } from '@/api/repo';
import { apiRequest, ApiError } from '@/api/client';
import { session } from '../store/session';

const router = useRouter();

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
 * 告警数据源不可得 / 不支持的说明（`GET /api/alerts` 的 `reason` 原文 / 真实失败原因）。
 *
 * 非空即表示「后端当前没有真实告警数据源」，页面据此给出诚实空态。
 */
const alertsApiNotice = ref('');

/** 后端无告警数据源 → 展示诚实空态（绝不伪造告警）。 */
const alertsUnsupported = computed<boolean>(() => alarms.value.length === 0 && alertsApiNotice.value !== '');

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

/** 处置状态 → 中文（与数据层口径一致）。 */
const STATE_TEXT: Record<string, string> = { open: '待处理', acking: '已确认', resolved: '已恢复' };

/** `/api/alerts` 条目 → 告警记录（后端当前恒为空数组；落地后按契约宽容映射）。 */
function mapAlarmRow(raw: Record<string, unknown>, idx: number): AlarmRecord {
  const level = pickText(raw, 'level', 'minor');
  const state = pickText(raw, 'state', 'open');
  const levelKey = (['critical', 'major', 'minor', 'warning'].includes(level) ? level : 'minor') as AlarmLevel;
  return {
    id: pickText(raw, 'id', `al-${idx}`),
    level: levelKey,
    levelLabel: levelTextOf(levelKey),
    sourceType: pickText(raw, 'sourceType', pickText(raw, 'source_type', 'system')),
    sourceLabel: pickText(raw, 'sourceLabel', pickText(raw, 'source', '—')),
    title: pickText(raw, 'title', '—'),
    detail: pickText(raw, 'detail', ''),
    firstSeenAt: alarmTimeText(pickText(raw, 'firstSeenAt', pickText(raw, 'first_seen_at', ''))),
    lastSeenAt: alarmTimeText(pickText(raw, 'lastSeenAt', pickText(raw, 'last_seen_at', ''))),
    count: 1,
    state: (['open', 'acking', 'resolved'].includes(state) ? state : 'open') as AlarmState,
    stateLabel: STATE_TEXT[state] ?? state,
    ackedBy: pickText(raw, 'ackedBy', pickText(raw, 'acked_by', '')),
    note: pickText(raw, 'note', ''),
  };
}

/**
 * 加载告警。
 *
 * 以 `GET /api/alerts` 为**唯一**告警数据源（后端无告警引擎 →
 * `{items:[],source:"unsupported",reason}`，页面原样呈现 reason）；接口不可得
 * （网络 / 5xx）时如实说明失败原因，**绝不回退演示数据造假**。
 */
async function loadAlarms(): Promise<void> {
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
    alarms.value = [];
    const code = cause instanceof ApiError ? cause.status : 0;
    alertsApiNotice.value = code === 0 ? 'GET /api/alerts 失败：网关不可达。' : `GET /api/alerts 失败：HTTP ${code}。`;
  }
}

onMounted(() => {
  void loadAlarms();
});

/**
 * 缓存填充完成（dataVersion 自增）→ 重新请求 `GET /api/alerts`
 * （后端告警引擎落地后自动生效，绝不回退演示数据）。
 */
watch(dataVersion, () => {
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
 * `/api/alerts` 的时间戳（后端 `first_seen_at` / `last_seen_at` 为 13 位毫秒
 * epoch 字符串，守大数红线按字符串传递）→ `YYYY-MM-DD HH:mm:ss` 展示串。
 *
 * 必须格式化后再比较：`new Date('1790900000000')` 在 V8 下是 **Invalid Date**
 * （纯数字串被当成年份无法解析），实测 `getTime()` 返回 `NaN`；而未格式化的
 * 毫秒串再和 `YYYY-MM-DD HH:mm:ss` 做字典序比较恒为 `true`（首字符 `'1' < '2'`），
 * 会把全部记录误剔除。两侧都先过本函数，同一个格式才能比。
 * 只认 10 位秒 / 13 位毫秒，其余原样透传（与 `repo.ts:formatEpochText` 同口径）。
 */
function alarmTimeText(value: string): string {
  const trimmed = value.trim();
  if (/^\d{10}$/.test(trimmed)) {
    return epochToText(Number(trimmed) * 1000);
  }
  if (/^\d{13}$/.test(trimmed)) {
    return epochToText(Number(trimmed));
  }
  return trimmed || '—';
}

/** epoch 毫秒 → `YYYY-MM-DD HH:mm:ss`（走**本地**时区 getters，与展示一致）。 */
function epochToText(ms: number): string {
  const d = new Date(ms);
  if (Number.isNaN(d.getTime())) {
    return '—';
  }
  const p = (n: number): string => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

/**
 * 时间范围 → 起始时刻（`YYYY-MM-DD HH:mm:ss`）。
 *
 * 以数据集内的**最大时间戳**为「现在」（而非浏览器本地时间）—— 网关与浏览器
 * 时钟可能存在偏差，用数据自身基准可避免把全部记录过滤掉。
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
  const base = new Date(latest.replace(' ', 'T'));
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

/** 批量确认弹窗开关。 */
const batchOpen = ref<boolean>(false);

/** 批量确认下发中（防重复提交）。 */
const batchRunning = ref<boolean>(false);

/** 批量确认结果摘要（空 = 无）。 */
const batchMessage = ref<string>('');

/** 批量确认失败明细（id + 后端真实原因；聚合展示，禁静默吞错）。 */
const batchFailures = ref<readonly { id: string; reason: string }[]>([]);

/** 批量确认目标快照（打开弹窗时定格当前页「待处理」，避免翻页 / 筛选漂移）。 */
const batchTargets = ref<AlarmRecord[]>([]);

/** 打开批量确认弹窗（弹窗内展示影响条数，确认后才逐条下发）。 */
function batchAck(): void {
  batchTargets.value = paged.value.items.filter((a) => a.state === 'open');
  batchMessage.value = '';
  batchFailures.value = [];
  batchOpen.value = true;
}

/** 关闭批量确认弹窗（下发中不允许关闭）。 */
function closeBatchAck(): void {
  if (batchRunning.value) {
    return;
  }
  batchOpen.value = false;
  batchMessage.value = '';
  batchFailures.value = [];
}

/**
 * 批量确认：逐条下发四要素 `{state, note, reason, confirm}`（confirm = 告警 id 原文，
 * 后端大小写不敏感精确匹配、空串即 mismatch）；失败**聚合展示真实原因**，绝不静默吞错。
 * 批量走 `{ refetch: false }`（N 条 = N 次 POST），结束后统一 `refreshAlerts()` 一次重取
 * （fetch + bump → `watch(dataVersion)` 自动刷新本页，恰好 1 次清单 GET）。
 */
async function runBatchAck(): Promise<void> {
  if (batchRunning.value || batchTargets.value.length === 0) {
    return;
  }
  batchRunning.value = true;
  batchMessage.value = '';
  batchFailures.value = [];
  const failures: { id: string; reason: string }[] = [];
  let okCount = 0;
  for (const alarm of batchTargets.value) {
    const result = await repo.resolveAlarm(
      {
        id: alarm.id,
        state: 'acking',
        note: '批量确认（告警中心）',
        reason: '批量确认（告警中心）',
        confirm: alarm.id,
        actor: session.state.displayName,
      },
      { refetch: false },
    );
    if (result.ok) {
      okCount += 1;
    } else {
      failures.push({ id: alarm.id, reason: result.message });
    }
  }
  batchRunning.value = false;
  await refreshAlerts();
  if (failures.length === 0) {
    batchOpen.value = false;
    selectedIds.value = [];
    return;
  }
  batchFailures.value = failures;
  batchMessage.value = `下发完成：成功 ${okCount} 条，失败 ${failures.length} 条（真实原因见明细）。`;
}

/** 单条确认失败的真实原因（空 = 无）。 */
const ackNotice = ref<string>('');

/** 单条确认（四要素下发；失败在列表卡头展示真实原因）。 */
async function confirmAlarm(id: string): Promise<void> {
  ackNotice.value = '';
  const result = await repo.resolveAlarm({
    id,
    state: 'acking',
    note: '确认告警（告警中心）',
    reason: '确认告警（告警中心）',
    confirm: id,
    actor: session.state.displayName,
  });
  if (!result.ok) {
    ackNotice.value = `确认告警 ${id} 失败：${result.message}`;
    return;
  }
  // 成功刷新交给 repo 默认 refetch + bumpCacheVersion → watch(dataVersion) 自动重取，
  // 页面不再显式重复 GET。
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

/** 详情处置失败的真实原因（空 = 无）。 */
const detailAckError = ref<string>('');

/** 打开详情（重置处置草稿）。 */
function openDetail(id: string): void {
  detailId.value = id;
  ackNote.value = '';
  detailAckError.value = '';
  detailOpen.value = true;
}

/** 关闭详情（清草稿）。 */
function closeDetail(): void {
  detailOpen.value = false;
  detailId.value = '';
  ackNote.value = '';
  detailAckError.value = '';
}

/** 从详情处置（四要素下发；confirm = 告警 id 原文；失败就地展示真实原因，不关抽屉）。 */
async function ackFromDetail(): Promise<void> {
  if (!detailId.value || ackNote.value.trim().length === 0) {
    return;
  }
  detailAckError.value = '';
  const result = await repo.resolveAlarm({
    id: detailId.value,
    state: 'acking',
    note: ackNote.value.trim(),
    reason: '详情处置（告警中心）',
    confirm: detailId.value,
    actor: session.state.displayName,
  });
  if (!result.ok) {
    detailAckError.value = `处置失败：${result.message}`;
    return;
  }
  closeDetail();
  // 成功刷新交给 repo 默认 refetch + bumpCacheVersion → watch(dataVersion) 自动重取，
  // 页面不再显式重复 GET。
}

// ---------------------------------------------------------------------------
// 规则（阈值 / 持续时间 / 抑制）—— 按钮 + 弹窗
// ---------------------------------------------------------------------------

/** 告警规则（来自后端 `GET /api/alerts/rules`）。 */
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
  readonly hitCount: number;
  /** 是否启用 */
  enabled: boolean;
}

/** 规则表。 */
const rules = ref<AlarmRule[]>([]);

/** 规则弹窗开关。 */
const rulesOpen = ref<boolean>(false);

/** 规则清单来源说明（读取接口不可得时的真实原因）。 */
const rulesNotice = ref('');

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

/** 规则草稿是否已被用户编辑（编辑前不展示必填错误，避免一打开就报红）。 */
const ruleTouched = ref(false);

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

/** 用户一旦编辑草稿即进入「已触碰」态（此后才展示必填 / 格式错误）。 */
watch(ruleDraft, () => {
  ruleTouched.value = true;
});

/** 重置规则草稿。 */
function resetRuleDraft(): void {
  ruleDraft.threshold = '';
  ruleDraft.durationSec = '';
  ruleDraft.suppressMin = '';
  ruleDraft.target = '';
  ruleTouched.value = false;
}

/** 规则操作结果提示（后端 501 / 403 等结构化结果原样写在这里）。 */
const ruleMessage = ref('');

/** `/api/alerts/rules` 条目 → 规则（宽容映射，后端契约落地后自动生效）。 */
function mapRuleRow(raw: Record<string, unknown>, idx: number): AlarmRule {
  const condition = pickText(raw, 'condition', pickText(raw, 'threshold', '—'));
  return {
    id: pickText(raw, 'id', `rule-${idx}`),
    name: pickText(raw, 'name', condition),
    condition,
    durationSec: Number(pickText(raw, 'duration_sec', pickText(raw, 'durationSec', '0'))) || 0,
    suppressMin: Number(pickText(raw, 'suppress_min', pickText(raw, 'suppressMin', '0'))) || 0,
    hitCount: Number(pickText(raw, 'hit_count', pickText(raw, 'hitCount', '0'))) || 0,
    enabled: raw['enabled'] !== false,
  };
}

/**
 * 加载告警规则清单（惰性 —— 仅在打开规则弹窗时触发，避免无谓请求）。
 *
 * 后端当前未提供规则读接口 → 原样呈现失败原因，**绝不伪造规则**。
 */
async function loadRules(): Promise<void> {
  try {
    const raw = await apiRequest<Record<string, unknown>>('/api/alerts/rules');
    const items = Array.isArray(raw['items']) ? raw['items'] : Array.isArray(raw['rules']) ? raw['rules'] : [];
    rules.value = items
      .map((row, i) => (row !== null && typeof row === 'object' && !Array.isArray(row) ? mapRuleRow(row as Record<string, unknown>, i) : null))
      .filter((r): r is AlarmRule => r !== null);
    rulesNotice.value = raw['source'] === 'unsupported' ? pickText(raw, 'reason', '后端告警规则引擎未落地，无规则清单') : '';
  } catch (cause) {
    rules.value = [];
    const code = cause instanceof ApiError ? cause.status : 0;
    rulesNotice.value = code === 0 ? '告警规则清单读取失败：网关不可达。' : `告警规则清单读取失败：HTTP ${code}（后端未提供告警规则读接口）。`;
  }
}

/** 打开规则弹窗（顺带拉取规则清单）。 */
function openRules(): void {
  rulesOpen.value = true;
  void loadRules();
}

/** 关闭规则弹窗（清提示，不清规则清单）。 */
function closeRules(): void {
  rulesOpen.value = false;
  ruleMessage.value = '';
}

/** 告警规则写失败的「真实原因」。 */
function ruleWriteFailureText(cause: unknown): string {
  const code = cause instanceof ApiError ? cause.status : 0;
  if (code === 501) {
    return '规则未保存：后端告警引擎未落地（HTTP 501 not_implemented）。';
  }
  if (code === 403) {
    return '规则未保存：当前账号无 device.write 权限（HTTP 403）。';
  }
  if (code === 0) {
    return '规则未保存：网关不可达（网络层失败）。';
  }
  return `规则未保存：HTTP ${code}。`;
}

/**
 * 保存规则。
 *
 * `PUT /api/alerts/rules` —— 后端当前返回 501，页面**原样呈现真实原因**，
 * 绝不把 501 吞成「已保存」。
 */
async function saveRule(): Promise<void> {
  if (ruleError.value.length > 0 || !canEditRules.value) {
    return;
  }
  const targetName = ruleDraft.target ? repo.getDevice(ruleDraft.target)?.name ?? '指定设备' : '全部设备';
  const condition = ruleDraft.threshold.trim();
  ruleMessage.value = '';
  try {
    await apiRequest<unknown>('/api/alerts/rules', {
      method: 'PUT',
      body: JSON.stringify({
        rules: [
          {
            name: `${targetName} · ${condition}`,
            condition,
            duration_sec: Number(ruleDraft.durationSec),
            suppress_min: Number(ruleDraft.suppressMin),
            target: ruleDraft.target,
          },
        ],
      }),
    });
    ruleMessage.value = `规则「${targetName} · ${condition}」已保存。`;
    resetRuleDraft();
    await loadRules();
  } catch (cause) {
    ruleMessage.value = ruleWriteFailureText(cause);
  }
}

/** 启用 / 停用规则（后端无写接口 → 原样呈现真实原因，不改本地状态）。 */
function toggleRule(): void {
  ruleMessage.value = '规则启停未生效：后端未提供规则状态写接口（告警引擎未落地）。';
}

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

/** 提交删除（四要素已由 DangerConfirmModal 校验通过；后端无删除接口 → 诚实告知未生效）。 */
function submitDelete(): void {
  if (!deleteTargetId.value) {
    return;
  }
  ruleMessage.value = `删除规则「${deleteTarget.value?.name ?? ''}」未生效：后端未提供删除告警规则接口（告警引擎未落地）。`;
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
/* 真实原因 / 结果提示（紧凑单行，非解释性文案块） */
.al-notice {
  margin: 0;
  display: flex;
  align-items: center;
  gap: 10px;
  font-size: var(--fs-caption);
  color: var(--text-2);
}
.al-notice__text {
  min-width: 0;
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
.al-error {
  margin: 0;
  font-size: var(--fs-caption);
  color: var(--danger);
}
.wc-table .is-right {
  text-align: right;
}

/* ---------- 规则弹窗（自持遮罩 + 面板，与 DangerConfirmModal 同一视觉语言） ---------- */
.al-modal__mask {
  position: fixed;
  inset: 0;
  background: var(--mask);
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 2010;
  padding: 24px;
}
.al-modal {
  background: var(--bg-card);
  border-radius: var(--radius);
  box-shadow: var(--shadow);
  width: 720px;
  max-width: 100%;
  max-height: 88vh;
  display: flex;
  flex-direction: column;
}
.al-modal__head {
  padding: 14px 20px;
  border-bottom: 1px solid var(--divider);
  display: flex;
  align-items: center;
  justify-content: space-between;
}
.al-modal__head h3 {
  margin: 0;
  font-size: 16px;
  font-weight: 600;
}
.al-modal__body {
  padding: 20px;
  overflow: auto;
  display: flex;
  flex-direction: column;
  gap: 14px;
}
.al-modal__foot {
  padding: 14px 20px;
  border-top: 1px solid var(--divider);
  display: flex;
  justify-content: flex-end;
  gap: 8px;
}

/* ---------- 告警详情抽屉 ---------- */
.al-mask {
  position: fixed;
  inset: 0;
  background: var(--mask);
  display: flex;
  justify-content: flex-end;
  z-index: 2000;
}
.al-drawer {
  background: var(--bg-card);
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
