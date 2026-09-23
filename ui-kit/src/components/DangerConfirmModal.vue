<!--
  =============================================================================
  DangerConfirmModal —— 高危二次确认弹窗（设计契约 / design contract）
  =============================================================================
  权威来源：`docs/design/ui-design-system.md` §3（组件契约）与 §4.1（高危操作规范）；
            `docs/design/ui-admin-console.md` §3.2（废弃 / 重发 / 密钥轮换弹窗要求）。

  ── 契约：四要素缺一不可 ────────────────────────────────────────────────────
   1) 影响清单（`impacts`）：把**后果**写清 —— 影响什么实体 + 是否可恢复 + 恢复路径；
      只写「确定要废弃吗」属于违约。
   2) 原因必填：`reasons` 枚举 + 补充说明（`note`，≥ `minNoteLength` 字）。
      —— 原因会随操作写入审计日志，缺失即无法解释「谁在何时为何做了此事」。
   3) 对象名二次校验：输入目标对象的名字 / 后 8 位（`expectedTail`）。
      —— 防止「手滑选错行」，把确认对象从隐式选择变为显式复述。
   4) 双人复核：当全局 `dualApproval` 开启时（`requireSecondApprover=true`），
      第二审批人必填 —— 高危操作需两位管理员。

  ── 契约：草稿隔离（draft isolation，本组件的核心不变式）─────────────────────
   · 所有输入写入**本地 `form` 草稿**，绝不直接写入任何真实数据 / 上层 store。
   · `watch(() => props.open)`：打开时**初始化**草稿（默认选中第一种原因）；
     关闭时**彻底清空**（`resetDraft()`）。
   · 因此「本次打开填一半 → 取消 → 再次打开」时草稿必然复位，不会「复活」；
     取消 / 遮罩点击与「确认」在数据意义上完全等价为「什么都没发生」。
   · 按钮可用性由 `computed canSubmit` 实时推导（校验不过即 `disabled`），
     而不是「先允许点、提交后再报错」——把错误挡在点击之前。

  ── 红线 ─────────────────────────────────────────────────────────────────────
   本组件是前端**交互层**保障；真正的授权与幂等判定在服务端，前端不可信。
-->
<template>
  <!--
    DangerConfirmModal —— 本系统最危险的确认弹窗（设计系统 §3 / §4.1）。
    四要素缺一不可：
      1) 影响清单（把后果写清：影响什么 + 恢复路径）
      2) 原因必填（枚举 + 补充说明 ≥ minNoteLength 字）
      3) 对象名二次校验（输入对象后 8 位/全名）
      4) 双人复核开关时，第二审批人必填
    实现要点（血泪教训）：草稿一律在**打开时初始化**，关闭时**彻底清空**，
    绝不允许草稿写进真实数据，也绝不允许下次打开时草稿「复活」。
  -->
  <Teleport to="body">
    <div v-if="open" class="uik-dcm__mask" @click.self="onMaskClick">
      <div class="uik-dcm" role="dialog" aria-modal="true" :aria-label="title" @keydown.esc.stop>
        <div class="uik-dcm__head">
          <h3 class="uik-dcm__title">
            <span class="uik-dcm__warn-icon" aria-hidden="true">⚠</span>{{ title }}
          </h3>
        </div>

        <div class="uik-dcm__body">
          <!-- 1) 影响清单 -->
          <section class="uik-dcm__impact">
            <p class="uik-dcm__impact-title">影响提示</p>
            <ul>
              <li v-for="(line, i) in impacts" :key="i">{{ line }}</li>
            </ul>
          </section>

          <!-- 对象摘要（让操作者确认「我在操作谁」） -->
          <dl v-if="facts.length" class="uik-dcm__facts">
            <template v-for="fact in facts" :key="fact.label">
              <dt>{{ fact.label }}</dt>
              <dd>{{ fact.value }}</dd>
            </template>
          </dl>

          <!-- 2) 原因必填（枚举 + 补充说明） -->
          <div class="uik-dcm__grid">
            <UiField label="操作原因" required hint="必选，将记入审计">
              <UiSelect v-model="form.reason" :options="reasonOptions" />
            </UiField>

            <UiField
              class="uik-dcm__note"
              label="补充说明"
              required
              :hint="`必填，≥ ${minNoteLength} 字，将随操作一并记入审计日志`"
              :error="noteError"
              full
            >
              <UiTextarea
                v-model="form.note"
                :rows="3"
                :min-length="minNoteLength"
                show-counter
                :invalid="noteError.length > 0"
                placeholder="说明本次操作的具体背景，便于日后追溯"
              />
            </UiField>
          </div>

          <!-- 3) 对象名二次校验 -->
          <UiField
            :label="confirmLabel"
            required
            :hint="confirmHint"
            :error="tailError"
          >
            <UiInput
              v-model="form.tail"
              :invalid="tailError.length > 0"
              :placeholder="confirmPlaceholder"
            />
          </UiField>

          <!-- 4) 双人复核 -->
          <UiField
            v-if="requireSecondApprover"
            label="第二审批人"
            required
            hint="已开启双人复核：本操作需另一位管理员账号复核"
            :error="approverError"
          >
            <UiInput v-model="form.secondApprover" placeholder="输入复核管理员账号（如 wang.gong）" />
          </UiField>

          <p class="uik-dcm__irreversible">本操作不可撤销。</p>
        </div>

        <div class="uik-dcm__foot">
          <button type="button" class="uik-btn" @click="close">取消</button>
          <button
            type="button"
            class="uik-btn uik-btn--danger"
            :disabled="!canSubmit"
            @click="submit"
          >
            {{ confirmText }}
          </button>
        </div>
      </div>
    </div>
  </Teleport>
