/**
 * @file time.ts
 * @module admin-console/utils/time
 * @description 时间戳展示格式化（**严禁页面裸显 epoch**）。
 *
 * 后端（licensing-server）的时间字段一律以 **String** 原文下发（大数红线：
 * uint64 / unix 秒不进 JSON number）。可能是：
 *   · unix 秒（10 位）/ 毫秒（13 位）/ 微秒（16 位）/ 纳秒（19 位）纯数字串；
 *   · 已经是可读时间串（幂等原样返回）。
 *
 * 本模块统一把纯数字 epoch 串按位数识别单位、经 **BigInt** 取秒（**绝不 `Number()`
 * 大数**，避免 2^53 精度丢失）→ 本地时区格式化为 `YYYY-MM-DD HH:mm:ss`。
 */

/** 纯数字串判定（长度 ≥ 8 才视为时间戳；`—` / 空串不算）。 */
function isEpochDigits(text: string): boolean {
  return /^\d{8,}$/.test(text);
}

/** 两位补零。 */
function pad2(value: number): string {
  return String(value).padStart(2, '0');
}

/**
 * 把后端时间字段格式化为本地可读串（`YYYY-MM-DD HH:mm:ss`）。
 *
 * @param raw 后端下发的时间（String；epoch 数字串或可读串）
 * @param placeholder 缺失 / 空值时的占位（默认 `—`）
 */
export function formatTimestampText(raw: unknown, placeholder = '—'): string {
  if (raw === null || raw === undefined) {
    return placeholder;
  }
  const text = typeof raw === 'string' ? raw.trim() : String(raw).trim();
  if (text === '' || text === '—') {
    return placeholder;
  }
  if (!isEpochDigits(text)) {
    // 已是可读时间串：原样透传（幂等，绝不二次加工）
    return text;
  }
  // 按位数识别单位并取「秒」（BigInt 除法：整数秒恒在安全整数范围内）。
  const divisor = text.length >= 18 ? 1_000_000_000n : text.length >= 12 ? 1_000n : 1n;
  let secs: number;
  try {
    secs = Number(BigInt(text) / divisor);
  } catch {
    return text;
  }
  const date = new Date(secs * 1000);
  if (Number.isNaN(date.getTime())) {
    return text;
  }
  return (
    `${date.getFullYear()}-${pad2(date.getMonth() + 1)}-${pad2(date.getDate())} ` +
    `${pad2(date.getHours())}:${pad2(date.getMinutes())}:${pad2(date.getSeconds())}`
  );
}
