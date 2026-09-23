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
