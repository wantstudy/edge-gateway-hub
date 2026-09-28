/**
 * @file mask.spec.ts
 * @description 掩码 / 校验 / 时间格式化工具的单元测试。
 *              这些是安全相关纯函数（激活码掩码是默认态），必须有断言保护。
 */
import { describe, it, expect } from 'vitest';
import {
  maskCode,
  codeTail8,
  maskMachineSummary,
  formatMachineCode,
  maskMachineCode,
  normalizeMachineCodeInput,
  maskIp,
  isValidMachineCode,
  formatDateTime,
  relativeTime,
} from '../src/mask';

describe('maskCode（激活码掩码）', () => {
  it('保留前缀与后 2 位，中间全打码', () => {
    expect(maskCode('IOT-2026-8C3F-1234-ABCD-A1')).toBe('IOTDAQ-****-****-****-A1');
  });

  it('对已是掩码态输入保持幂等（不叠加、不报错）', () => {
    const once = maskCode('IOT-2026-8C3F-1234-ABCD-A1');
    const twice = maskCode(once);
    expect(twice).toBe('IOTDAQ-****-****-****-A1');
    expect(once).toBe(twice);
  });

  it('空值返回占位符而不是抛错', () => {
    expect(maskCode(null)).toBe('—');
    expect(maskCode('')).toBe('—');
    expect(maskCode(undefined)).toBe('—');
  });

  it('非 IOT 前缀时保留原首段（便于辨识第三方码）', () => {
    expect(maskCode('ACME-1111-2222-3333-ZZ')).toBe('ACME-****-****-****-ZZ');
  });
});

describe('codeTail8（危险操作二次校验）', () => {
  it('去分隔符后取大写后 8 位', () => {
    expect(codeTail8('IOT-2025-1D90-ABCD-3K')).toBe('90ABCD3K');
  });

  it('长度不足时返回全部（不补位、不报错）', () => {
    expect(codeTail8('AB-12')).toBe('AB12');
  });

  it('空值返回空串', () => {
    expect(codeTail8(null)).toBe('');
    expect(codeTail8('')).toBe('');
  });
});

describe('机器码工具', () => {
  it('formatMachineCode 每 4 位分段并大写', () => {
    expect(formatMachineCode('8f3a91c27d045be6')).toBe('8F3A-91C2-7D04-5BE6');
  });

  it('maskMachineCode 仅保留首尾段', () => {
    expect(maskMachineCode('8f3a91c27d045be6')).toBe('8F3A-****-****-5BE6');
  });

  it('maskMachineSummary 保留前 4 后 2', () => {
    expect(maskMachineSummary('6f2a9b31c1')).toBe('6f2a…c1');
  });

  it('isValidMachineCode 对 hex 校验', () => {
    expect(isValidMachineCode('C1D2-4E5F-6A7B-8C9D')).toBe(true);
    expect(isValidMachineCode('ZZZZ')).toBe(false);
    expect(isValidMachineCode('')).toBe(false);
    expect(isValidMachineCode(null)).toBe(false);
  });

  it('maskIp 脱敏末段', () => {
    expect(maskIp('10.20.3.14')).toBe('10.20.3.**');
    expect(maskIp('bad-ip')).toBe('bad-ip');
    expect(maskIp(null)).toBe('—');
  });
});

describe('normalizeMachineCodeInput（提交归一：展示态 → 匹配态）', () => {
  it('带 `-` 的大写展示态归一为无分隔符小写匹配态', () => {
    expect(normalizeMachineCodeInput('8F3A-91C2-7D04-5BE6')).toBe('8f3a91c27d045be6');
    expect(normalizeMachineCodeInput('8f3a91c27d045be6')).toBe('8f3a91c27d045be6');
  });

  it('剥离 `:` `_` 与前后 / 内部空白', () => {
    expect(normalizeMachineCodeInput('  8f3a:91c2:7d04:5be6  ')).toBe('8f3a91c27d045be6');
    expect(normalizeMachineCodeInput('8F3A_91C2_7D04_5BE6')).toBe('8f3a91c27d045be6');
    expect(normalizeMachineCodeInput('8f3a 91c2 7d04 5be6')).toBe('8f3a91c27d045be6');
  });

  it('幂等：对已归一输入再归一不变', () => {
    const once = normalizeMachineCodeInput('8F3A-91C2-7D04-5BE6');
    expect(normalizeMachineCodeInput(once)).toBe(once);
  });

  it('红线：实质不同的机器码归一后仍不相等（不做模糊匹配）', () => {
    expect(normalizeMachineCodeInput('8f3a91c27d045be6')).not.toBe(
      normalizeMachineCodeInput('8f3a91c27d045be7'),
    );
  });
});

describe('时间格式化', () => {
  it('formatDateTime 输出 YYYY-MM-DD HH:mm:ss', () => {
    const d = new Date(2026, 8, 23, 12, 25, 6); // 2026-09-23 12:25:06
    expect(formatDateTime(d)).toBe('2026-09-23 12:25:06');
  });

  it('formatDateTime 对空值返回占位符', () => {
    expect(formatDateTime(null)).toBe('—');
    expect(formatDateTime('')).toBe('—');
  });

  it('relativeTime 给出现场可读的相对时间（分钟/小时/天）', () => {
    const now = new Date(2026, 8, 23, 12, 30, 0);
    expect(relativeTime(new Date(2026, 8, 23, 12, 24, 0), now)).toBe('6 分钟前');
    expect(relativeTime(new Date(2026, 8, 23, 10, 30, 0), now)).toBe('2 小时前');
    expect(relativeTime(new Date(2026, 8, 21, 12, 30, 0), now)).toBe('2 天前');
  });
});
