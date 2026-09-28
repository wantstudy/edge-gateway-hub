/**
 * @file danger-modal.spec.ts
 * @description DangerConfirmModal 的**行为正确性**测试（本项目最高风险组件）。
 *
 * 覆盖验收要点：
 *  - 原因未选 / 补充说明 < 10 字 / 后 8 位不符 / 第二审批人缺失 → 提交按钮**禁用**
 *  - 四要素齐备 → 可提交，且提交 payload 正确
 *  - 关闭后再打开 → 草稿被清空（不「复活」）
 *
 * 实现说明：组件使用 `<Teleport to="body">` 渲染弹窗，因此断言必须查询 `document.body`
 * 而不是 wrapper 的根元素（否则会得到空文本）。
 */
import { describe, it, expect, beforeEach } from 'vitest';
import { mount, type VueWrapper } from '@vue/test-utils';
import { nextTick } from 'vue';
import DangerConfirmModal from '../src/components/DangerConfirmModal.vue';

/** 测试用完整激活码；后 8 位 = `90ABCD3K`。 */
const CODE = 'IOT-2025-1D90-ABCD-3K';

/** 默认 props。 */
function baseProps(overrides: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    open: true,
    title: '废弃激活码',
    impacts: ['该设备将立即停止北向转发', '本操作不可撤销'],
    reasons: ['客户更换硬件', '设备报废'],
    confirmValue: CODE,
    confirmText: '废弃激活码',
    ...overrides,
  };
}

/**
 * 挂载组件并返回便捷查询器。
 * 弹窗内容在 `document.body` 中，因此所有查询走 `document`。
 */
function setup(overrides: Record<string, unknown> = {}): {
  wrapper: VueWrapper;
  submitBtn: () => HTMLButtonElement | null;
  noteArea: () => HTMLTextAreaElement | null;
  textInputs: () => HTMLInputElement[];
  bodyText: () => string;
} {
  const wrapper = mount(DangerConfirmModal, { props: baseProps(overrides), attachTo: document.body });

  return {
    wrapper,
    submitBtn: () =>
      Array.from(document.querySelectorAll<HTMLButtonElement>('.uik-dcm__foot button')).find(
        (b) => b.textContent?.trim() === String(overrides.confirmText ?? '废弃激活码'),
      ) ?? null,
    noteArea: () => document.querySelector<HTMLTextAreaElement>('.uik-dcm__body textarea'),
    textInputs: () => Array.from(document.querySelectorAll<HTMLInputElement>('.uik-dcm__body input[type="text"]')),
    bodyText: () => document.body.textContent ?? '',
  };
}

/** 设置原生控件值并派发 input 事件（触发 Vue 的 v-model）。 */
async function setNativeValue(el: HTMLInputElement | HTMLTextAreaElement, value: string): Promise<void> {
  el.value = value;
  el.dispatchEvent(new Event('input', { bubbles: true }));
  await nextTick();
}

/** 选择原因下拉的某一项（`baseProps` 默认把首项作为默认值预选）。 */
async function chooseReason(value: string): Promise<void> {
  const select = document.querySelector<HTMLSelectElement>('.uik-dcm__body select');
  expect(select).not.toBeNull();
  select!.value = value;
  select!.dispatchEvent(new Event('change', { bubbles: true }));
  await nextTick();
}

beforeEach(() => {
  // 清空上一次测试残留的 Teleport 内容
  document.body.innerHTML = '';
});

