<!--
  =============================================================================
  SettingsPage —— 系统设置
  =============================================================================
  页签结构（`docs/design/prototype/gateway-v2a-glacier.html` :2141-2253）：
    基础 / 网络 / 存储 / 安全。

  真实读写边界（重要）：
    · real 模式：进页 `GET /api/settings` 填充 → 「保存」`PUT /api/settings` 持久化
      （写前自动备份 + 热重载生效，结果区展示 config_version 与备份文件名）；
      400 字段级校验原因（field + reason）原样呈现；
    · 可写组（后端白名单）：basic{gateway_id} / storage{sqlite_path,max_size_mb,retention_days} /
      security{web_auth_enabled,tls_cert_path,tls_key_path}；
    · 只读组：OEM（由厂商授权后台随牌照下发）与 network（出口在北向转发页管理）——
      展示为只读态并注明原因（PUT 这两组会 400）；
    · 脱敏红线：出口 password 后端已脱敏（<redacted>/null），激活码只回布尔位，
      管理用户不含口令哈希——前端一律原样展示，不尝试还原；
    · 「配置回滚」：备份清单来自真实 `GET /api/settings/backups`，支持定向回滚
      （DangerConfirmModal 完整契约 + 备份文件名后 8 位二次校验）。
-->
<template>
  <PageHeader
    crumb="系统 / 系统设置"
    title="系统设置"
    desc="按「基础 / 网络 / 存储 / 安全」四类组织：基础 / 存储 / 安全三组真实读写（保存即持久化并热重载），OEM 与网络为只读组（分别由厂商授权后台与北向转发页管理）。"
  >
    <template #actions>
      <RoleGate :allowed="canEdit" mode="disable" deny-text="设置写入仅限管理员（system 角色）" fallback-label="无权保存">
        <button
          type="button"
          class="wc-btn wc-btn--primary wc-btn--sm"
          data-testid="btn-save-settings"
          :disabled="saving"
          @click="saveSettings"
        >
          {{ saving ? '保存中…' : saved ? '已保存' : '保存' }}
        </button>
      </RoleGate>
    </template>
  </PageHeader>

  <div class="wc-content">
    <!-- 未保存提示 -->
    <div v-if="dirty" class="wc-banner wc-banner--warn" data-testid="dirty-banner">
      <span aria-hidden="true">⚠</span>
      <span>有未保存的修改。保存后网关自动生成写前备份，可在此页「基础 · 配置回滚」中回退。</span>
    </div>

    <!-- 读取失败：如实呈现原因 + 恢复路径，绝不回退演示数据 -->
    <div v-if="settingsLoadError" class="wc-banner wc-banner--danger" data-testid="settings-load-error">
      <span aria-hidden="true">⚠</span>
      <span>设置读取失败：{{ settingsLoadError }}（请检查网关连接后刷新页面重试。）</span>
    </div>

    <!-- 保存结果（后端结果原样呈现，含 400 字段级原因） -->
    <div v-if="saveResult" class="st-result" :class="`is-${saveResultKind}`" data-testid="save-result">
      <span aria-hidden="true">{{ saveResultKind === 'ok' ? '✓' : '⚠' }}</span>
      <span>{{ saveResult }}</span>
    </div>

    <!-- 页签 -->
    <div class="st-tabs" role="tablist">
      <button
        v-for="(tab, i) in TABS"
        :key="tab"
        type="button"
        class="st-tab"
        :class="{ 'is-on': activeTab === i }"
        role="tab"
        :aria-selected="activeTab === i ? 'true' : 'false'"
        @click="activeTab = i"
      >
        {{ tab }}
      </button>
    </div>

    <!-- ══════════ 基础 ══════════ -->
    <template v-if="activeTab === 0">
      <div class="wc-grid wc-grid--2-1">
        <section class="wc-card">
          <div class="wc-card__head">
            <h3>网关标识</h3>
            <span class="wc-card__sub" data-testid="basic-mode-hint">真实读写（写前自动备份 + 热重载生效）</span>
          </div>
          <div class="wc-card__body">
            <div class="wc-form">
              <UiField label="网关标识" required hint="重启 / 停止等运维操作需回显该标识；保存后热重载生效">
                <UiInput v-model="basicForm.gatewayId" data-testid="set-gateway-id" />
              </UiField>
              <UiField label="数据目录" hint="网关运行期数据根目录（只读）">
                <UiInput :model-value="dataDir" :disabled="true" placeholder="—" />
              </UiField>
            </div>
            <p class="wc-note" data-testid="basic-note">
              <span class="wc-note__icon" aria-hidden="true">ⓘ</span>
              <span>
                原型中的 所属站点 / 时区 / 界面语言 / NTP 时间源 暂无后端设置项（PUT basic 白名单仅
                <code>gateway_id</code>），待端点扩展后接入，此处不做假表单。
              </span>
            </p>
          </div>
        </section>

        <section class="wc-card">
          <div class="wc-card__head">
            <h3>配置回滚</h3>
            <span class="wc-card__sub">危险操作 · 备份来自网关 config 目录</span>
          </div>
          <div class="wc-card__body">
            <dl class="wc-kv">
              <dt>当前配置版本</dt>
              <dd class="wc-mono" data-testid="current-version">{{ configVersion || '—' }}</dd>
              <dt>可用备份</dt>
              <dd data-testid="backups-count">{{ backups.length }} 份</dd>
              <dt>回滚影响</dt>
              <dd>仅回滚配置，不影响采集数据与设备连接</dd>
            </dl>

            <p v-if="backupsError" class="wc-note wc-note--warn" data-testid="backups-error">
              <span class="wc-note__icon" aria-hidden="true">⚠</span>
              <span>备份清单读取失败：{{ backupsError }}</span>
            </p>
            <p v-else-if="!backups.length" class="wc-note" data-testid="backups-empty">
              <span class="wc-note__icon" aria-hidden="true">ⓘ</span>
              <span>
                网关 config 目录暂无备份：每次配置写入 / 回滚前会自动生成
                <code>config.toml.YYYYMMDD-HHmmss-NNN.bak</code> 写前备份（内嵌 UTC+8 时刻）；执行一次「保存」后再来查看。
              </span>
            </p>
            <template v-else>
              <div class="wc-list">
                <div v-for="bak in pagedBackups" :key="bak.file" class="wc-list__item">
                  <div>
                    <div class="wc-list__title wc-mono">{{ bak.file }}</div>
                    <div class="wc-list__desc">{{ formatBytes(bak.sizeBytes) }} · {{ formatMtime(bak.mtimeMs) }}</div>
                  </div>
                  <div class="wc-list__ops">
                    <RoleGate :allowed="canRollback" mode="disable" deny-text="当前角色无权执行配置回滚" fallback-label="回滚">
                      <button
                        type="button"
                        class="wc-btn wc-btn--sm wc-btn--danger"
                        data-testid="btn-rollback"
                        @click="openRollback(bak)"
                      >
                        回滚到此版本
                      </button>
                    </RoleGate>
                  </div>
                </div>
              </div>
              <UiPager
                v-if="backups.length > BACKUP_PAGE_SIZE"
                :page="backupPage"
                :total="backups.length"
                :page-size="BACKUP_PAGE_SIZE"
                @update:page="onBackupPage"
              />
              <RoleGate
                :allowed="canRollback"
                mode="disable"
                deny-text="当前角色无权执行配置回滚"
                fallback-label="回滚到最新备份"
              >
                <button
                  type="button"
                  class="wc-btn wc-btn--sm st-latest-btn"
                  data-testid="btn-rollback-latest"
                  @click="openRollback()"
                >
                  回滚到最新备份
                </button>
              </RoleGate>
            </template>

            <div
              v-if="rollbackResult"
              class="st-result"
              :class="`is-${rollbackResultKind}`"
              data-testid="rollback-result"
            >
              <span aria-hidden="true">{{ rollbackResultKind === 'ok' ? '✓' : '⚠' }}</span>
              <span>{{ rollbackResult }}</span>
            </div>

            <p class="wc-note" data-testid="rollback-note">
              <span class="wc-note__icon" aria-hidden="true">ⓘ</span>
              <span>
                real 模式调用真实接口 <code>POST /api/settings/rollback</code>：选择某份备份即定向回滚
                （后端校验文件名前缀，防路径穿越）；不选则回滚到最新备份。回滚前网关对当前配置自动再备份
                （可逆），热重载即时生效。
              </span>
            </p>
          </div>
        </section>
      </div>

      <!-- 贴牌（OEM）—— 只读组 -->
      <section class="wc-card">
        <div class="wc-card__head">
          <h3>贴牌（OEM）</h3>
          <span class="wc-tag wc-tag--neutral">只读 · 由厂商授权后台下发</span>
        </div>
        <div class="wc-card__body">
          <dl class="wc-kv">
            <dt>管理方</dt>
            <dd class="wc-mono" data-testid="oem-managed-by">{{ oemInfo.managedBy || 'vendor-license-backend' }}</dd>
            <dt>说明</dt>
            <dd data-testid="oem-note">{{ oemInfo.note || '贴牌品牌信息由厂商管理后台随牌照统一下发，网关本地只读应用。' }}</dd>
            <dt>当前生效品牌</dt>
            <dd>IoT-DAQ（原厂标识）</dd>
          </dl>
          <p class="wc-note">
            <span class="wc-note__icon" aria-hidden="true">ⓘ</span>
            <span>
              OEM 组只读（PUT 会 400）：品牌名 / Logo / 主题色由厂商总后台在下发牌照时写入，网关本地只读应用。
              贴牌仅替换视觉层——<b>授权判定、数据链路与审计标识不受影响</b>，审计日志始终记录设备与授权真实归属。
            </span>
          </p>
        </div>
      </section>
    </template>

    <!-- ══════════ 网络 ══════════ -->
    <template v-else-if="activeTab === 1">
      <section class="wc-card">
        <div class="wc-card__head">
          <h3>北向出口（network）</h3>
          <span class="wc-tag wc-tag--neutral">只读 · 出口在「北向转发」页管理</span>
        </div>
        <div class="wc-card__body">
          <p class="wc-note">
            <span class="wc-note__icon" aria-hidden="true">ⓘ</span>
            <span>
              network 组为只读（PUT 会 400）：出口登记 / 编辑在「北向转发」页进行；出口密码已由后端脱敏为
              <code>&lt;redacted&gt;</code>，前端不还原明文。
            </span>
          </p>
          <template v-if="outlets.length">
          <table class="st-outlets" data-testid="outlets-table">
            <thead>
              <tr>
                <th>名称</th><th>Broker</th><th>Topic 前缀</th><th>QoS</th><th>TLS</th><th>编码</th><th>用户名</th><th>密码</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="o in pagedOutlets" :key="o.id">
                <td class="wc-mono">{{ o.name || o.id }}</td>
                <td class="wc-mono">{{ o.broker }}</td>
                <td class="wc-mono">{{ o.topicPrefix }}</td>
                <td class="wc-mono">{{ o.qos }}</td>
                <td>
                  <span class="wc-tag" :class="o.tls ? 'wc-tag--ok' : 'wc-tag--neutral'">{{ o.tls ? '启用' : '关闭' }}</span>
                </td>
                <td class="wc-mono">{{ o.encoding }}</td>
                <td class="wc-mono">{{ o.username || '—' }}</td>
                <td class="wc-mono">{{ o.password || '未配置' }}</td>
              </tr>
            </tbody>
          </table>
          <UiPager
            v-if="outlets.length > OUTLET_PAGE_SIZE"
            :page="outletPage"
            :total="outlets.length"
            :page-size="OUTLET_PAGE_SIZE"
            @update:page="onOutletPage"
          />
          </template>
          <p v-else class="wc-note" data-testid="outlets-empty">
            <span class="wc-note__icon" aria-hidden="true">ⓘ</span>
            <span>当前配置没有北向出口。出口登记在「北向转发」页进行。</span>
          </p>
        </div>
      </section>
    </template>

    <!-- ══════════ 存储 ══════════ -->
    <template v-else-if="activeTab === 2">
      <div class="wc-grid wc-grid--2">
        <section class="wc-card">
          <div class="wc-card__head">
            <h3>本地存储</h3>
            <span class="wc-card__sub">取自网关当前生效配置（GET storage）</span>
          </div>
          <div class="wc-card__body">
            <dl class="wc-kv">
              <dt>数据目录</dt><dd class="wc-mono">{{ dataDir || '—' }}</dd>
              <dt>SQLite 队列库</dt><dd class="wc-mono">{{ storageForm.sqlitePath || '—' }}</dd>
              <dt>加密方式</dt><dd class="wc-mono">SQLCipher · HKDF(机器码)</dd>
              <dt>队列上限</dt><dd class="wc-mono">{{ storageForm.maxSizeMb || '—' }} MB</dd>
              <dt>遥测保留</dt><dd class="wc-mono">{{ storageForm.retentionDays || '—' }} 天</dd>
            </dl>
          </div>
        </section>

        <section class="wc-card">
          <div class="wc-card__head">
            <h3>存储参数</h3>
            <span class="wc-card__sub" data-testid="storage-mode-hint">真实读写（写前自动备份 + 热重载生效）</span>
          </div>
          <div class="wc-card__body">
            <div class="wc-form wc-form--single">
              <UiField label="SQLite 队列库文件" hint="相对数据目录的库文件名；保存后热重载生效">
                <UiInput v-model="storageForm.sqlitePath" data-testid="set-sqlite-path" />
              </UiField>
              <UiField label="队列环形覆盖上限（MB）" hint="达到上限后覆盖最旧数据">
                <UiInput v-model="storageForm.maxSizeMb" type="number" data-testid="set-max-size-mb" />
              </UiField>
              <UiField label="遥测数据保留（天）" hint="超期由网关后台线程清理">
                <UiInput v-model="storageForm.retentionDays" type="number" data-testid="set-retention-days" />
              </UiField>
            </div>
            <p class="wc-note wc-note--warn">
              <span class="wc-note__icon" aria-hidden="true">⚠</span>
              <span>
                Docker 部署时数据目录必须挂载到<b>宿主机持久卷</b>，否则容器重建即丢失队列与遥测。
                原型中的日志保留 / 队列保留天数细分项暂无后端设置项（PUT storage 白名单仅
                sqlite_path / max_size_mb / retention_days），待端点扩展后接入。
              </span>
            </p>
          </div>
        </section>
      </div>
    </template>

    <!-- ══════════ 安全 ══════════ -->
    <template v-else>
      <div class="wc-grid wc-grid--2">
        <section class="wc-card">
          <div class="wc-card__head">
            <h3>安全设置</h3>
            <span class="wc-card__sub" data-testid="security-mode-hint">真实读写（写前自动备份 + 热重载生效）</span>
          </div>
          <div class="wc-card__body">
            <div class="st-row">
              <div class="st-row__text">
                <div class="st-row__title">管理端登录鉴权（web_auth_enabled）</div>
                <div class="st-row__desc">关闭后管理 API 不再校验登录，仅限隔离内网使用。</div>
              </div>
              <UiSwitch v-model="securityForm.webAuthEnabled" />
              <span class="st-cur wc-mono">{{ securityForm.webAuthEnabled ? '启用' : '关闭' }}</span>
            </div>

            <div class="wc-form wc-form--single">
              <UiField label="TLS 证书路径" hint="管理端 HTTPS 证书 PEM；留空表示未配置">
                <UiInput v-model="securityForm.tlsCertPath" placeholder="/pem/cert.pem" data-testid="set-tls-cert-path" />
              </UiField>
              <UiField label="TLS 私钥路径" hint="仅存路径，内容不回显">
                <UiInput v-model="securityForm.tlsKeyPath" placeholder="/pem/key.pem" data-testid="set-tls-key-path" />
              </UiField>
            </div>

            <dl class="wc-kv">
              <dt>激活码</dt>
              <dd>
                <span class="wc-tag" :class="activationCodeSet ? 'wc-tag--ok' : 'wc-tag--neutral'" data-testid="activation-code-state">
                  {{ activationCodeSet === null ? '—' : activationCodeSet ? '已设置' : '未设置' }}
                </span>
                <span class="pt-dd-hint">激活码本体不下发，只回布尔位。</span>
              </dd>
              <dt>管理用户</dt>
              <dd>
                <template v-if="mgmtUsers.length">
                  <span v-for="u in mgmtUsers" :key="u.name" class="wc-tag wc-tag--info st-user-tag">{{ u.name }}（{{ u.role }}）</span>
                </template>
                <span v-else>—</span>
                <span class="pt-dd-hint">只读展示（name / role），口令哈希不下发。</span>
              </dd>
            </dl>

            <p class="wc-note">
              <span class="wc-note__icon" aria-hidden="true">ⓘ</span>
              <span>
                原型中的 北向 TLS 强制校验 / mTLS 双向认证 暂无后端设置项（PUT security 白名单仅
                web_auth_enabled / tls_cert_path / tls_key_path），待端点扩展后接入，此处不做假开关。
              </span>
            </p>
          </div>
        </section>

        <section class="wc-card">
          <div class="wc-card__head">
            <h3>审计</h3>
            <span class="wc-card__sub">网关侧强制，界面只读</span>
          </div>
          <div class="wc-card__body">
            <dl class="wc-kv">
              <dt>登录事件</dt><dd><span class="wc-tag wc-tag--ok">记录</span></dd>
              <dt>配置变更</dt><dd><span class="wc-tag wc-tag--ok">记录（含改前改后）</span></dd>
              <dt>模拟开关变更</dt>
              <dd>
                <span class="wc-tag wc-tag--info">随 P0-4 模拟策略提供</span>
                <span class="pt-dd-hint">逐点模拟尚未立项，故当前不存在该类事件。</span>
              </dd>
              <dt>授权事件</dt><dd><span class="wc-tag wc-tag--ok">记录</span></dd>
              <dt>日志保留</dt><dd>追加写入 + 分段校验，不可篡改</dd>
            </dl>
            <p class="wc-note">
              <span class="wc-note__icon" aria-hidden="true">ⓘ</span>
              <span>审计日志采用追加写入 + 分段校验，检测到篡改会告警。</span>
            </p>
          </div>
        </section>
      </div>
    </template>
  </div>

  <!-- ===== 配置回滚：危险操作二次确认（原因必填 + 备份文件名二次校验）===== -->
  <DangerConfirmModal
    :open="rollback.open"
    :title="rollback.title"
    :impacts="rollback.impacts"
    :facts="rollback.facts"
    :reasons="ROLLBACK_REASONS"
    :min-note-length="10"
    :confirm-value="rollback.confirmValue"
    confirm-label="备份二次确认（输入备份文件名后 8 位）"
    confirm-placeholder="输入备份文件名去分隔符后的后 8 位"
    confirm-text="确认回滚"
    @close="rollback.open = false"
    @submit="submitRollback"
  />
