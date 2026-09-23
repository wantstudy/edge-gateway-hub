/**
 * @file mask.ts
 * @module ui-kit/mask
 * @description 激活码 / 机器码 / IP 的**掩码工具**（纯函数，可单测）。
 *
 * 安全要求（`docs/design/ui-admin-console.md` §6 验收要点）：
 * 「激活码明文默认掩码，查看需权限且记审计」。
 * 因此掩码不是装饰，而是默认态；揭示必须走显式动作（由页面负责，并上报审计）。
 */

/** 把 hex 片段中的第 idx 个字符替换为 `*`（用于机器码摘要打码）。 */
function starAt(hex: string, indices: readonly number[]): string {
  const chars = hex.split('');
  for (const i of indices) {
    if (i >= 0 && i < chars.length) {
      chars[i] = '*';
    }
  }
  return chars.join('');
}

/**
 * 激活码掩码。
 *
 * 输入形如 `IOT-2026-8C3F-1234-ABCD-A1`，输出形如 `IOTDAQ-****-****-****-A1`。
 * 规则：保留可识别的前缀段与**后 2 位**（后 8 位校验需要客户/客服可核对），中间全部打码。
 *
 * @param code 激活码原文（允许已是掩码态，函数幂等）
 * @returns 掩码后的激活码
 */
export function maskCode(code: string | null | undefined): string {
  if (!code) {
    return '—';
  }
  const parts = code.split('-').filter((p) => p.length > 0);
  if (parts.length === 0) {
    return '—';
  }
  // 尾段（含后 2 位明文）—— 这是掩码保留的唯一明文信息，用于人工核对尾部。
  const tail = parts[parts.length - 1];
  const visibleTail = tail.slice(-2);
  const prefix = parts[0] === 'IOT' ? 'IOTDAQ' : parts[0];
  return `${prefix}-****-****-****-${visibleTail}`;
}

/**
 * 激活码后 8 位（用于危险操作「对象名二次校验」）。
 *
 * 去掉分隔符后取最后 8 个字符并大写，便于与用户输入做**去分隔符**比较。
 *
 * @param code 激活码原文
 * @returns 去分隔符后的大写后 8 位
 */
export function codeTail8(code: string | null | undefined): string {
  if (!code) {
    return '';
  }
  const stripped = code.replace(/[^A-Za-z0-9]/g, '').toUpperCase();
  return stripped.slice(-8);
}

/**
 * 机器码摘要（掩码态）。
 *
 * @param summary 形如 `6f2a9b31c1` 的完整摘要
 * @returns 形如 `6f2a…c1` 的掩码摘要
 */
export function maskMachineSummary(summary: string | null | undefined): string {
  if (!summary) {
    return '—';
  }
  const s = summary.replace(/\s/g, '');
  if (s.length <= 4) {
    return starAt(s, s.split('').map((_, i) => i));
  }
  return `${s.slice(0, 4)}…${s.slice(-2)}`;
}

/**
 * 完整机器码分段显示（`8F3A-91C2-7D04-5BE6`）。
 *
 * @param raw 完整机器码（可能有/无分隔符）
 * @returns 每 4 位一段、`-` 连接的大写字符串
 */
export function formatMachineCode(raw: string | null | undefined): string {
  if (!raw) {
    return '—';
  }
  const stripped = raw.replace(/[^A-Za-z0-9]/g, '').toUpperCase();
  const groups = stripped.match(/.{1,4}/g) ?? [];
  return groups.join('-');
}

/**
 * 机器码「部分掩码」展示：保留前 4 与后 4，中间打码。
 *
 * 用于列表页（既不完整暴露，也能人工比对）。
 *
 * @param raw 完整机器码
 * @returns 形如 `8F3A-****-****-5BE6`
 */
export function maskMachineCode(raw: string | null | undefined): string {
  if (!raw) {
    return '—';
  }
  const formatted = formatMachineCode(raw);
  const parts = formatted.split('-');
  if (parts.length <= 2) {
    return formatted;
  }
  const masked = parts.map((p, i) => (i === 0 || i === parts.length - 1 ? p : '*'.repeat(p.length)));
  return masked.join('-');
}

/**
 * 来源 IP 掩码（审计页默认脱敏末段）。
 *
 * @param ip 形如 `10.20.3.14`
 * @returns 形如 `10.20.3.**`
 */
export function maskIp(ip: string | null | undefined): string {
  if (!ip) {
    return '—';
  }
  const parts = ip.split('.');
  if (parts.length !== 4) {
    return ip;
  }
  return `${parts[0]}.${parts[1]}.${parts[2]}.**`;
}

/** 激活码格式校验（放宽：允许字母数字与分隔符，长度 12–64）。 */
export const CODE_PATTERN = /^[A-Za-z0-9][A-Za-z0-9-]{10,62}[A-Za-z0-9]$/;

/** 机器码格式校验：去分隔符后必须是 8–64 位 hex。 */
export const MACHINE_CODE_PATTERN = /^[0-9A-Fa-f]{8,64}$/;

/**
 * 校验机器码格式。
 *
 * @param raw 用户输入
 * @returns 合法返回 true
 */
export function isValidMachineCode(raw: string | null | undefined): boolean {
  if (!raw) {
    return false;
  }
  return MACHINE_CODE_PATTERN.test(raw.replace(/[^A-Za-z0-9]/g, ''));
}

/** 时间统一显示格式（设计系统 §5：`YYYY-MM-DD HH:mm:ss`）。 */
export function formatDateTime(input: string | number | Date | null | undefined): string {
  if (input === null || input === undefined || input === '') {
    return '—';
  }
  const d = input instanceof Date ? input : new Date(input);
  if (Number.isNaN(d.getTime())) {
    return String(input);
  }
  const p = (n: number): string => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

/**
 * 相对时间（「6 分钟前」），用于表格辅助列与悬停提示。
 *
 * @param input 时间
 * @param now 基准时间（便于测试注入）
 */
export function relativeTime(input: string | number | Date | null | undefined, now: Date = new Date()): string {
  if (input === null || input === undefined || input === '') {
    return '—';
  }
  const d = input instanceof Date ? input : new Date(input);
  if (Number.isNaN(d.getTime())) {
    return '—';
  }
  const diffSec = Math.floor((now.getTime() - d.getTime()) / 1000);
  if (diffSec < 0) {
    return '刚刚';
  }
  if (diffSec < 60) {
    return `${diffSec} 秒前`;
  }
  if (diffSec < 3600) {
    return `${Math.floor(diffSec / 60)} 分钟前`;
  }
  if (diffSec < 86400) {
    return `${Math.floor(diffSec / 3600)} 小时前`;
  }
  return `${Math.floor(diffSec / 86400)} 天前`;
}