describe('DangerConfirmModal —— 危险操作四要素', () => {
  it('初始状态提交按钮禁用（说明与后 8 位为空）', () => {
    const { submitBtn } = setup();
    expect(submitBtn()).not.toBeNull();
    expect(submitBtn()!.disabled).toBe(true);
  });

  it('三要素齐备后提交按钮解除禁用', async () => {
    const { submitBtn, noteArea, textInputs } = setup();
    await setNativeValue(noteArea()!, '客户主板损坏已寄回厂商检修');
    const inputs = textInputs();
    await setNativeValue(inputs[inputs.length - 1], '90ABCD3K');
    expect(submitBtn()!.disabled).toBe(false);
  });

  it('补充说明不足 10 字时提交被禁用', async () => {
    const { submitBtn, noteArea, textInputs } = setup();
    await setNativeValue(noteArea()!, '太短了');
    const inputs = textInputs();
    await setNativeValue(inputs[inputs.length - 1], '90ABCD3K');
    expect(submitBtn()!.disabled).toBe(true);
  });

  it('后 8 位不符时提交被禁用，并给出字段级错误', async () => {
    const { submitBtn, noteArea, textInputs, bodyText } = setup();
    await setNativeValue(noteArea()!, '客户主板损坏已寄回厂商检修');
    const inputs = textInputs();
    await setNativeValue(inputs[inputs.length - 1], 'WRONG888');
    expect(submitBtn()!.disabled).toBe(true);
    expect(bodyText()).toContain('与对象标识后 8 位不一致');
  });

  it('后 8 位校验容忍小写与分隔符差异', async () => {
    const { submitBtn, noteArea, textInputs } = setup();
    await setNativeValue(noteArea()!, '客户主板损坏已寄回厂商检修');
    const inputs = textInputs();
    await setNativeValue(inputs[inputs.length - 1], '90ab-cd3k');
    expect(submitBtn()!.disabled).toBe(false);
  });

  it('双人复核开启时第二审批人缺失则禁用，填写后解除', async () => {
    const { submitBtn, noteArea, textInputs, bodyText } = setup({ requireSecondApprover: true });
    await setNativeValue(noteArea()!, '客户主板损坏已寄回厂商检修');
    // 双人复核开启时会出现两个文本输入：后 8 位校验 + 第二审批人（顺序：后 8 位在前）
    const inputs = textInputs();
    expect(inputs.length).toBe(2);
    const [tailInput, approverInput] = inputs;
    await setNativeValue(tailInput, '90ABCD3K');
    // 第二审批人字段必须存在且提交被禁用
    expect(bodyText()).toContain('第二审批人');
    expect(submitBtn()!.disabled).toBe(true);
    await setNativeValue(approverInput, 'wang.gong');
    expect(submitBtn()!.disabled).toBe(false);
  });

  it('提交后向上冒泡完整 payload', async () => {
    const { wrapper, submitBtn, noteArea, textInputs } = setup();
    await setNativeValue(noteArea()!, '客户主板损坏已寄回厂商检修');
    const inputs = textInputs();
    await setNativeValue(inputs[inputs.length - 1], '90ABCD3K');
    submitBtn()!.click();
    await nextTick();
    const emitted = wrapper.emitted('submit');
    expect(emitted).toBeTruthy();
    const payload = emitted![0][0] as Record<string, string>;
    expect(payload.note).toBe('客户主板损坏已寄回厂商检修');
    expect(payload.tail).toBe('90ABCD3K');
    expect(payload.reason).toBe('客户更换硬件');
  });

  it('业务侧 disabled=true 时无论如何都不能提交', async () => {
    const { submitBtn, noteArea, textInputs } = setup({ disabled: true });
    await setNativeValue(noteArea()!, '客户主板损坏已寄回厂商检修');
    const inputs = textInputs();
    await setNativeValue(inputs[inputs.length - 1], '90ABCD3K');
    expect(submitBtn()!.disabled).toBe(true);
  });
});

