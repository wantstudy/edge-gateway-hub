<template>
  <!--
    UpdatesPage —— 系统更新（页面清单第 12 项，仅「系统」角色可见）。
    功能：网关 OTA 升级包仓库的查看 / 上传（签名落库为草稿）/ 发布 / 停用。
    安全契约（licensing-server `POST /admin/updates*`，仅 system）：
      · 上传 / 发布 / 停用均为**高危操作**：原因（枚举必填）+ 补充说明（≥10 字）+
        对象名二次确认（`confirm` = 版本号，大小写不敏感精确匹配）三个字段**彼此独立**；
      · `note`（发布说明）与 `note_detail`（危险操作补充说明）绝不相同、绝不拼接。
  -->
  <PageHeader
    crumb="系统 / 系统更新"
    title="系统更新"
    desc="网关 OTA 升级包仓库。上传并签名后落库为草稿，需再「发布」才对网关可见；网关按 manifest 的版本号决定是否升级。"
  >
    <template #actions>
      <button type="button" class="ac-btn" :disabled="loading" @click="reload">刷新</button>
      <RoleGate :allowed="canPublish" mode="disable" deny-text="当前角色无「系统更新发布」权限">
        <button type="button" class="ac-btn ac-btn--primary" @click="openUpload">上传升级包</button>
      </RoleGate>
    </template>
  </PageHeader>

  <div class="ac-content">
    <!-- 筛选 -->
    <div class="ac-card">
      <div class="ac-card__body">
        <div class="ac-filters">
          <div class="ac-filters__item">
            <label for="u-status">状态</label>
            <UiSelect v-model="filters.status" :options="statusOptions" />
          </div>
          <div class="ac-filters__item ac-filters__grow">
            <label for="u-keyword">版本号 / 签名密钥 / 说明</label>
            <UiInput v-model="filters.keyword" placeholder="输入版本号、kid 或说明片段" />
          </div>
        </div>
      </div>
    </div>

    <!-- 升级包列表 -->
    <section class="ac-card">
      <div class="ac-card__head">
        <h3>升级包列表</h3>
        <span class="ac-card__sub">共 {{ filtered.length }} 个</span>
      </div>

      <p v-if="loading" class="ac-note">正在加载升级包列表…</p>

      <EmptyState
        v-else-if="filtered.length === 0"
        :title="emptyTitle"
        :desc="emptyDesc"
      >
        <template #actions>
          <button type="button" class="ac-btn" :disabled="loading" @click="reload">刷新</button>
        </template>
      </EmptyState>

      <UiTable v-else :columns="columns" :rows="filtered" row-key-field="version">
        <template #cell-version="{ row }">
          <span class="ac-mono">v{{ row.version }}</span>
        </template>
        <template #cell-size="{ row }">
          <span class="ac-mono">{{ row.size === '' ? '—' : row.size }}</span>
        </template>
        <template #cell-payloadSha256="{ row }">
          <span class="ac-mono" :title="row.payloadSha256">{{ shortSha(row.payloadSha256) }}</span>
        </template>
        <template #cell-status="{ row }">
          <StatusTag :status="otaStatus(row.status).toneKey" :text="otaStatus(row.status).label" />
        </template>
        <template #cell-publishedAt="{ row }">
          <span class="ac-mono">{{ row.publishedAt }}</span>
        </template>
        <template #cell-note="{ row }">
          {{ row.note || '—' }}
        </template>
        <template #actions="{ row }">
          <template v-if="canPublish && (row.status === 'draft' || row.status === 'disabled')">
            <button type="button" class="ac-btn ac-btn--sm ac-btn--primary" @click="openPublish(row)">发布</button>
          </template>
          <template v-else-if="canPublish && row.status === 'published'">
            <button type="button" class="ac-btn ac-btn--sm" @click="openDisable(row)">停用</button>
          </template>
          <span v-else class="ac-note">—</span>
        </template>
      </UiTable>
    </section>
  </div>

  <!-- 上传对话框（文件读取为 base64 → payload_b64；确认步走 DangerConfirmModal） -->
  <Teleport to="body">
    <div v-if="uploadOpen" class="ac-modal-mask" @click.self="closeUpload">
      <div class="ac-modal" role="dialog" aria-modal="true" aria-label="上传升级包">
        <div class="ac-modal__head"><h3>上传升级包</h3></div>
        <div class="ac-modal__body">
          <div class="ac-grid ac-grid--2">
            <UiField label="版本号" required hint="u64 单调序十进制串，需大于当前已发布版本">
              <UiInput v-model="uploadForm.version" placeholder="如 152" />
            </UiField>
            <UiField label="通道" required>
              <UiSelect v-model="uploadForm.channel" :options="channelOptions" />
            </UiField>
          </div>

          <UiField
            label="升级包文件"
            required
            hint="选择后将读取为 base64 作为 payload_b64 上传；SHA-256 与包体大小由服务端核算"
            :error="uploadForm.readError"
            full
          >
            <input ref="fileInputEl" type="file" class="ac-file" @change="onFileChange" />
            <p v-if="uploadForm.fileName" class="ac-note">
              已选：{{ uploadForm.fileName }}（{{ uploadForm.fileSize }} 字节）
              <template v-if="uploadForm.reading">· 读取中…</template>
              <template v-else-if="uploadForm.payloadB64">· 已就绪</template>
            </p>
          </UiField>

          <UiField label="发布说明" hint="面向本版本的内容概述（可选，将随版本展示）" full>
            <UiTextarea v-model="uploadForm.note" :rows="2" placeholder="例如：修复采集断连后重连抖动" />
          </UiField>

          <p class="ac-note">
            上传为高危操作：下一步需填写操作原因与补充说明，并输入版本号二次确认（记入审计）。
          </p>
        </div>
        <div class="ac-modal__foot">
          <button type="button" class="ac-btn" @click="closeUpload">取消</button>
          <button
            type="button"
            class="ac-btn ac-btn--primary"
            :disabled="!canSubmitUpload"
            @click="uploadConfirmOpen = true"
          >
            下一步：确认上传
          </button>
        </div>
      </div>
    </div>
  </Teleport>

  <!-- 上传二次确认（四要素） -->
  <DangerConfirmModal
    :open="uploadConfirmOpen"
    :title="`确认上传并签名 v${uploadForm.version.trim()}`"
    :impacts="[
      `将读取所选文件并以其 base64 作为包体上传，服务端核算 SHA-256 后落库为「草稿」（网关此时不可见）。`,
      '需再执行「发布」后该版本才对网关可见。',
      '上传与签名会写入审计日志（含操作原因与补充说明）。',
    ]"
    :facts="uploadFacts"
    :reasons="UPLOAD_REASONS"
    :confirm-value="uploadForm.version.trim()"
    confirm-mode="full"
    confirm-label="风险二次确认（请输入版本号）"
    confirm-placeholder="输入版本号（如 152）"
    confirm-text="确认上传"
    @close="uploadConfirmOpen = false"
    @submit="submitUpload"
  />

  <!-- 发布二次确认（四要素） -->
  <DangerConfirmModal
    :open="publishOpen"
    :title="`确认发布 v${publishTarget?.version ?? ''}（${publishTarget?.channel ?? ''}）`"
    :impacts="[
      `版本 v${publishTarget?.version ?? ''} 状态将改为「已发布」，成为该通道 ${publishTarget?.channel ?? ''} 下网关可拉取的目标（网关依据 manifest 版本号决定是否升级）。`,
      '本操作可逆：如需撤回可再执行「停用」。',
    ]"
    :facts="statusFacts(publishTarget)"
    :reasons="PUBLISH_REASONS"
    :confirm-value="publishTarget?.version ?? ''"
    confirm-mode="full"
    confirm-label="风险二次确认（请输入版本号）"
    confirm-placeholder="输入版本号"
    confirm-text="确认发布"
    @close="publishOpen = false"
    @submit="submitPublish"
  />

  <!-- 停用二次确认（四要素） -->
  <DangerConfirmModal
    :open="disableOpen"
    :title="`确认停用 v${disableTarget?.version ?? ''}（${disableTarget?.channel ?? ''}）`"
    :impacts="[
      `版本 v${disableTarget?.version ?? ''} 状态将改为「已停用」，网关不再将其作为可升级目标。`,
      '已升级到该版本的网关不会自动降级；如需修复请发布更高版本。',
      '本操作可逆：可再次「发布」恢复。',
    ]"
    :facts="statusFacts(disableTarget)"
    :reasons="DISABLE_REASONS"
    :confirm-value="disableTarget?.version ?? ''"
    confirm-mode="full"
    confirm-label="风险二次确认（请输入版本号）"
    confirm-placeholder="输入版本号"
    confirm-text="确认停用"
    @close="disableOpen = false"
    @submit="submitDisable"
  />
