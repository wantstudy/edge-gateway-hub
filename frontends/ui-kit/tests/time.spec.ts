/**
 * @file time.spec.ts
 * @description `ui-kit/time` 时间格式化**唯一出口**的单元测试。
 *
 * 为什么必须有断言保护：项目红线「页面禁裸显时间戳」。历史缺陷是
 * 旧 `formatDateTime`（原在 `mask.ts`）走 `Number.isNaN(getTime()) → String(input)`，
 * 对 10/13/16/19 位纯数字 epoch 串 `new Date(str)` 一律 Invalid Date，
 * 于是**把原始数字串原样透传回页面**——「调了 formatDateTime 却仍裸显」的总根因。
 *
 * 本文件含**对照实验**（control experiment）：把旧算法内联进测试，断言旧算法
 * 确实产生裸显 / 精度丢失，从而证明本测试对缺陷敏感（不是橡皮图章）。
 */
import { describe, it, expect } from 'vitest';
import {
  TIME_PLACEHOLDER,
  parseTime,
  formatDateTime,
  formatDate,
  formatMonthDay,
  formatClock,
  formatRelative,
  relativeTime,
} from '../src/time';

/** 基准时刻：unix 秒 1759032000 = UTC 2025-09-28 04:00:00（+08 本地 12:00:00）。 */
const BASE_SECS = '1759032000';
const BASE_UTC_DATE = '2025-09-28';

/** 后端可能下发的四种 epoch 量级（同一时刻的不同精度表示）。 */
const EPOCH_VARIANTS: ReadonlyArray<readonly [string, string]> = [
  ['秒（10 位）', '1759032000'],
  ['毫秒（13 位）', '1759032000123'],
  ['微秒（16 位）', '1759032000123456'],
  ['纳秒（19 位）', '1759032000123456789'],
];

describe('parseTime（单位识别）', () => {
  it.each(EPOCH_VARIANTS)('%s → 解析到同一时刻', (_label, value) => {
    const d = parseTime(value);
    expect(d).not.toBeNull();
    expect((d as Date).toISOString()).toBe('2025-09-28T04:00:00.000Z');
  });

  it('已是可读串时幂等解析（不做二次换算）', () => {
    const d = parseTime('2025-09-28 12:00:00');
    expect(d).not.toBeNull();
    // 本地 12:00:00 → UTC 04:00:00（Asia/Shanghai）
    expect((d as Date).getHours()).toBe(12);
  });

  it('Date 实例直接透传；非法实例返回 null', () => {
    const ok = new Date(2025, 8, 28, 12, 0, 0);
    expect(parseTime(ok)?.getTime()).toBe(ok.getTime());
    expect(parseTime(new Date('nope'))).toBeNull();
  });

  it('缺失 / 占位 / 非法输入一律 null（不抛错）', () => {
    expect(parseTime(null)).toBeNull();
    expect(parseTime(undefined)).toBeNull();
    expect(parseTime('')).toBeNull();
    expect(parseTime('   ')).toBeNull();
    expect(parseTime(TIME_PLACEHOLDER)).toBeNull();
    expect(parseTime('not-a-date')).toBeNull();
  });

  it('纯数字但位数不可判定为 epoch → 诚实空态，绝不被 new Date() 猜成年份', () => {
    // V8: new Date('0') → 2000-01-01（把裸数字当「0 年」）；new Date('1234') → 1234 年
    expect(parseTime('0')).toBeNull();
    expect(parseTime('1234')).toBeNull();
    // 8 位 YYYYMMDD 若被当 epoch 秒会得出 1970-08-23 —— 同样必须拦住
    expect(parseTime('20260928')).toBeNull();
    expect(formatDateTime('0')).toBe(TIME_PLACEHOLDER);
    expect(formatDateTime('20260928')).toBe(TIME_PLACEHOLDER);
    expect(formatDate('20260928')).toBe(TIME_PLACEHOLDER);
  });

  it('9 位（1973–2001 年区间）秒级 epoch 仍可正常识别', () => {
    // 946684800 = 2000-01-01T00:00:00Z
    expect(formatDate('946684800')).toBe('2000-01-01');
  });

  it('超出 19 位的纯数字串拒绝解析（不猜单位）', () => {
    expect(parseTime('17590320001234567890')).toBeNull();
  });
});

describe('formatDate / formatMonthDay（UTC 锚点，跨时区稳定）', () => {
  it.each(EPOCH_VARIANTS)('%s → 同一天（unit 识别正确）', (_label, value) => {
    expect(formatDate(value)).toBe(BASE_UTC_DATE);
  });

  it('formatMonthDay 输出 MM-DD', () => {
    expect(formatMonthDay(BASE_SECS)).toBe('09-28');
  });

  it('实参 utc=false 时按本地时区渲染', () => {
    // 本地（Asia/Shanghai=UTC+8）应为 09-28 12:00 的同一天
    expect(formatDate(BASE_SECS, false)).toBe('2025-09-28');
  });

  it('缺失输入回落占位', () => {
    expect(formatDate(null)).toBe(TIME_PLACEHOLDER);
    expect(formatMonthDay('')).toBe(TIME_PLACEHOLDER);
  });
});