describe('DangerConfirmModal —— 三字段独立（reason / note / confirm）', () => {
  it('改写原因枚举后，note 仍保持原样（note 不得被拼进 reason）', async () => {
    const { wrapper, submitBtn, noteArea, textInputs } = setup();
    await setNativeValue(noteArea()!, '设备已过保修期，客户申请报废处理');
    const inputs = textInputs();
    await setNativeValue(inputs[inputs.length - 1], '90ABCD3K');
    await chooseReason('设备报废');

    submitBtn()!.click();
    await nextTick();
    const payload = wrapper.emitted('submit')![0][0] as Record<string, string>;

    expect(payload.reason).toBe('设备报废');
    expect(payload.note).toBe('设备已过保修期，客户申请报废处理');
    // reason 文本不得被卷进 note，反之亦然：两者必须各自独立成字段
    expect(payload.note).not.toContain('设备报废');
    expect(payload.reason).not.toContain('设备已过保修期');
  });

  it('tail8 模式也要随 payload 发出 confirm（否则二次校验在调用侧被静默丢弃）', async () => {
    const { wrapper, submitBtn, noteArea, textInputs } = setup();
    await setNativeValue(noteArea()!, '客户主板损坏已寄回厂商检修');
    const inputs = textInputs();
    await setNativeValue(inputs[inputs.length - 1], '90ABCD3K');
    submitBtn()!.click();
    await nextTick();
    const payload = wrapper.emitted('submit')![0][0] as Record<string, string>;
    expect(payload.confirm).toBe('90ABCD3K');
  });
});

describe('DangerConfirmModal —— tail8 口径对非激活码标识同样成立', () => {
  it('confirmValue 为 kid 类短标识时，比对的是归一化后 8 位，照抄全名过不了', async () => {
    // 回归闸门：KeysPage 曾把口径写成「输入新 kid 全名」并把占位符示例写成 `kid-2026Q4`，
    // 而组件实际期望 `KID2026Q4` 的后 8 位 `ID2026Q4` —— 用户照抄示例后确认按钮永久禁用，
    // 密钥轮换无法执行。这里把口径钉死在「归一化后 8 位」。
    const { submitBtn, noteArea, textInputs } = setup({
      confirmValue: 'kid-2026Q4',
      confirmLabel: '风险二次确认（请输入新 kid 全名）',
      confirmPlaceholder: '例如 kid-2026Q4',
    });
    await setNativeValue(noteArea()!, '计划性周期轮换，旧 kid 观察 30 天后退役');
    const inputs = textInputs();

    // 全名（= 占位符示例）必须仍然被拒绝
    await setNativeValue(inputs[inputs.length - 1], 'kid-2026Q4');
    expect(submitBtn()!.disabled).toBe(true);

    // 归一化后的后 8 位才放行
    await setNativeValue(inputs[inputs.length - 1], 'ID2026Q4');
    expect(submitBtn()!.disabled).toBe(false);
  });
});

describe('DangerConfirmModal —— confirm-value 缺失时的 fail-closed', () => {
  it('调用方漏传 confirmValue 时，即使填了正确的后 8 位也必须禁用提交', async () => {
    // 回归闸门：expectedTail 为空串时，旧逻辑会跳过二次校验直接放行
    // （四要素退化成三要素，confirm 形同摆设）。fail-closed 要求「校验缺失即拒绝」。
    const { submitBtn, noteArea, textInputs, bodyText } = setup({ confirmValue: '' });
    await setNativeValue(noteArea()!, '客户主板损坏已寄回厂商检修');
    const inputs = textInputs();
    await setNativeValue(inputs[inputs.length - 1], '90ABCD3K');
    expect(submitBtn()!.disabled).toBe(true);
    expect(bodyText()).toContain('对象标识缺失（confirmValue 未传入）');
  });
});