</template>

<script setup lang="ts">
/**
 * @file DangerConfirmModal.vue
 * @module ui-kit/components/DangerConfirmModal
 * @description 高危操作确认弹窗（废弃 / 重发 / 密钥轮换 / 策略变更）。
 *
 * 校验规则全部在 `computed` 中实时计算，**不满足则提交按钮禁用**（不是提交后才报错）。
 */
import { computed, reactive, watch } from 'vue';
import UiField from './UiField.vue';
import UiInput from './UiInput.vue';
import UiSelect, { type SelectOption } from './UiSelect.vue';
import UiTextarea from './UiTextarea.vue';
import { codeTail8 } from '../mask';

/** 对象摘要项。 */
export interface DangerFact {
  /** 字段名 */
  label: string;
  /** 字段值 */
  value: string;
}

interface Props {
  /** 是否打开 */
  open: boolean;
  /** 弹窗标题（应包含操作对象名，便于操作者确认） */
  title: string;
  /** 影响清单（每条一行，必须写清后果与恢复路径） */
  impacts: readonly string[];
  /** 操作对象摘要 */
  facts?: readonly DangerFact[];
  /** 可选的必选原因枚举（为空则不渲染原因下拉） */
  reasons?: readonly string[];
  /** 补充说明最小字数（默认 10，与 licensing-api.md §2.2 对齐） */
  minNoteLength?: number;
  /**
   * 二次校验目标值：组件内部取其后 8 位（去分隔符、大写）与用户输入比较。
   * 例如传入完整激活码 `IOT-2026-8C3F-1234-ABCD-A1`，用户需输入 `C3F123…` 的后 8 位。
   */
  confirmValue?: string;
  /** 二次校验字段标签 */
  confirmLabel?: string;
  /** 二次校验输入占位符 */
  confirmPlaceholder?: string;
  /** 确认按钮文案（必须是动词短语，如「废弃激活码」） */
  confirmText?: string;
  /** 是否需要第二审批人（双人复核开关） */
  requireSecondApprover?: boolean;
  /** 是否禁用提交（业务侧额外条件，如「无权操作」） */
  disabled?: boolean;
}

const props = withDefaults(defineProps<Props>(), {
  facts: () => [],
  reasons: () => [],
  minNoteLength: 10,
  confirmValue: '',
  confirmLabel: '风险二次确认（输入对象后 8 位）',
  confirmPlaceholder: '输入对象标识去分隔符后的后 8 位',
  confirmText: '确认执行',
  requireSecondApprover: false,
  disabled: false,
});

const emit = defineEmits<{
  /** 关闭弹窗（取消 / 遮罩点击）——调用方必须清草稿 */
  (e: 'close'): void;
  /** 通过全部校验后提交 */
  (e: 'submit', payload: { reason: string; note: string; tail: string; secondApprover: string }): void;
}>();

/** 弹窗内部草稿（**只在打开时初始化，关闭时清空**）。 */
const form = reactive({
  reason: '',
  note: '',
  tail: '',
  secondApprover: '',
});

/** 原因下拉选项（首项「请选择」value 为空，等价于未选）。 */
const reasonOptions = computed<readonly SelectOption[]>(() => [
  { value: '', label: '请选择原因…' },
  ...props.reasons.map((r) => ({ value: r, label: r })),
]);

/** 期望的后 8 位（去分隔符、大写）。 */
const expectedTail = computed(() => codeTail8(props.confirmValue));

/** 补充说明错误：有内容但字数不足才提示，避免刚打开就报红。 */
const noteError = computed(() => {
  const len = form.note.trim().length;
  if (len === 0) {
    return '';
  }
  return len < props.minNoteLength ? `还差 ${props.minNoteLength - len} 字` : '';
});

/** 二次校验错误：仅在用户已输入但填错时提示。 */
const tailError = computed(() => {
  const input = form.tail.trim();
  if (input.length === 0) {
    return '';
  }
  const normalize = input.replace(/[^A-Za-z0-9]/g, '').toUpperCase();
  return normalize === expectedTail.value ? '' : '与对象标识后 8 位不一致';
});

/** 第二审批人错误：必填但未填时提示（仅在双人复核开启且用户已交互时提示）。 */
const approverError = computed(() => {
  if (!props.requireSecondApprover) {
    return '';
  }
  return form.secondApprover.trim().length === 0 ? '双人复核已开启，必须填写第二审批人' : '';
});

