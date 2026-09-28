import { describe, it, expect } from 'vitest';
import { extractErrorMessage } from './repo';

/**
 * `extractErrorMessage` 回归 + 新增能力测试。
 *
 * 背景：网关 daemon 在预绑定冲突时返回 HTTP 422，响应体形如
 * `{ error:'activation_rejected', message:'…', hint:'…' }`——`hint` 是后端算好的
 * 恢复路径。改动前该函数只读 `message`/`reason`/`error`，用户永远看不到 `hint`。
 */
describe('extractErrorMessage (后端错误体 → 人读消息)', () => {
  it('预绑定冲突形状：主消息与 hint 恢复路径同时出现', () => {
    const out = extractErrorMessage({
      error: 'activation_rejected',
      message: '激活被拒绝：该激活码已预绑定到另一台机器（一机一码约束）。',
      hint: '请确认本网关的机器码与授权后台「预绑定机器码」完全一致；若确属换机，请在授权后台提交换机工单并解绑旧机后，再重新激活。',
    });
    expect(out).toContain('激活被拒绝');
    expect(out).toContain('请确认');
    // 两段之间以「 恢复路径：」衔接
    expect(out).toContain(' 恢复路径：');
  });

  it('只有 hint（无 message/reason/error）也要返回恢复路径', () => {
    const out = extractErrorMessage({ hint: '请前往授权后台解绑旧机后重试。' });
    expect(out).toContain('恢复路径：');
    expect(out).toContain('请前往授权后台解绑旧机后重试。');
  });

  it('只有 message（无 hint）→ 输出与改动前逐字节相同', () => {
    const out = extractErrorMessage({
      message: '激活被拒绝：该激活码已预绑定到另一台机器（一机一码约束）。',
    });
    expect(out).toBe('——激活被拒绝：该激活码已预绑定到另一台机器（一机一码约束）。');
  });

  it('既有优先级不被破坏：reason 优先于 error', () => {
    expect(extractErrorMessage({ reason: '原因字段', error: '错误码字段' })).toBe('——原因字段');
    expect(extractErrorMessage({ message: '消息字段', reason: '原因字段', error: '错误码字段' })).toBe(
      '——消息字段',
    );
  });

  it('hint 为空串视为不存在（不产生多余后缀）', () => {
    expect(extractErrorMessage({ message: '消息字段', hint: '' })).toBe('——消息字段');
  });
});