describe('DangerConfirmModal —— 草稿隔离（血泪教训）', () => {
  it('关闭后再打开，草稿必须被清空（不复活）', async () => {
    const wrapper = mount(DangerConfirmModal, {
      props: {
        open: true,
        title: '废弃激活码',
        impacts: ['不可撤销'],
        reasons: ['客户更换硬件', '设备报废'],
        confirmValue: CODE,
        confirmText: '废弃激活码',
      },
      attachTo: document.body,
    });

    const noteArea = (): HTMLTextAreaElement => document.querySelector<HTMLTextAreaElement>('.uik-dcm__body textarea')!;
    const tailInput = (): HTMLInputElement => {
      const inputs = Array.from(document.querySelectorAll<HTMLInputElement>('.uik-dcm__body input[type="text"]'));
      return inputs[inputs.length - 1];
    };

    // 第一次填写
    await setNativeValue(noteArea(), '第一次填写的说明内容足够长');
    await setNativeValue(tailInput(), '90ABCD3K');
    expect(noteArea().value).toBe('第一次填写的说明内容足够长');

    // 关闭
    await wrapper.setProps({ open: false });
    await nextTick();
    // 再次打开
    await wrapper.setProps({ open: true });
    await nextTick();

    expect(noteArea().value).toBe('');
    expect(tailInput().value).toBe('');
  });

  it('影响清单必须渲染出来（可解释性）', () => {
    const { bodyText } = setup({ impacts: ['原设备将立即停止北向转发，本地采集继续'] });
    expect(bodyText()).toContain('原设备将立即停止北向转发，本地采集继续');
  });
});

