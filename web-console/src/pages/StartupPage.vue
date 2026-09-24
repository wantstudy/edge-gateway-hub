<template>
  <!--
    StartupPage —— 启动与自启（运维分组，路由 `/startup`）。

    硬性约定遵守情况：
      · 部署形态「自动识别」：网关自检运行环境（/.dockerenv / systemd 接管）后高亮当前形态，
        另一形态置灰标注「未采用」，绝不二选一让用户手动选错；
      · 两种形态（Docker / systemd）的配置与命令并列展示，互不干扰；
      · 自启配置（开机自启开关）属系统级变更，受 RoleGate 控制（仅管理员可改）；
      · 所有危险/系统操作只给「演示」反馈与真实命令提示，不伪造后端落地；
      · 授权判定不在前端（红线 3），本页只展示运行态。
  -->
  <PageHeader
    crumb="运维 / 启动与自启"
    title="启动与自启"
    desc="网关自检运行环境，自动识别当前部署形态（Docker 容器 / systemd 服务），并展示对应的自启配置与运维命令。系统级自启变更仅管理员可操作。"
  />

  <div class="wc-content">
    <!-- 识别结果横幅 -->
    <section class="wc-card startup-banner">
      <div class="startup-banner__main">
        <div class="startup-banner__label">当前部署形态（自动识别）</div>
        <div class="startup-banner__form">
          <StatusTag :status="detectedForm" />
          <span class="startup-banner__form-cn">{{ formCn }}</span>
        </div>
      </div>
      <button type="button" class="wc-btn wc-btn--sm" @click="showDetect = !showDetect">
        {{ showDetect ? '收起识别依据' : '识别依据' }}
      </button>
      <p v-if="showDetect" class="startup-banner__note">
        识别逻辑：运行体启动后探测 <code>/.dockerenv</code> 是否存在，并判断 PID 1 是否由
        <code>systemd</code> 托管（<code>/run/systemd/system</code> 存在且被接管）。命中容器特征即判定为
        <b>Docker</b> 形态，否则判定为 <b>systemd</b> 原生服务形态。前端仅展示该识别结果，真实判定在 Rust 侧。
      </p>
    </section>

    <!-- 形态并排卡片 -->
    <div class="wc-grid wc-grid--2">
      <!-- Docker 形态 -->
      <section class="wc-card startup-form" :class="{ 'is-active': detectedForm === 'docker' }">
        <div class="wc-card__head">
          <h3>Docker 容器形态</h3>
          <span v-if="detectedForm === 'docker'" class="wc-tag wc-tag--primary">当前形态</span>
          <span v-else class="wc-tag wc-tag--neutral">未采用</span>
        </div>
        <div class="wc-card__body startup-form__body">
          <dl class="startup-kv">
            <div><dt>容器名</dt><dd class="mono">{{ runtime.docker.containerName }}</dd></div>
            <div><dt>镜像</dt><dd class="mono">{{ runtime.docker.imageDigest }}</dd></div>
            <div><dt>重启策略</dt><dd>{{ runtime.docker.restartPolicy }}</dd></div>
            <div><dt>健康检查</dt><dd><StatusTag :status="runtime.docker.health" /></dd></div>
            <div><dt>编排文件</dt><dd class="mono small">{{ runtime.docker.composeFile }}</dd></div>
          </dl>
          <div class="startup-cmd">
            <span class="startup-cmd__title">常用命令</span>
            <pre class="mono">docker compose -f {{ runtime.docker.composeFile }} ps
docker compose -f {{ runtime.docker.composeFile }} logs -f
docker compose -f {{ runtime.docker.composeFile }} restart</pre>
          </div>
        </div>
      </section>

      <!-- systemd 形态 -->
      <section class="wc-card startup-form" :class="{ 'is-active': detectedForm === 'native' }">
        <div class="wc-card__head">
          <h3>systemd 服务形态</h3>
          <span v-if="detectedForm === 'native'" class="wc-tag wc-tag--primary">当前形态</span>
          <span v-else class="wc-tag wc-tag--neutral">未采用</span>
        </div>
        <div class="wc-card__body startup-form__body">
          <dl class="startup-kv">
            <div><dt>服务单元</dt><dd class="mono">{{ runtime.native.serviceName }}</dd></div>
            <div><dt>运行状态</dt><dd class="mono">{{ runtime.native.active }}</dd></div>
            <div><dt>开机自启</dt><dd>{{ runtime.native.bootEnable ? '已启用 (enabled)' : '已禁用 (disabled)' }}</dd></div>
          </dl>
          <div class="startup-cmd">
            <span class="startup-cmd__title">常用命令</span>
            <pre class="mono">systemctl status {{ runtime.native.serviceName }}