</template>

<script setup lang="ts">
/**
 * @file UpdatesPage.vue
 * @module admin-console/pages/UpdatesPage
 * @description 系统更新（OTA 升级包仓库）页，仅 system 角色。
 */
import { computed, onMounted, reactive, ref } from 'vue';
import {
  PageHeader,
  UiField,
  UiInput,
  UiSelect,
  UiTextarea,
  UiTable,
  StatusTag,
  EmptyState,
  RoleGate,
  DangerConfirmModal,
  can,
  type SelectOption,
  type TableColumn,
} from '@ui-kit';
import { repo, API_MODE, DEFAULT_ACTOR, updatesLoadError, type OtaPackageRecord } from '../api/repo';
import { session } from '../store/session';

const isReal = API_MODE === 'real';

/** 是否有发布 / 上传 / 停用权限（仅 system）。 */
const canPublish = computed(() => can(session.state.role, 'update.publish'));

/** 加载态。 */
const loading = ref(false);
/** 升级包记录。 */
const records = ref<OtaPackageRecord[]>([]);

/** 拉取列表（real 失败时 repo 会把真实原因写入 updatesLoadError）。 */
async function reload(): Promise<void> {
  loading.value = true;
  try {
    records.value = await repo.listUpdates();
  } finally {
    loading.value = false;
  }
}

