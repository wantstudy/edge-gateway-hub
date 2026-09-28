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
 * 机器码**提交归一**（与后端 `normalize_machine_code` **同一套规则**）。
 *
 * # 为什么提交前必须归一
 * 机器码存在两种**等价写法**：
 * - **展示态**：分段大写、带 `-` 分隔（如 `8F3A-91C2-7D04-5BE6`，见
 *   [`formatMachineCode`](./mask.ts)）——运维会从网关控制台复制它；
 * - **匹配态**：无分隔符小写（如 `8f3a91c27d045be6`）——网关上报的即为此态。
 *
 * 若展示态被原样提交，服务端存下带 `-` 的值，与网关上报的无分隔符值精确比较不等，
 * 「同机」恒判「异机」→ `PREBIND_CONFLICT`（HTTP 422）。故提交侧必须归一到匹配态。
 *
 * 规则：剥 `-` `:` `_` 与所有空白 + 小写。**仅**剥离这套「纯排版字符」，不改动任何
 * 十六进制字符本身——实质不同的机器码仍保持不同（一机一码红线）。
 * 展示 / 复制仍用 [`formatMachineCode`](./mask.ts)，二者互补、全链路归一后等价。
 *
 * @param raw 用户输入或从网关复制来的机器码
 * @returns 无分隔符小写机器码（幂等：已归一输入再归一不变）
 */
export function normalizeMachineCodeInput(raw: string): string {
  return raw.replace(/[-:_\s]/g, '').toLowerCase();
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

/**
 * 时间统一显示格式（设计系统 §5：`YYYY-MM-DD HH:mm:ss`）。
 *
 * **实现已迁移至 [`./time`](./time.ts)**：旧实现在 `new Date(epoch 数字串)` 解析失败时
 * `return String(input)`，把 10 位 epoch 串原样透传回页面（裸显根因）。此处仅保留
 * 再导出，保持既有导入路径（`@ui-kit` → `./mask`）不变。
 */
export {
  formatDateTime,
  formatDate,
  formatClock,
  formatRelative,
  relativeTime,
  parseTime,
  TIME_PLACEHOLDER,
} from './time';