</template>

<script setup lang="ts">
/**
 * @file SettingsPage.vue
 * @module web-console/pages/SettingsPage
 * @description 系统设置：基础 / 网络 / 存储 / 安全四个页签。
 *
 * real 模式：进页 GET /api/settings 填充 → 保存 PUT /api/settings（白名单三组，写前备份 +
 * 热重载）→ 备份清单 GET /api/settings/backups 支撑定向回滚。OEM / network 只读组展示原因。
 * 写失败（400 / 403 / 网络错误）一律原样呈现，绝不回退演示数据或「本地假成功」。
 */
import { computed, nextTick, onMounted, reactive, ref, watch } from 'vue';
import {
  PageHeader,
  UiField,
  UiInput,
  UiSwitch,
  UiPager,
  RoleGate,
  DangerConfirmModal,
  type DangerFact,
} from '@ui-kit';
import { session } from '../store/session';
import { repo } from '@/api/repo';
import type { SettingsBackupRow, SettingsOutletRow } from '@/api/repo';
import { formatTimestampText } from '@/utils/time';

/** 页签名（原型 :2143）。 */
const TABS = ['基础', '网络', '存储', '安全'] as const;

const activeTab = ref(0);

// ---------------------------------------------------------------------------
// 真实设置视图（GET /api/settings 填充；取不到即诚实空态 + 错误横幅）
// ---------------------------------------------------------------------------

