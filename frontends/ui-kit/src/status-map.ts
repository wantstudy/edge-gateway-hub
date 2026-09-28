/**
 * @file status-map.ts
 * @module ui-kit/status-map
 * @description 状态 → (文案, 色调) 的**唯一映射表**。
 *
 * 设计系统硬约束（`docs/design/ui-design-system.md` §3）：
 * 「每个状态固定色 + 固定文案，禁止各处自造同义词」。
 * 因此两端一切状态标签都必须经 `StatusTag` 渲染，不得在页面里手写颜色/文案。
 */

import type { Tone } from './tokens';

/** 状态标签的渲染结果。 */
export interface StatusView {
  /** 中文显示文案（固定，不允许同义替换） */
  readonly label: string;
  /** 语义色调 */
  readonly tone: Tone;
}

/**
 * 全量状态字典。key 为后端契约中的原始英文枚举值。
 *
 * 覆盖范围：授权码状态、设备/租约状态、回执健康度、用户状态、密钥状态。
 * 若后端返回未知枚举，`statusView()` 会降级为 `unknown` 色调并原样显示 key（便于排障），
 * 而不会静默丢字段。
 */
export const STATUS_MAP: Readonly<Record<string, StatusView>> = Object.freeze({
  // ---- 激活码状态（licensing-api.md §2.4 status） ----
  issued: { label: '已发放', tone: 'info' },
  bound: { label: '已绑定', tone: 'ok' },
  revoked: { label: '已废弃', tone: 'danger' },
  reissued: { label: '已重发', tone: 'warn' },

  // ---- 设备连接 / 租约状态 ----
  online: { label: '在线', tone: 'ok' },
  offline: { label: '离线', tone: 'danger' },
  error: { label: '故障', tone: 'danger' },
  reconnecting: { label: '重连中', tone: 'warn' },
  collect_failed: { label: '采集失败', tone: 'danger' },
  active: { label: '已授权', tone: 'ok' },
  trial: { label: '试用中', tone: 'warn' },
  trial_ending: { label: '试用将到期', tone: 'warn' },
  grace: { label: '宽限期', tone: 'warn' },
  degraded: { label: '已降级', tone: 'warn' },
  revoked_lease: { label: '授权失效', tone: 'danger' },
  inactive: { label: '未激活', tone: 'unknown' },

  // ---- 回执健康度 ----
  receipt_ok: { label: '正常', tone: 'ok' },
  receipt_gap: { label: '序号跳空', tone: 'warn' },
  receipt_missing: { label: '回执缺失', tone: 'danger' },
  receipt_rollback: { label: '序号回退', tone: 'danger' },
  receipt_bad_sig: { label: '签名无效', tone: 'danger' },
  receipt_delay: { label: '回执延迟', tone: 'info' },
  receipt_na: { label: '无（C 档）', tone: 'unknown' },

  // ---- 部署形态 ----
  native: { label: '原生', tone: 'unknown' },
  docker: { label: '容器', tone: 'info' },

  // ---- 处置 / 工单状态 ----
  pending: { label: '待处理', tone: 'warn' },
  pending_check: { label: '待核查', tone: 'warn' },
  processed: { label: '已处理', tone: 'ok' },
  verified: { label: '已核实', tone: 'ok' },
  dismissed: { label: '已忽略', tone: 'unknown' },

  // ---- 密钥状态 ----
  key_active: { label: '启用中（签发 + 验签）', tone: 'ok' },
  key_retiring: { label: '已退役（仅验签）', tone: 'warn' },
  key_retired: { label: '已退役', tone: 'warn' },
  key_disabled: { label: '已停用', tone: 'unknown' },

  // ---- 用户 / 角色 ----
  user_enabled: { label: '启用', tone: 'ok' },
  user_disabled: { label: '停用', tone: 'unknown' },

  // ---- 审计结果 ----
  success: { label: '成功', tone: 'ok' },
  denied: { label: '拒绝 403', tone: 'danger' },
  failed: { label: '失败', tone: 'danger' },
});

/**
 * 将后端原始状态值解析为展示对象。
 *
 * @param key 后端返回的状态原始值（如 `'bound'`）
 * @returns 固定的文案 + 色调；未知值降级为灰色并回显原始值
 */
export function statusView(key: string | null | undefined): StatusView {
  if (!key) {
    return { label: '—', tone: 'unknown' };
  }
  return STATUS_MAP[key] ?? { label: key, tone: 'unknown' };
}