describe('DangerConfirmModal —— confirmMode="full" 本地 fail-fast 精确匹配', () => {
  // ① 正确全名 → 无本地错误，按钮可用
  it('① 输入与 confirmValue 完全一致的全名 → 无错误且可提交', async () => {
    const { submitBtn, noteArea, textInputs, bodyText } = setup({ confirmMode: 'full', confirmValue: 'zhangsan' });
    await setNativeValue(noteArea()!, '岗位职责调整需要重置该账号');
    const inputs = textInputs();
    expect(submitBtn()!.disabled).toBe(true); // 未输入时仍禁用
    await setNativeValue(inputs[inputs.length - 1], 'zhangsan');
    expect(submitBtn()!.disabled).toBe(false);
    expect(bodyText()).not.toContain('与对象全名不一致');
    expect(bodyText()).not.toContain('与对象标识后 8 位不一致');
  });

  it('① 中文全名一致 → 可提交', async () => {
    const { submitBtn, noteArea, textInputs } = setup({ confirmMode: 'full', confirmValue: '值班长' });
    await setNativeValue(noteArea()!, '岗位职责调整需要重建该角色');
    const inputs = textInputs();
    await setNativeValue(inputs[inputs.length - 1], '值班长');
    expect(submitBtn()!.disabled).toBe(false);
  });

  it('① full 模式默认标签 / 提示为「全名」口径，并明确宣称「大小写不敏感」', () => {
    const { bodyText } = setup({ confirmMode: 'full', confirmValue: 'zhangsan' });
    expect(bodyText()).toContain('风险二次确认（输入对象全名）');
    expect(bodyText()).toContain('请完整填写对象全名');
    // 与实现对齐：`rules_api:694/:859`、`alerts_api:270 check_trio`、`remote_ops:632/742`
    // 一律是 `trim()` + `eq_ignore_ascii_case`，故提示文案必须明确宣称「不区分大小写」。
    expect(bodyText()).toContain('大小写不敏感');
  });

  // ② 错误名称 → 字段级错误 + 禁用
  it('② 完全不同的名称 → 本地错误且禁用', async () => {
    const { submitBtn, noteArea, textInputs, bodyText } = setup({ confirmMode: 'full', confirmValue: 'zhangsan' });
    await setNativeValue(noteArea()!, '岗位职责调整需要重置该账号');
    const inputs = textInputs();
    await setNativeValue(inputs[inputs.length - 1], 'lisi');
    expect(submitBtn()!.disabled).toBe(true);
    expect(bodyText()).toContain('与对象全名不一致');
  });

  it('② 大小写不同但字母相同 → 仍匹配（大小写不敏感）', async () => {
    const { submitBtn, noteArea, textInputs } = setup({ confirmMode: 'full', confirmValue: 'ZhangSan' });
    await setNativeValue(noteArea()!, '岗位职责调整需要重置该账号');
    const inputs = textInputs();
    await setNativeValue(inputs[inputs.length - 1], 'zhangsan');
    expect(submitBtn()!.disabled).toBe(false);
  });

  it('② 仅首尾空格差异 → 仍匹配（本地先 trim）', async () => {
    const { submitBtn, noteArea, textInputs } = setup({ confirmMode: 'full', confirmValue: 'zhangsan' });
    await setNativeValue(noteArea()!, '岗位职责调整需要重置该账号');
    const inputs = textInputs();
    await setNativeValue(inputs[inputs.length - 1], '  zhangsan  ');
    expect(submitBtn()!.disabled).toBe(false);
  });

  it('② 子串 / 超集不匹配 → 禁用（不做子串、不做分隔符剥离）', async () => {
    const { submitBtn, noteArea, textInputs } = setup({ confirmMode: 'full', confirmValue: 'zhang-san' });
    await setNativeValue(noteArea()!, '岗位职责调整需要重置该账号');
    const inputs = textInputs();
    // 输入去掉分隔符的 "zhangsan" 不应匹配含分隔符的全名（不做分隔符剥离）
    await setNativeValue(inputs[inputs.length - 1], 'zhangsan');
    expect(submitBtn()!.disabled).toBe(true);
  });

  it('② 空输入（含纯空白）→ 禁用，但无「不一致」错误（仅因非空校验）', async () => {
    const { submitBtn, noteArea, textInputs, bodyText } = setup({ confirmMode: 'full', confirmValue: 'zhangsan' });
    await setNativeValue(noteArea()!, '岗位职责调整需要重置该账号');
    const inputs = textInputs();
    await setNativeValue(inputs[inputs.length - 1], '   ');
    expect(submitBtn()!.disabled).toBe(true);
    expect(bodyText()).not.toContain('与对象全名不一致');
  });

  // ③ confirmValue 为空 → 退化为仅校验非空
  it('③ confirmValue 漏传（空）→ 有非空输入即放行（本地不做匹配，交服务端兜底）', async () => {
    const { submitBtn, noteArea, textInputs, bodyText } = setup({ confirmMode: 'full', confirmValue: '' });
    await setNativeValue(noteArea()!, '岗位职责调整需要重置该账号');
    const inputs = textInputs();
    expect(submitBtn()!.disabled).toBe(true); // 空输入禁用
    await setNativeValue(inputs[inputs.length - 1], '任意非空文本');
    expect(submitBtn()!.disabled).toBe(false);
    expect(bodyText()).not.toContain('与对象全名不一致');
  });

  // ④ tail8 回归：既有行为零改动
  it('④ tail8 默认标签仍是「后 8 位」口径（零回归）', () => {
    const { bodyText } = setup();
    expect(bodyText()).toContain('风险二次确认（输入对象后 8 位）');
  });

  it('④ tail8 模式仍按后 8 位本地校验（不区分大小写 / 容忍分隔符）', async () => {
    const { submitBtn, noteArea, textInputs } = setup(); // 默认 tail8
    await setNativeValue(noteArea()!, '客户主板损坏已寄回厂商检修');
    const inputs = textInputs();
    await setNativeValue(inputs[inputs.length - 1], '90abcd3k'); // 小写
    expect(submitBtn()!.disabled).toBe(false);
  });

  it('④ full 模式 payload：confirm 为原文（不 trim），tail 亦为原文；note 仍 trim', async () => {
    const { wrapper, submitBtn, noteArea, textInputs } = setup({ confirmMode: 'full', confirmValue: ' 值班长 ' });
    await setNativeValue(noteArea()!, '岗位职责调整需要重建该角色');
    const inputs = textInputs();
    await setNativeValue(inputs[inputs.length - 1], ' 值班长 ');
    submitBtn()!.click();
    await nextTick();
    const payload = wrapper.emitted('submit')![0][0] as Record<string, string>;
    expect(payload.confirm).toBe(' 值班长 ');
    expect(payload.tail).toBe(' 值班长 ');
    expect(payload.note).toBe('岗位职责调整需要重建该角色');
    expect(payload.reason).toBe('客户更换硬件');
  });
});