const settingsLoadError = ref('');
const configVersion = ref('');
const dataDir = ref('');
const oemInfo = reactive({ managedBy: '', note: '' });
const outlets = ref<SettingsOutletRow[]>([]);

/**
 * 网络出口列表分页（切片留在页面级 computed；原生 table 纯展示）。
 * 出口来自真实 `GET /api/settings` 的 network.outlets，是可增长列表，按页渲染。
 */
const OUTLET_PAGE_SIZE = 5;
const outletPage = ref(1);

/** 当前页出口（由 `outletPage` 驱动的真实切片）。 */
const pagedOutlets = computed<SettingsOutletRow[]>(() => {
  const start = (outletPage.value - 1) * OUTLET_PAGE_SIZE;
  return outlets.value.slice(start, start + OUTLET_PAGE_SIZE);
});

/** 换页（由 UiPager 驱动）。 */
function onOutletPage(next: number): void {
  outletPage.value = next;
}
const mgmtUsers = ref<{ name: string; role: string }[]>([]);
const activationCodeSet = ref<boolean | null>(null);

/** 可写表单（与后端白名单一一对应）。 */
const basicForm = reactive({ gatewayId: repo.getGateway().name });
const storageForm = reactive({ sqlitePath: 'queue.db', maxSizeMb: '10', retentionDays: '30' });
const securityForm = reactive({ webAuthEnabled: true, tlsCertPath: '', tlsKeyPath: '' });