journalctl -u {{ runtime.native.serviceName }} -f
systemctl enable --now {{ runtime.native.serviceName }}</pre>
          </div>
        </div>
      </section>
    </div>

    <!-- 自启配置 + 操作 -->
    <section class="wc-card">
      <div class="wc-card__head">
        <h3>自启与运行控制</h3>
        <span class="wc-card__sub">作用于当前形态：{{ formCn }}</span>
      </div>
      <div class="wc-card__body">
        <div class="startup-row">
          <div class="startup-row__text">
            <div class="startup-row__title">开机自启</div>
            <div class="startup-row__desc">
              {{ detectedForm === 'docker'
                ? '对应 compose 的 restart: unless-stopped，宿主重启/断电后自动拉起。'
                : '对应 systemctl enable，宿主开机时由 systemd 自动拉起。' }}
            </div>
          </div>
          <RoleGate :allowed="canManage">
            <UiSwitch
              v-model="autostartEnabled"
              on-text="已启用"
              off-text="已关闭"
              @update:model-value="onAutostart"
            />
          </RoleGate>
          <span v-if="!canManage" class="wc-tag wc-tag--neutral">仅管理员可改</span>
        </div>

        <div class="startup-row">
          <div class="startup-row__text">
            <div class="startup-row__title">运行控制</div>
            <div class="startup-row__desc">立即重启运行体，或打开日志流（演示，不实际执行）。</div>
          </div>
          <div class="startup-row__actions">
            <button type="button" class="wc-btn" @click="onRestart">重启服务</button>
            <button type="button" class="wc-btn" @click="onLogs">查看日志</button>
          </div>
        </div>

        <p v-if="lastAction" class="startup-action-note">
          <StatusTag status="success" /> {{ lastAction }}
        </p>
      </div>
    </section>
  </div>
</template>

<script setup lang="ts">
/**
 * @file StartupPage.vue
 * @module web-console/pages/StartupPage
 * @description 启动与自启：部署形态（Docker / systemd）自动识别 + 自启配置 + 运行控制。
 *
 * 运行态数据为前端只读展示（网关自检结果），真实判定在 Rust 侧；本页不持有任何授权/放行依据。
 */
import { computed, ref } from 'vue';
import { PageHeader, StatusTag, RoleGate, UiSwitch } from '@ui-kit';
import { session } from '../store/session';

/** 部署形态。 */
type RuntimeForm = 'docker' | 'native';

/** 自动识别出的当前形态（mock：默认 Docker，与 Linux 交付主推形态一致）。 */
const detectedForm = ref<RuntimeForm>('docker');

/** 形态中文名。 */
const formCn = computed(() => (detectedForm.value === 'docker' ? 'Docker 容器' : 'systemd 服务'));

/** 两种形态的运行态快照（只读展示）。 */
const runtime = {
  docker: {
    containerName: 'iot-daq-gateway',
    imageDigest: 'sha256:9f2c4b…a31e',
    restartPolicy: 'unless-stopped',
    health: 'healthy',
    composeFile: 'deploy/docker/docker-compose.yml',
  },
  native: {
    serviceName: 'iot-daq-gateway.service',
    active: 'active (running)',
    bootEnable: true,
  },
} as const;

/** 是否可管理系统级自启（仅管理员）。 */
const canManage = computed(() => session.state.role === 'admin');

/** 开机自启开关（作用于当前形态）。 */
const autostartEnabled = ref(true);

/** 最近一次操作反馈（演示）。 */
const lastAction = ref('');

/** 识别依据展开态。 */
const showDetect = ref(false);

/** 自启开关变更。 */
function onAutostart(next: boolean): void {
  autostartEnabled.value = next;
  const mechanism = detectedForm.value === 'docker' ? 'compose restart 策略' : 'systemd enable/disable';
  lastAction.value = `已${next ? '启用' : '关闭'}开机自启（作用于 ${mechanism}）`;
}