/** 二次校验提示：写清比对的是哪一段。 */
const confirmHint = computed(() =>
  expectedTail.value
    ? `请完整填写对象标识后 8 位（去分隔符、不区分大小写），用于防止误操作`
    : '请填写对象标识后 8 位以确认',
);

/**
 * 提交可用性：四要素全部满足才允许提交。
 * 1) 原因已选（若提供枚举）
 * 2) 补充说明 ≥ minNoteLength
 * 3) 后 8 位完全一致
 * 4) 双人复核时第二审批人非空
 */
const canSubmit = computed(() => {
  if (props.disabled) {
    return false;
  }
  if (props.reasons.length > 0 && form.reason === '') {
    return false;
  }
  if (form.note.trim().length < props.minNoteLength) {
    return false;
  }
  if (expectedTail.value.length > 0 && expectedTail.value !== form.tail.trim().replace(/[^A-Za-z0-9]/g, '').toUpperCase()) {
    return false;
  }
  if (props.requireSecondApprover && form.secondApprover.trim().length === 0) {
    return false;
  }
  return true;
});

/** 打开时重置默认原因（选第一项枚举），保证草稿从干净状态开始。 */
watch(
  () => props.open,
  (isOpen) => {
    if (isOpen) {
      resetDraft();
      form.reason = props.reasons.length > 0 ? props.reasons[0] : '';
    } else {
      resetDraft();
    }
  },
  { immediate: true },
);

/** 清空草稿（关闭时必须调用，防止下次打开草稿「复活」）。 */
function resetDraft(): void {
  form.reason = '';
  form.note = '';
  form.tail = '';
  form.secondApprover = '';
}

/** 关闭：清草稿后通知父级。 */
function close(): void {
  resetDraft();
  emit('close');
}

/** 遮罩点击关闭（危险弹窗允许遮罩关闭，但提交前必须显式点击确认）。 */
function onMaskClick(): void {
  close();
}

/** 提交：再次兜底校验，然后清草稿 + 冒泡。 */
function submit(): void {
  if (!canSubmit.value) {
    return;
  }
  const payload = {
    reason: form.reason,
    note: form.note.trim(),
    tail: form.tail.trim(),
    secondApprover: form.secondApprover.trim(),
  };
  resetDraft();
  emit('submit', payload);
}
</script>

<style scoped>
.uik-dcm__mask {
  position: fixed;
  inset: 0;
  background: rgba(29, 33, 41, 0.45);
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 2000;
  padding: 24px;
}
.uik-dcm {
  background: #fff;
  border-radius: var(--radius);
  box-shadow: var(--shadow);
  width: 660px;
  max-width: 100%;
  max-height: 88vh;
  display: flex;
  flex-direction: column;
}
.uik-dcm__head {
  padding: 16px 20px;
  border-bottom: 1px solid var(--divider);
}
.uik-dcm__title {
  margin: 0;
  font-size: 16px;
  font-weight: 600;
  color: var(--text-1);
  display: flex;
  align-items: center;
  gap: 6px;
}
.uik-dcm__warn-icon {
  color: var(--danger);
}
.uik-dcm__body {
  padding: 20px;
  overflow: auto;
  display: flex;
  flex-direction: column;
  gap: 14px;
}
.uik-dcm__impact {
  background: var(--danger-bg);
  border: 1px solid var(--danger-border);
  border-radius: var(--radius-sm);
  padding: 12px 14px;
  color: #7a1418;
}
.uik-dcm__impact-title {
  margin: 0;
  font-size: var(--fs-table);
  font-weight: 600;
}
.uik-dcm__impact ul {
  margin: 6px 0 0;
  padding-left: 18px;
  font-size: var(--fs-table);
  line-height: 1.75;
}
.uik-dcm__facts {
  display: grid;
  grid-template-columns: 130px 1fr;
  gap: 8px 16px;
  margin: 0;
  font-size: var(--fs-table);
}
.uik-dcm__facts dt {
  color: var(--text-3);
  font-size: var(--fs-caption);
}
.uik-dcm__facts dd {
  margin: 0;
  color: var(--text-1);
  word-break: break-all;
}
.uik-dcm__grid {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: 14px 16px;
}
.uik-dcm__note {
  grid-column: 1 / -1;
}
.uik-dcm__irreversible {
  margin: 0;
  font-size: var(--fs-caption);
  color: var(--danger);
  font-weight: 600;
}
.uik-dcm__foot {
  padding: 14px 20px;
  border-top: 1px solid var(--divider);
  display: flex;
  justify-content: flex-end;
  gap: 8px;
}
.uik-btn {
  font-family: inherit;
  font-size: var(--fs-table);
  min-height: 32px;
  padding: 6px 14px;
  border-radius: var(--radius-sm);
  border: 1px solid var(--border);
  background: #fff;
  color: var(--text-1);
  cursor: pointer;
}
.uik-btn:hover {
  border-color: #c9cdd4;
  background: var(--bg-hover);
}
.uik-btn--danger {
  background: var(--danger);
  border-color: var(--danger);
  color: #fff;
}
.uik-btn--danger:hover {
  background: #d92d2d;
  border-color: #d92d2d;
}
.uik-btn--danger:disabled,
.uik-btn:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}
</style>