onMounted(() => {
  void reload();
});

/** 筛选草稿。 */
const filters = reactive({ status: '', keyword: '' });

/** 状态筛选选项。 */
const statusOptions: readonly SelectOption[] = [
  { value: '', label: '全部状态' },
  { value: 'draft', label: '草稿' },
  { value: 'published', label: '已发布' },
  { value: 'disabled', label: '已停用' },
  { value: 'revoked', label: '已撤销' },
];

/** 通道选项（后端闭集 OTA_CHANNELS = stable / beta）。 */
const channelOptions: readonly SelectOption[] = [
  { value: 'stable', label: 'stable（稳定）' },
  { value: 'beta', label: 'beta（测试）' },
];

/** 过滤后的列表。 */
const filtered = computed(() => {
  const kw = filters.keyword.trim().toLowerCase();
  return records.value.filter((r) => {
    if (filters.status && r.status !== filters.status) {
      return false;
    }
    if (kw) {
      const hay = `${r.version} ${r.channel} ${r.kid} ${r.note} ${r.publishedBy}`.toLowerCase();
      if (!hay.includes(kw)) {
        return false;
      }
    }
    return true;
  });
});

/** 列定义。 */
const columns: readonly TableColumn[] = [
  { key: 'version', label: '版本号', mono: true },
  { key: 'channel', label: '通道' },
  { key: 'size', label: '包体大小（字节）' },
  { key: 'payloadSha256', label: 'SHA-256', mono: true },
  { key: 'status', label: '状态' },
  { key: 'publishedAt', label: '发布时间', mono: true },
  { key: 'publishedBy', label: '发布人' },
  { key: 'note', label: '说明' },
];

/** 空态标题（按是否处于筛选态区分）。 */
const emptyTitle = computed(() =>
  filters.status || filters.keyword.trim() ? '没有符合条件的升级包' : '暂无升级包',
);

/**
 * 空态说明：real 模式拿不到数据时给出**真实原因**，绝不回退演示数据、绝不伪造成功。
 */
const emptyDesc = computed(() => {
  if (isReal) {
    return updatesLoadError.value
      ? `未能从后端获取升级包列表：${updatesLoadError.value}`
      : '后端当前没有可展示的升级包（可能尚未上传过任何版本，或筛选条件过窄）。';
  }
  return '当前没有任何升级包记录（演示模式）。';
});

/** SHA-256 截断展示（不伪造内容，仅截断原文）。 */
function shortSha(sha: string): string {
  if (!sha || sha === '—') {
    return '—';
  }
  return sha.length > 16 ? `${sha.slice(0, 16)}…` : sha;
}

/**
 * OTA 状态 → (StatusTag 色调键, 中文文案)。
 *
 * 说明：后端 OTA 状态为闭集 `draft / published / disabled / revoked`，
 * 尚未进入 ui-kit `STATUS_MAP`；此处借用既有色调键并显式覆盖文案
 * （`StatusTag` 官方 `text` 覆盖能力），避免页面自造颜色，保证「颜色不是唯一信号」。
 */