/** 当前角色是否可保存（后端 PUT 仅 system 角色；web-console 侧 system 映射为 admin）。 */
const canEdit = computed<boolean>(() => session.state.role === 'admin');

// ---------------------------------------------------------------------------
// 保存（PUT /api/settings）
// ---------------------------------------------------------------------------

const saved = ref(false);
const dirty = ref(false);
const saving = ref(false);
const saveResult = ref('');
const saveResultKind = ref<'ok' | 'warn'>('ok');

/** 表单填充期间抑制脏标记（nextTick 后恢复，避免回填被误判为用户修改）。 */
let suppressDirty = false;

watch([basicForm, storageForm, securityForm], () => {
  if (!suppressDirty) {
    dirty.value = true;
  }
}, { deep: true });

/** 拉取设置视图 + 备份清单并填充表单（real 专用）。 */
async function loadSettings(): Promise<void> {
  settingsLoadError.value = '';
  const result = await repo.settings.get();
  if (!result.ok || !result.view) {
    settingsLoadError.value = result.message || '设置读取失败。';
    return;
  }
  const view = result.view;
  suppressDirty = true;
  configVersion.value = view.configVersion;
  dataDir.value = view.basic.dataDir;
  oemInfo.managedBy = view.oem.managedBy;
  oemInfo.note = view.oem.note;
  outlets.value = view.network.outlets;
  outletPage.value = 1;
  mgmtUsers.value = view.security.mgmtUsers;
  activationCodeSet.value = view.security.activationCodeSet;
  basicForm.gatewayId = view.basic.gatewayId;
  storageForm.sqlitePath = view.storage.sqlitePath;
  storageForm.maxSizeMb = view.storage.maxSizeMb;
  storageForm.retentionDays = view.storage.retentionDays;
  securityForm.webAuthEnabled = view.security.webAuthEnabled;
  securityForm.tlsCertPath = view.security.tlsCertPath;
  securityForm.tlsKeyPath = view.security.tlsKeyPath;
  dirty.value = false;
  void nextTick(() => {
    suppressDirty = false;
  });

  const backupsResult = await repo.settings.backups();
  if (backupsResult.ok) {
    backups.value = backupsResult.rows;
    backupPage.value = 1;
    backupsError.value = '';
  } else {
    backups.value = [];
    backupPage.value = 1;
    backupsError.value = backupsResult.message;
  }
}

