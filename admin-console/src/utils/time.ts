/**
 * @file time.ts
 * @module admin-console/utils/time
 * @description 时间戳展示格式化（**严禁页面裸显 epoch**）。
 *
 * **实现已收敛到 [`@ui-kit/time`](../../../ui-kit/src/time.ts) 单一出口**：
 * 后端时间字段一律为 JSON 字符串（unix 秒/毫秒/微秒/纳秒，或已是可读串），
 * 统一按位数识别单位 + **BigInt** 取秒（绝不 `Number()` 大数）后格式化。
 *
 * 这里保留 `formatTimestampText` 别名纯粹为了兼容既有调用点
 * （`api/repo.ts` 等在 API 层做字段预格式化），底层逻辑与页面完全同源。
 */
export {
  formatDateTime,
  formatDateTime as formatTimestampText,
  formatDate,
  formatMonthDay,
  formatClock,
  formatRelative,
  parseTime,
  TIME_PLACEHOLDER,
} from '@ui-kit/time';
