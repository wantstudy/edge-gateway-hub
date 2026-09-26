/**
 * 把 10 位 epoch 秒或 13 位 epoch 毫秒格式化为本地日期时间。
 *
 * JSON 大数红线：只在纯 10/13 位数字通过正则校验后进行 Number 转换，
 * 且转换结果仅传给 Date 用于展示。uint64 / 纳秒时间戳等超过 15 位的字符串
 * 绝不数值化，统一返回空态，避免精度损失或把原始时间戳暴露到页面。
 */
export function formatTimestampText(value: string | number | null | undefined): string {
  if (value === null || value === undefined) {
    return '—';
  }

  const text = String(value).trim();
  if (!text) {
    return '—';
  }

  const millisecondsText = /^\d{10}$/.test(text) ? `${text}000` : /^\d{13}$/.test(text) ? text : '';
  if (!millisecondsText) {
    return '—';
  }

  const milliseconds = Number(millisecondsText);
  if (!Number.isFinite(milliseconds) || milliseconds <= 0) {
    return '—';
  }

  const date = new Date(milliseconds);
  if (Number.isNaN(date.getTime())) {
    return '—';
  }

  const pad = (part: number): string => String(part).padStart(2, '0');
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`;
}

/**
 * epoch 纳秒字符串（SSE 帧 `ts` / 审计 `ts_ns`，约 19 位）→ `YYYY-MM-DD HH:mm:ss`。
 *
 * JSON 大数红线：纳秒串远超 `Number` 安全整数区间，**绝不整串数值化**；只截取秒段
 * （去掉末 9 位）后交给 `formatTimestampText`。10 位秒 / 13 位毫秒按原样识别；
 * 非纯数字或位数不足（无法判定位意）一律回 `—`。
 */
export function formatNanoTimestampText(value: string | number | null | undefined): string {
  if (value === null || value === undefined) {
    return '—';
  }

  const text = String(value).trim();
  if (!/^\d+$/.test(text)) {
    return '—';
  }

  // >13 位视为纳秒：截掉末 9 位得到 10 位秒段，全程不把整串转成 Number。
  const secondsText = text.length > 13 ? text.slice(0, -9) : text;
  return formatTimestampText(secondsText);
}

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
    const formatted = run.length === 19 ? formatNanoTimestampText(run) : formatTimestampText(run);
    if (formatted === '—') {
      return run;
    }
    const year = Number(formatted.slice(0, 4));
    return year >= 2000 && year <= 2100 ? formatted : run;
  });
}