onMounted(() => {
  void loadSettings();
});

/** 保存设置：`PUT /api/settings`（三组白名单；写前备份 + 热重载）。 */
async function saveSettings(): Promise<void> {
  saving.value = true;
  saveResult.value = '';
  const result = await repo.settings.update({
    actor: session.state.displayName,
    basic: { gatewayId: basicForm.gatewayId },
    storage: {
      sqlitePath: storageForm.sqlitePath,
      maxSizeMb: storageForm.maxSizeMb,
      retentionDays: storageForm.retentionDays,
    },
    security: {
      webAuthEnabled: securityForm.webAuthEnabled,
      tlsCertPath: securityForm.tlsCertPath,
      tlsKeyPath: securityForm.tlsKeyPath,
    },
  });
  saving.value = false;
  saveResult.value = result.ok
    ? `${result.message}（配置版本 ${result.configVersion ?? '—'} · 写前备份 ${result.backup ?? '—'}）`
    : result.message;
  saveResultKind.value = result.ok ? 'ok' : 'warn';
  if (result.ok) {
    dirty.value = false;
    await loadSettings();
  }
}

// ---------------------------------------------------------------------------
// 配置回滚（真实备份清单 + 定向回滚）
// ---------------------------------------------------------------------------

/** 当前角色是否可执行回滚（仅 admin；后端 device.write / system）。 */
const canRollback = computed<boolean>(() => session.state.role === 'admin');