const OTA_STATUS: Readonly<Record<string, { toneKey: string; label: string }>> = {
  draft: { toneKey: 'inactive', label: '草稿' },
  published: { toneKey: 'active', label: '已发布' },
  disabled: { toneKey: 'key_disabled', label: '已停用' },
  revoked: { toneKey: 'revoked', label: '已撤销' },
};

function otaStatus(status: string): { toneKey: string; label: string } {
  return OTA_STATUS[status] ?? { toneKey: status, label: status || '—' };
}

// ---------------- 上传 ----------------
/** 上传对话框开关。 */
const uploadOpen = ref(false);
/** 上传二次确认开关。 */
const uploadConfirmOpen = ref(false);
/** 原生文件输入引用。 */
const fileInputEl = ref<HTMLInputElement | null>(null);

/** 上传表单草稿（打开时初始化，关闭时清空）。 */
const uploadForm = reactive({
  version: '',
  channel: 'stable',
  note: '',
  fileName: '',
  fileSize: 0,
  payloadB64: '',
  reading: false,
  readError: '',
});

/** 上传可选原因。 */
const UPLOAD_REASONS: readonly string[] = ['版本发布新增功能', '修复线上缺陷', '安全补丁', '合规要求', '其他'];

/** 打开上传对话框：初始化草稿。 */
function openUpload(): void {
  uploadForm.version = '';
  uploadForm.channel = 'stable';
  uploadForm.note = '';
  uploadForm.fileName = '';
  uploadForm.fileSize = 0;
  uploadForm.payloadB64 = '';
  uploadForm.reading = false;
  uploadForm.readError = '';
  uploadOpen.value = true;
}

/** 关闭上传对话框：清草稿。 */
function closeUpload(): void {
  uploadOpen.value = false;
  uploadConfirmOpen.value = false;
  uploadForm.version = '';
  uploadForm.channel = 'stable';
  uploadForm.note = '';
  uploadForm.fileName = '';
  uploadForm.fileSize = 0;
  uploadForm.payloadB64 = '';
  uploadForm.reading = false;
  uploadForm.readError = '';
}

/** 文件 → base64（去 data URL 前缀）。 */
function readFileAsBase64(file: File): Promise<string> {
  return new Promise<string>((resolve, reject) => {
    const reader = new FileReader();
    reader.onerror = () => reject(new Error('文件读取失败'));
    reader.onload = () => {
      const result = reader.result;
      if (typeof result !== 'string') {
        reject(new Error('文件读取结果格式异常'));
        return;
      }
      const idx = result.indexOf(',');
      resolve(idx >= 0 ? result.slice(idx + 1) : result);
    };
    reader.readAsDataURL(file);
  });
}

/** 选择文件：异步读取为 base64。 */
async function onFileChange(e: Event): Promise<void> {
  const input = e.target as HTMLInputElement;
  const file = input.files && input.files.length > 0 ? input.files[0] : null;
  uploadForm.payloadB64 = '';
  uploadForm.readError = '';
  if (!file) {
    uploadForm.fileName = '';
    uploadForm.fileSize = 0;
    return;
  }
  uploadForm.fileName = file.name;
  uploadForm.fileSize = file.size;
  uploadForm.reading = true;
  try {
    uploadForm.payloadB64 = await readFileAsBase64(file);
  } catch (cause) {
    uploadForm.readError = cause instanceof Error ? cause.message : '文件读取失败';
    uploadForm.payloadB64 = '';
  } finally {
    uploadForm.reading = false;
  }
}

/** 是否可进入确认步。 */
const canSubmitUpload = computed(
  () =>
    uploadForm.version.trim().length > 0 &&
    uploadForm.payloadB64.length > 0 &&
    !uploadForm.reading &&
    uploadForm.readError === '',
);

/** 上传二次确认对象摘要。 */
const uploadFacts = computed(() => [
  { label: '版本号', value: uploadForm.version.trim() || '—' },
  { label: '通道', value: uploadForm.channel },
  { label: '文件', value: uploadForm.fileName || '—' },
  { label: '大小', value: `${uploadForm.fileSize} 字节` },
  { label: '发布说明', value: uploadForm.note.trim() || '—' },
]);