describe('formatDateTime / formatClock（本地时区事件时刻）', () => {
  it('Date 实参输出 YYYY-MM-DD HH:mm:ss', () => {
    expect(formatDateTime(new Date(2025, 8, 28, 12, 0, 5))).toBe('2025-09-28 12:00:05');
  });

  it.each(EPOCH_VARIANTS)('%s 格式化后绝不是原始数字串（红线：禁裸显）', (_label, value) => {
    const out = formatDateTime(value);
    expect(out).not.toBe(value);
    expect(out).toMatch(/^\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}$/);
  });

  it('formatClock 输出 HH:mm:ss', () => {
    expect(formatClock(new Date(2025, 8, 28, 12, 0, 5))).toBe('12:00:05');
  });

  it('缺失输入回落占位（而不是空串 / 数字串）', () => {
    expect(formatDateTime(null)).toBe(TIME_PLACEHOLDER);
    expect(formatDateTime('')).toBe(TIME_PLACEHOLDER);
    expect(formatClock(undefined)).toBe(TIME_PLACEHOLDER);
  });
});

describe('formatRelative / relativeTime（兼容别名）', () => {
  const now = new Date(2025, 8, 28, 12, 30, 0);

  it('给出秒 / 分 / 时 / 天档位', () => {
    expect(formatRelative(new Date(2025, 8, 28, 12, 30, 0), now)).toBe('0 秒前');
    expect(formatRelative(new Date(2025, 8, 28, 12, 24, 0), now)).toBe('6 分钟前');
    expect(formatRelative(new Date(2025, 8, 28, 10, 30, 0), now)).toBe('2 小时前');
    expect(formatRelative(new Date(2025, 8, 26, 12, 30, 0), now)).toBe('2 天前');
  });

  it('未来时刻回落「刚刚」', () => {
    expect(formatRelative(new Date(2025, 8, 28, 12, 31, 0), now)).toBe('刚刚');
  });

  it('relativeTime 是 formatRelative 的同义别名', () => {
    expect(relativeTime(new Date(2025, 8, 28, 12, 24, 0), now)).toBe('6 分钟前');
  });

  it('缺失输入回落占位', () => {
    expect(formatRelative(null, now)).toBe(TIME_PLACEHOLDER);
  });
});

// ===========================================================================
// 对照实验（control experiment）——证明测试对历史缺陷敏感
// ===========================================================================
describe('[对照实验] 旧实现确实裸显 epoch / 丢失精度', () => {
  /**
   * 旧实现（重构前 `mask.ts::formatDateTime` 的行为，逐行内联）。
   * 缺陷：纯数字 epoch 串 `new Date(str)` → Invalid Date → `String(input)` 原样透传。
   */
  function legacyFormatDateTime(input: unknown): string {
    if (input === null || input === undefined || input === '') {
      return TIME_PLACEHOLDER;
    }
    const d = input instanceof Date ? input : new Date(String(input));
    if (Number.isNaN(d.getTime())) {
      return String(input); // ← 缺陷：把 epoch 数字串原样返回页面
    }
    const p = (n: number): string => String(n).padStart(2, '0');
    return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
  }

  /** 旧实现（重构前 `mask.ts::formatDate` 若按本地时区渲染）—— 仅用于对照说明。 */
  function legacyParseByNumber(input: string): number {
    // 旧路径常见写法：Number(str) 后直接当秒
    return Math.floor(Number(input) / 1e9);
  }

  it('旧实现对四种 epoch 量级全部原样透传（裸显）', () => {
    for (const [, value] of EPOCH_VARIANTS) {
      expect(legacyFormatDateTime(value)).toBe(value); // 旧实现确实裸显
    }
  });

  it('新实现对同样输入格式化（测试能捕获该缺陷）', () => {
    for (const [, value] of EPOCH_VARIANTS) {
      const out = formatDateTime(value);
      expect(out).not.toBe(value);
      expect(out).toMatch(/^\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}$/);
    }
  });

  it('微秒（16 位）被旧逻辑按毫秒处理会得到荒谬年份 —— 新实现已修正', () => {
    const us = '1759032000123456';
    // 旧：把 16 位当毫秒 → 1759032000123456ms → 年份 ~57726，而非 2025
    const legacyYear = new Date(Number(us)).getUTCFullYear();
    expect(legacyYear).toBeGreaterThan(50000); // 证明旧口径确实错
    expect(formatDate(us)).toBe(BASE_UTC_DATE); // 新口径正确
  });

  it('19 位纳秒经 Number() 取秒会丢精度（±1 秒），BigInt 不会', () => {
    const ns = '1759032000999999999';
    const viaNumber = legacyParseByNumber(ns);
    const viaBigInt = Number(BigInt(ns) / 1_000_000_000n);
    expect(viaNumber).not.toBe(viaBigInt); // 精度丢失 → 证明必须用 BigInt
    // 新实现走 BigInt：秒值恒等于纳秒串截断后的整数秒
    expect(parseTime(ns)?.getTime()).toBe(viaBigInt * 1000);
  });

  it('旧实现的兜底分支会把 "0" 猜成 2000 年（伪造数据）—— 新实现返回占位', () => {
    // 旧实现：new Date('0') 在 V8 中被当作「0 年」→ 2000-01-01，于是页面显示一个假时间
    expect(legacyFormatDateTime('0')).toBe('2000-01-01 00:00:00');
    expect(formatDateTime('0')).toBe(TIME_PLACEHOLDER); // 新实现诚实空态
  });
});