const backups = ref<SettingsBackupRow[]>([]);
const backupsError = ref('');

/**
 * 配置回滚列表分页（切片留在页面级 computed，UiTable / 列表只做纯展示，不在组件内做局部 slice）。
 * 备份清单来自真实 `GET /api/settings/backups`，可有多份，按页渲染。
 */
const BACKUP_PAGE_SIZE = 4;
const backupPage = ref(1);

/** 当前页备份（由 `backupPage` 驱动的真实切片）。 */
const pagedBackups = computed<SettingsBackupRow[]>(() => {
  const start = (backupPage.value - 1) * BACKUP_PAGE_SIZE;
  return backups.value.slice(start, start + BACKUP_PAGE_SIZE);
});

/** 换页（由 UiPager 驱动）。 */
function onBackupPage(next: number): void {
  backupPage.value = next;
}

/**
 * 备份大小（字节字符串 → 人类可读；解析失败原样展示）。
 *
 * 大数红线：`size_bytes` 是 JSON **字符串**编码的 uint64，绝不可 `Number()` / `parseInt`
 * （> 2^53−1 静默丢精度）。这里全程用 `BigInt` 做整数除法，仅在最后一步转小数。
 */
function formatBytes(sizeBytes: string): string {
  const text = sizeBytes.trim();
  if (!/^\d+$/.test(text)) {
    return sizeBytes;
  }
  const bytes = BigInt(text);
  if (bytes < 1024n) {
    return `${bytes} B`;
  }
  const kb = bytes / 1024n;
  if (kb < 1024n) {
    return `${kb} KB`;
  }
  const mb = kb / 1024n;
  return mb < 1024n
    ? `${(Number(mb) / 1024).toFixed(1)} MB`
    : `${(Number(mb / 1024n) / 1024).toFixed(1)} GB`;
}