/** 重启服务（演示）。 */
function onRestart(): void {
  lastAction.value = '已向运行体发送重启信号（演示，真实形态下即 restart 当前服务）';
}

/** 查看日志（演示）。 */
function onLogs(): void {
  const cmd =
    detectedForm.value === 'docker'
      ? `docker compose -f ${runtime.docker.composeFile} logs -f`
      : `journalctl -u ${runtime.native.serviceName} -f`;
  lastAction.value = `已打开日志流（演示）。真实命令：${cmd}`;
}
</script>

<style scoped>
.startup-banner {
  display: flex;
  align-items: center;
  gap: 16px;
  flex-wrap: wrap;
}
.startup-banner__main {
  flex: 1 1 auto;
  min-width: 220px;
}
.startup-banner__label {
  font-size: var(--fs-caption);
  color: var(--text-3);
  margin-bottom: 6px;
}
.startup-banner__form {
  display: flex;
  align-items: center;
  gap: 10px;
}
.startup-banner__form-cn {
  font-size: var(--fs-h3);
  font-weight: 600;
  color: var(--text-1);
}
.startup-banner__note {
  flex-basis: 100%;
  margin: 4px 0 0;
  padding-top: 12px;
  border-top: 1px dashed var(--border-2);
  font-size: var(--fs-caption);
  color: var(--text-3);
  line-height: 1.7;
}
.startup-banner__note code {
  font-family: var(--font-mono);
  background: var(--surface-2);
  padding: 1px 5px;
  border-radius: var(--radius-sm);
  color: var(--text-2);
}

.startup-form {
  transition: border-color 0.16s ease, box-shadow 0.16s ease;
}
.startup-form.is-active {
  border-color: var(--brand);
  box-shadow: 0 0 0 1px var(--brand) inset;
}
.startup-form__body {
  display: flex;
  flex-direction: column;
  gap: 16px;
}
.startup-kv {
  margin: 0;
  display: grid;
  grid-template-columns: 1fr;
  gap: 8px;
}
.startup-kv > div {
  display: flex;
  justify-content: space-between;
  gap: 12px;
  padding: 6px 0;
  border-bottom: 1px solid var(--border-1);
}
.startup-kv dt {
  font-size: var(--fs-caption);
  color: var(--text-3);
}
.startup-kv dd {
  margin: 0;
  font-size: var(--fs-body);
  color: var(--text-1);
  text-align: right;
}
.startup-kv dd.small {
  font-size: var(--fs-caption);
}
.startup-cmd__title {
  display: block;
  font-size: var(--fs-caption);
  color: var(--text-3);
  margin-bottom: 6px;
}
.startup-cmd pre {
  margin: 0;
  background: var(--surface-2);
  border: 1px solid var(--border-1);
  border-radius: var(--radius-sm);
  padding: 10px 12px;
  font-size: 12px;
  line-height: 1.6;
  color: var(--text-2);
  overflow-x: auto;
  white-space: pre;
}

.startup-row {
  display: flex;
  align-items: center;
  gap: 16px;
  flex-wrap: wrap;
  padding: 14px 0;
  border-bottom: 1px solid var(--border-1);
}
.startup-row:last-of-type {
  border-bottom: 0;
}
.startup-row__text {
  flex: 1 1 auto;
  min-width: 220px;
}
.startup-row__title {
  font-size: var(--fs-body);
  font-weight: 600;
  color: var(--text-1);
}
.startup-row__desc {
  font-size: var(--fs-caption);
  color: var(--text-3);
  margin-top: 4px;
  line-height: 1.6;
}
.startup-row__actions {
  display: flex;
  gap: 8px;
}
.startup-action-note {
  margin: 12px 0 0;
  padding: 10px 12px;
  background: var(--ok-bg);
  border: 1px solid var(--ok-border);
  border-radius: var(--radius-sm);
  font-size: var(--fs-caption);
  color: var(--ok-fg);
  display: flex;
  align-items: center;
  gap: 8px;
}
.mono {
  font-family: var(--font-mono);
  font-variant-numeric: tabular-nums;
}
</style>
