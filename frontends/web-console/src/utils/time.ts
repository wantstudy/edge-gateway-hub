/**
 * @file time.ts
 * @module web-console/utils/time
 * @description 时间戳展示格式化（**严禁页面裸显 epoch**）。
 *
 * **实现已收敛到 [`@ui-kit/time`](../../../ui-kit/src/time.ts) 单一出口**：
 * 网关端（web-console）与授权端（admin-console）共用同一套
 * 「按位数识别单位（秒/毫秒/微秒/纳秒） + BigInt 取秒 + 位数不可判定即诚实空态」
 * 的逻辑，避免出现两份漂移口径。
 *
 * 这里保留历史函数名（`formatTimestampText` / `formatNanoTimestampText`）纯粹为了
 * 兼容既有调用点，底层逻辑与页面完全同源。
 */
import { formatDateTime, TIME_PLACEHOLDER } from '@ui-kit/time';

export { formatDateTime, formatDateTime as formatTimestampText, TIME_PLACEHOLDER } from '@ui-kit/time';

/**
 * epoch 纳秒字符串（SSE 帧 `ts` / 审计 `ts_ns`，约 19 位）→ `YYYY-MM-DD HH:mm:ss`。
 *
 * 新实现**原生支持** 19 位纳秒（按位数识别单位 + BigInt 整除，全程不把整串数值化），
 * 故本函数即 [`formatTimestampText`] 的同义别名；非纯数字 / 位数不可判定时回 `—`。
 */
export const formatNanoTimestampText = formatDateTime;

/** 自由文本中可识别的内嵌 epoch 数字段：19 位纳秒 / 13 位毫秒 / 10 位秒（词边界精确匹配）。 */
const EMBEDDED_EPOCH_RE = /(?<!\d)(\d{19}|\d{13}|\d{10})(?!\d)/g;

/**
 * 把**自由文本中内嵌的** epoch 时间戳替换为可读文本。
 *
 * 场景：后端把带时间戳的标识符塞进 `detail` 原文，如审计
 * `create device "dev-1790381277499"`、备份文件名 `config.toml.bak-1790436890`。
 * 直接上屏即以「时间戳形式」暴露时间。
 *
 * 边界：只认**恰好** 10 / 13 / 19 位、且能构成合理日期（2000–2100 年）的连续数字段；
 * 其余数字段（如 `frequency_ms=1000`、`config_version=2`）一律原样保留，
 * 绝不做无边界模糊匹配。
 */
export function formatEmbeddedTimestamps(text: string): string {
  if (!text) {
    return text;
  }
  return text.replace(EMBEDDED_EPOCH_RE, (run) => {
    const formatted = formatDateTime(run);
    if (formatted === TIME_PLACEHOLDER) {
      return run;
    }
    const year = Number(formatted.slice(0, 4));
    return year >= 2000 && year <= 2100 ? formatted : run;
  });
}