/**
 * 备份时刻（epoch 秒 / 毫秒字符串 → 本地时间文本）。
 *
 * 大数红线：绝不 `Number()` 未判定位数的整串；统一走共享展示工具，
 * 只认 10 位秒 / 13 位毫秒，其余（uint64、纳秒、非法值）一律回 `—`。
 */
function formatMtime(mtimeMs: string): string {
  return formatTimestampText(mtimeMs);
}

/** 回滚确认弹窗状态（file 为空 = 回滚到最新备份）。 */
const rollback = reactive<{
  open: boolean;
  file: string;
  confirmValue: string;
  title: string;
  impacts: readonly string[];
  facts: readonly DangerFact[];
}>({
  open: false,
  file: '',
  confirmValue: '',
  title: '',
  impacts: [],
  facts: [],
});

/** 回滚原因枚举。 */
const ROLLBACK_REASONS: readonly string[] = [
  '配置变更后出现异常',
  '误操作需恢复',
  '现场参数需要还原',
  '其它（请在补充说明中描述）',
];

/** 打开回滚二次确认（row 缺省 = 最新备份）。 */
function openRollback(row?: SettingsBackupRow): void {
  const latest = backups.value.length ? backups.value[backups.value.length - 1] : undefined;
  const target = row ?? latest;
  if (!target) {
    return;
  }
  rollback.open = true;
  rollback.file = row?.file ?? '';
  rollback.confirmValue = target.file;
  rollback.title = `配置回滚 · ${target.file}`;
  rollback.impacts = [
    `将把当前配置回滚到备份 ${target.file}${row ? '' : '（最新备份）'}。`,
    '仅回滚配置，不回滚采集数据与设备连接状态。',
    '回滚前网关会对当前配置自动再备份（可逆），热重载即时生效。',
    '操作不可撤销；原因与补充说明将写入审计日志。',
    '需输入备份文件名后 8 位完成二次校验。',
  ];
  rollback.facts = [
    { label: '目标备份', value: target.file },
    { label: '备份大小', value: formatBytes(target.sizeBytes) },
    { label: '备份时刻', value: formatMtime(target.mtimeMs) },
    { label: '当前版本', value: configVersion.value || '—' },
    { label: '操作者', value: session.state.displayName },
    { label: '回滚范围', value: '仅配置文件（采集数据与设备连接状态不受影响）' },
  ];
}

