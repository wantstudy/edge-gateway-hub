/**
 * @file danger-modal.spec.ts
 * @description DangerConfirmModal 的**行为正确性**测试（本项目最高风险组件）。
 *
 * 覆盖验收要点：
 *  - 原因未选 / 补充说明 < 10 字 / 后 8 位不符 / 第二审批人缺失 → 提交按钮**禁用**
 *  - 四要素齐备 → 可提交，且提交 payload 正确
 *  - 关闭后再打开 → 草稿被清空（不「复活」）
 */
import { describe, it, expect } from 'vitest';
import { mount } from '@vue/test-utils';
import DangerConfirmModal from '../src/components/DangerConfirmModal.vue';

/** 测试用完整激活码；后 8 位 = `90ABCD3K`。 */
const CODE = 'IOT-2025-1D90-ABCD-3K';

/** 挂载helper：返回 wrapper 与提交按钮。 */
function mountModal(props: Record<string, unknown> = {}) {
  const wrapper = mount(DangerConfirmModal, {
    props: {
      open: true,
      title: '废弃激活码',
      impacts: ['该设备将立即停止北向转发', '本操作不可撤销'],
      reasons: ['客户更换硬件', '设备报废'],
      confirmValue: CODE,
      confirmText: '废弃激活码',
      ...props,
    },
    attachTo: document.body,
  });
  const submitBtn = wrapper.findAll('button').find((b) => b.text() === '废弃激活码');
  return { wrapper, submitBtn };
}

describe('DangerConfirmModal —— 危险操作四要素', () => {
  it('初始状态提交按钮禁用（原因默认选中但说明与后 8 位为空）', () => {
    const { submitBtn } = mountModal();
    expect(submitBtn).toBeTruthy();
    expect(submitBtn!.attributes('disabled')).toBeDefined();
  });

  it('三要素齐备后提交按钮解除禁用', async () => {
    const { wrapper, submitBtn } = mountModal();
    // 原因：默认选中第一项（组件在 open 时初始化）
    // 补充说明：≥10 字
    await wrapper.find('textarea').setValue('客户主板损坏已寄回厂商检修');
    // 后 8 位校验：去分隔符比较
    const inputs = wrapper.findAll('input[type="text"]');
    await inputs[inputs.length - 1].setValue('90ABCD3K');
    expect(submitBtn!.attributes('disabled')).toBeUndefined();
  });

  it('补充说明不足 10 字时提交被禁用', async () => {
    const { wrapper, submitBtn } = mountModal();
    await wrapper.find('textarea').setValue('太短了');
    const inputs = wrapper.findAll('input[type="text"]');
    await inputs[inputs.length - 1].setValue('90ABCD3K');
    expect(submitBtn!.attributes('disabled')).toBeDefined();
  });

  it('后 8 位不符时提交被禁用，并给出字段级错误', async () => {
    const { wrapper, submitBtn } = mountModal();
    await wrapper.find('textarea').setValue('客户主板损坏已寄回厂商检修');
    const inputs = wrapper.findAll('input[type="text"]');
    await inputs[inputs.length - 1].setValue('WRONG888');
    expect(submitBtn!.attributes('disabled')).toBeDefined();
    expect(wrapper.text()).toContain('与对象标识后 8 位不一致');
  });

  it('后 8 位校验容忍小写与分隔符差异', async () => {
    const { wrapper, submitBtn } = mountModal();
    await wrapper.find('textarea').setValue('客户主板损坏已寄回厂商检修');
    const inputs = wrapper.findAll('input[type="text"]');
    await inputs[inputs.length - 1].setValue('90ab-cd3k');
    expect(submitBtn!.attributes('disabled')).toBeUndefined();
  });

  it('双人复核开启时第二审批人缺失则禁用，填写后解除', async () => {
    const { wrapper, submitBtn } = mountModal({ requireSecondApprover: true });
    await wrapper.find('textarea').setValue('客户主板损坏已寄回厂商检修');
    const inputs = wrapper.findAll('input[type="text"]');
    await inputs[inputs.length - 1].setValue('90ABCD3K');
    // 第二审批人仍未填
    expect(submitBtn!.attributes('disabled')).toBeDefined();
    await inputs[0].setValue('wang.gong');
    expect(submitBtn!.attributes('disabled')).toBeUndefined();
  });

  it('提交后向上冒泡完整 payload', async () => {
    const { wrapper, submitBtn } = mountModal();
    await wrapper.find('textarea').setValue('客户主板损坏已寄回厂商检修');
    const inputs = wrapper.findAll('input[type="text"]');
    await inputs[inputs.length - 1].setValue('90ABCD3K');
    await submitBtn!.trigger('click');
    const emitted = wrapper.emitted('submit');
    expect(emitted).toBeTruthy();
    const payload = emitted![0][0] as Record<string, string>;
    expect(payload.note).toBe('客户主板损坏已寄回厂商检修');
    expect(payload.tail).toBe('90ABCD3K');
    expect(payload.reason).toBe('客户更换硬件');
  });

  it('业务侧 disabled=true 时无论如何都不能提交', async () => {
    const { wrapper, submitBtn } = mountModal({ disabled: true });
    await wrapper.find('textarea').setValue('客户主板损坏已寄回厂商检修');
    const inputs = wrapper.findAll('input[type="text"]');
    await inputs[inputs.length - 1].setValue('90ABCD3K');
    expect(submitBtn!.attributes('disabled')).toBeDefined();
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

    // 第一次填写
    await wrapper.find('textarea').setValue('第一次填写的说明内容足够长');
    const inputs = wrapper.findAll('input[type="text"]');
    await inputs[inputs.length - 1].setValue('90ABCD3K');
    expect((wrapper.find('textarea').element as HTMLTextAreaElement).value).toBe('第一次填写的说明内容足够长');

    // 关闭
    await wrapper.setProps({ open: false });
    // 再次打开
    await wrapper.setProps({ open: true });

    expect((wrapper.find('textarea').element as HTMLTextAreaElement).value).toBe('');
    const inputs2 = wrapper.findAll('input[type="text"]');
    expect((inputs2[inputs2.length - 1].element as HTMLInputElement).value).toBe('');
  });

  it('影响清单必须渲染出来（可解释性）', () => {
    const { wrapper } = mountModal({ impacts: ['原设备将立即停止北向转发，本地采集继续'] });
    expect(wrapper.text()).toContain('原设备将立即停止北向转发，本地采集继续');
  });
});