/** 提交上传（四要素已校验）。 */
async function submitUpload(payload: {
  reason: string;
  note: string;
  confirm: string;
}): Promise<void> {
  const created = await repo.uploadUpdate({
    version: uploadForm.version.trim(),
    channel: uploadForm.channel,
    payloadB64: uploadForm.payloadB64,
    note: uploadForm.note.trim(),
    reason: payload.reason,
    // note_detail 与 reason / note 彼此独立，绝不拼接
    noteDetail: payload.note,
    confirm: payload.confirm,
    actor: DEFAULT_ACTOR,
  });
  closeUpload();
  if (created) {
    await reload();
  }
  // 失败原因见全局横幅：保留列表原状，用户可重新选择文件重试。
}

// ---------------- 发布 / 停用 ----------------
/** 发布弹窗。 */
const publishOpen = ref(false);
const publishTarget = ref<OtaPackageRecord | null>(null);
/** 停用弹窗。 */
const disableOpen = ref(false);
const disableTarget = ref<OtaPackageRecord | null>(null);

/** 发布可选原因。 */
const PUBLISH_REASONS: readonly string[] = [
  '按计划发布新版本',
  '修复线上缺陷后的发布',
  '安全补丁发布',
  '重新发布（此前已停用）',
];

/** 停用可选原因。 */
const DISABLE_REASONS: readonly string[] = [
  '该版本存在严重缺陷，紧急停用',
  '计划性回滚',
  '合规要求下架',
  '其他',
];

/** 状态切换弹窗的对象摘要。 */
function statusFacts(row: OtaPackageRecord | null): { label: string; value: string }[] {
  if (!row) {
    return [];
  }
  return [
    { label: '版本号', value: row.version },
    { label: '通道', value: row.channel },
    { label: '当前状态', value: otaStatus(row.status).label },
    { label: '发布时间', value: row.publishedAt },
    { label: '签名密钥', value: row.kid || '—' },
  ];
}

/** 打开发布弹窗。 */
function openPublish(row: OtaPackageRecord): void {
  publishTarget.value = row;
  publishOpen.value = true;
}

/** 打开停用弹窗。 */
function openDisable(row: OtaPackageRecord): void {
  disableTarget.value = row;
  disableOpen.value = true;
}

/** 提交发布。 */
async function submitPublish(payload: { reason: string; note: string; confirm: string }): Promise<void> {
  const target = publishTarget.value;
  if (!target) {
    return;
  }
  const updated = await repo.publishUpdate(target.version, {
    channel: target.channel,
    reason: payload.reason,
    note: payload.note,
    confirm: payload.confirm,
    actor: DEFAULT_ACTOR,
  });
  publishOpen.value = false;
  publishTarget.value = null;
  if (updated) {
    await reload();
  }
}

/** 提交停用。 */
async function submitDisable(payload: { reason: string; note: string; confirm: string }): Promise<void> {
  const target = disableTarget.value;
  if (!target) {
    return;
  }
  const updated = await repo.disableUpdate(target.version, {
    channel: target.channel,
    reason: payload.reason,
    note: payload.note,
    confirm: payload.confirm,
    actor: DEFAULT_ACTOR,
  });
  disableOpen.value = false;
  disableTarget.value = null;
  if (updated) {
    await reload();
  }
}
</script>

<style scoped>
.ac-file {
  font-size: var(--fs-table);
  color: var(--text-1);
}
.ac-modal-mask {
  position: fixed;
  inset: 0;
  background: rgba(29, 33, 41, 0.45);
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 2000;
  padding: 24px;
}
.ac-modal {
  background: #fff;
  border-radius: var(--radius);
  box-shadow: var(--shadow);
  width: 640px;
  max-width: 100%;
  max-height: 88vh;
  display: flex;
  flex-direction: column;
}
.ac-modal__head {
  padding: 16px 20px;
  border-bottom: 1px solid var(--divider);
}
.ac-modal__head h3 {
  margin: 0;
  font-size: 16px;
  font-weight: 600;
}
.ac-modal__body {
  padding: 20px;
  overflow: auto;
  display: flex;
  flex-direction: column;
  gap: 14px;
}
.ac-modal__foot {
  padding: 14px 20px;
  border-top: 1px solid var(--divider);
  display: flex;
  justify-content: flex-end;
  gap: 8px;
  flex-wrap: wrap;
}
</style>