/** 回滚结果反馈（后端结果原样呈现，含失败；不伪造成功）。 */
const rollbackResult = ref('');
const rollbackResultKind = ref<'ok' | 'warn'>('ok');

/**
 * 提交回滚：调 `repo.settings.rollback`（真实 `POST /api/settings/rollback`；
 * 定向时 body 带 backup，否则缺省回滚到最新备份）。
 * 弹窗四要素（影响清单 + 原因必填 + 备份文件名二次校验 + 草稿隔离）由 DangerConfirmModal 保证。
 */
async function submitRollback(payload: { reason: string; note: string; tail: string }): Promise<void> {
  rollback.open = false;
  // 危险三要素：reason（原因枚举）与 note（补充说明）**各自独立下发**，禁止拼接进同一个字段。
  const result = await repo.settings.rollback({
    actor: session.state.displayName,
    reason: payload.reason,
    note: payload.note,
    backup: rollback.file || undefined,
  });
  rollbackResult.value = result.ok
    ? `${result.message}${result.restoredFrom ? `（恢复自备份 ${result.restoredFrom}${result.version ? `，配置版本 ${result.version}` : ''}）` : ''}`
    : `回滚未执行：${result.message}`;
  rollbackResultKind.value = result.ok ? 'ok' : 'warn';
  if (result.ok) {
    await loadSettings();
  }
}
</script>

<style scoped>
.wc-form {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: 14px 16px;
}
.wc-form--single {
  grid-template-columns: 1fr;
}
.wc-note--warn .wc-note__icon {
  color: var(--warn);
}

/* 页签按钮（原型 .sp-tabs 观感） */
.st-tabs {
  display: flex;
  gap: 6px;
  flex-wrap: wrap;
  margin-bottom: 4px;
}
.st-tab {
  font-family: inherit;
  font-size: var(--fs-caption);
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  padding: 7px 14px;
  background: var(--bg-hover);
  color: var(--text-2);
  cursor: pointer;
  white-space: nowrap;
  transition: all 160ms cubic-bezier(0.16, 1, 0.3, 1);
}
.st-tab:hover {
  border-color: var(--brand);
  color: var(--brand);
}
.st-tab.is-on {
  border-color: var(--brand);
  background: var(--brand-subtle);
  color: var(--brand);
  font-weight: 600;
}

/* 开关行 + 当前值 */
.st-row {
  display: flex;
  align-items: center;
  gap: 16px;
  flex-wrap: wrap;
  padding: 12px 0;
  border-bottom: 1px solid var(--divider);
}
.st-row:last-of-type {
  border-bottom: 0;
}
.st-row__text {
  flex: 1 1 260px;
  min-width: 0;
}
.st-row__title {
  font-size: var(--fs-body);
  font-weight: 600;
  color: var(--text-1);
}
.st-row__desc {
  margin-top: 4px;
  font-size: var(--fs-caption);
  color: var(--text-3);
  line-height: 1.6;
}
.st-cur {
  font-size: var(--fs-caption);
  color: var(--text-2);
}

/* 回滚到最新备份按钮 */
.st-latest-btn {
  margin-top: 10px;
}

/* 北向出口只读表（token 化） */
.st-outlets {
  width: 100%;
  border-collapse: collapse;
  font-size: var(--fs-caption);
}
.st-outlets th,
.st-outlets td {
  text-align: left;
  padding: 8px 10px;
  border-bottom: 1px solid var(--divider);
  white-space: nowrap;
}
.st-outlets th {
  color: var(--text-3);
  font-weight: 600;
}
.st-outlets tbody tr:hover {
  background: var(--bg-hover);
}

/* 管理用户标签间距 */
.st-user-tag {
  margin-right: 6px;
}

/* 操作结果反馈（保存 / 回滚共用，token 化） */
.st-result {
  display: flex;
  align-items: flex-start;
  gap: 8px;
  margin: 12px 0;
  padding: 10px 12px;
  border-radius: var(--radius-sm);
  font-size: var(--fs-caption);
  line-height: 1.6;
  border: 1px solid var(--ok-border);
  background: var(--ok-bg);
  color: var(--ok-fg);
}
.st-result.is-warn {
  border-color: var(--warn-border);
  background: var(--warn-bg);
  color: var(--warn-fg);
}

.pt-dd-hint {
  margin-left: 8px;
  font-size: var(--fs-caption);
  color: var(--text-3);
}
</style>
