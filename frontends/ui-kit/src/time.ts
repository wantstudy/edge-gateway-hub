/**
 * @file time.ts
 * @module ui-kit/time
 * @description 时间格式化**唯一出口**（web-console / admin-console 两端共用）。
 *
 * 后端时间字段一律以 **JSON 字符串**下发（大数红线：uint64 / unix 秒绝不进 JSON
 * number，见 `crates/protocol-proto` 头注释）。可能是：
 *   · unix **秒**（10 位）/ **毫秒**（13 位）/ **微秒**（16 位）/ **纳秒**（19 位）纯数字串；
 *   · 已经是可读时间串（幂等，`new Date()` 可解析）。
 *
 * 本模块把纯数字 epoch 串按**位数**识别单位、经 **BigInt** 取秒（**绝不 `Number()` 大数**，
 * 避免 2^53 精度丢失）→ 格式化为可读串。
 *
 * # 红线
 * 页面**严禁**裸显 epoch 串。任何非法 / 缺失输入一律回落占位 `—`，
 * **绝不**把原始数字串透传回页面 —— 这正是过去「调了 `formatDateTime` 却仍裸显」
 * 的总根因（旧实现 `Number.isNaN → String(input)` 把 10 位 epoch 原样返回）。
 */

/** 缺失 / 非法时间的统一占位。 */
export const TIME_PLACEHOLDER = '—';

/** 两位补零。 */
function pad2(value: number): string {
  return String(value).padStart(2, '0');
}

/**
 * 纯数字串判定：**9–19 位**才视为 epoch 时间戳（秒 / 毫秒 / 微秒 / 纳秒）。
 *
 * 下限取 9（覆盖 1973 年起的秒级时间戳），上限取 19（纳秒）。
 * **下界不可放宽**：`new Date()` 会把 1–4 位纯数字当「年份」解析
 * （`new Date('0')` → 2000-01-01），把 8 位纯数字当非法年份，
 * 于是 `'0'`、`'20260928'` 这类非时间串会被**猜**成看似合理的日期。
 */
function isEpochDigits(text: string): boolean {
  return /^\d{9,19}$/.test(text);
}

/** 纯数字（任意位数）—— 位数不在 epoch 区间时禁止交给 `new Date()` 猜。 */
function isAllDigits(text: string): boolean {
  return /^\d+$/.test(text);
}

/**
 * 把后端时间字段解析为 `Date`（解析失败 / 缺失 → `null`）。
 *
 * @param input epoch 数字串（秒/毫秒/微秒/纳秒）/ 可读时间串 / `Date` / 数字
 */
export function parseTime(input: unknown): Date | null {
  if (input === null || input === undefined) {
    return null;
  }
  if (input instanceof Date) {
    return Number.isNaN(input.getTime()) ? null : input;
  }
  const text = typeof input === 'string' ? input.trim() : String(input).trim();
  if (text === '' || text === TIME_PLACEHOLDER) {
    return null;
  }
  if (isEpochDigits(text)) {
    // 按位数识别单位（当前纪元的量级：秒 10 位 / 毫秒 13 位 / 微秒 16 位 / 纳秒 19 位）：
    //   ≥18 → 纳秒；15–17 → 微秒；12–14 → 毫秒；其余（8–11）→ 秒。
    // BigInt 整除恒为整数秒，**绝不 Number() 大数**（19 位纳秒转 Number 会丢精度，
    // 极端情况下跨秒导致 ±1 秒偏差）。
    const divisor = text.length >= 18 ? 1_000_000_000n : text.length >= 15 ? 1_000_000n : text.length >= 12 ? 1_000n : 1n;
    let secs: number;
    try {
      secs = Number(BigInt(text) / divisor);
    } catch {
      return null;
    }
    const fromEpoch = new Date(secs * 1000);
    return Number.isNaN(fromEpoch.getTime()) ? null : fromEpoch;
  }
  if (isAllDigits(text)) {
    // 纯数字但位数不可判定为 epoch（如 `0`、`20260928`）：诚实空态。
    // **绝不**交给 `new Date()` —— 它会把 `'0'` 猜成 2000-01-01，制造假数据。
    return null;
  }
  const parsed = new Date(text);
  return Number.isNaN(parsed.getTime()) ? null : parsed;
}

/**
 * 格式化为 `YYYY-MM-DD HH:mm:ss`（**本地时区**；用于事件时刻）。
 *
 * @param input 后端时间字段
 * @param placeholder 缺失 / 非法时的占位（默认 `—`）
 */
export function formatDateTime(input: unknown, placeholder = TIME_PLACEHOLDER): string {
  const d = parseTime(input);
  if (!d) {
    return placeholder;
  }
  return (
    `${d.getFullYear()}-${pad2(d.getMonth() + 1)}-${pad2(d.getDate())} ` +
    `${pad2(d.getHours())}:${pad2(d.getMinutes())}:${pad2(d.getSeconds())}`
  );
}

/**
 * 格式化为 `YYYY-MM-DD`。
 *
 * @param utc `true`（默认）按 **UTC** 锚点渲染 —— 用于「有效期 / 日期边界」这类
 *            按 UTC 语义存储的值，若用本地时区渲染会在东八区之外偏移一天；
 *            传 `false` 则按本地时区。
 */
export function formatDate(input: unknown, utc = true, placeholder = TIME_PLACEHOLDER): string {
  const d = parseTime(input);
  if (!d) {
    return placeholder;
  }
  const y = utc ? d.getUTCFullYear() : d.getFullYear();
  const m = utc ? d.getUTCMonth() + 1 : d.getMonth() + 1;
  const day = utc ? d.getUTCDate() : d.getDate();
  return `${y}-${pad2(m)}-${pad2(day)}`;
}

/**
 * 格式化为 `MM-DD`（图表 X 轴紧凑标签）。
 *
 * @param utc `true`（默认）按 **UTC** 锚点渲染 —— 后端「按日聚合」的 date 字段是
 *            「该日 00:00:00 UTC 的 unix 秒」，用本地时区渲染会在东八区之外偏一天。
 */
export function formatMonthDay(input: unknown, utc = true, placeholder = TIME_PLACEHOLDER): string {
  const d = parseTime(input);
  if (!d) {
    return placeholder;
  }
  const m = utc ? d.getUTCMonth() + 1 : d.getMonth() + 1;
  const day = utc ? d.getUTCDate() : d.getDate();
  return `${pad2(m)}-${pad2(day)}`;
}

/** 格式化为 `HH:mm:ss`（**本地时区**）。 */
export function formatClock(input: unknown, placeholder = TIME_PLACEHOLDER): string {
  const d = parseTime(input);
  if (!d) {
    return placeholder;
  }
  return `${pad2(d.getHours())}:${pad2(d.getMinutes())}:${pad2(d.getSeconds())}`;
}

/**
 * 相对时间（「6 分钟前」），用于表格辅助列与悬停提示。
 *
 * @param now 基准时间（便于测试注入）
 */
export function formatRelative(
  input: unknown,
  now: Date = new Date(),
  placeholder = TIME_PLACEHOLDER,
): string {
  const d = parseTime(input);
  if (!d) {
    return placeholder;
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

/** 兼容别名（旧 API `relativeTime(input, now?)`）。 */
export const relativeTime = formatRelative;
