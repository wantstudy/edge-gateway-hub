#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# lib.sh —— license-e2e 验收脚本的公共断言框架（fail-closed）。
#
# 纪律：
#   * 任何断言失败 = 整脚本失败（除显式 `skip` 外没有「软通过」）。
#   * 「命令没报错」永不作为通过条件；必须落在真实返回值上
#     （HTTP 状态码 / JSON 字段 / 常量取值 / 源码证据）。
#   * skip 必须显式计数并打印原因，禁止静默吞掉。
#   * 退出码：0 = 全部 PASS；1 = 有 FAIL；2 = 环境不满足（全部 skip 视为 0，
#     但会单独打印 SKIPPED 清单，提醒人工补跑）。
# ---------------------------------------------------------------------------
set -Eeuo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../../.." && pwd)"

# --- 统计 ---
E_TOTAL=0; E_PASS=0; E_FAIL=0; E_SKIP=0
FAILED_NAMES=()
SKIPPED_NAMES=()

# --- 可选配色（无 TTY / NO_COLOR 时降级为纯文本，保证 CI 日志干净）---
if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
    C_RED=$'\033[31m'; C_GREEN=$'\033[32m'; C_YELLOW=$'\033[33m'
    C_BLUE=$'\033[34m'; C_DIM=$'\033[2m'; C_BOLD=$'\033[1m'; C_OFF=$'\033[0m'
else
    C_RED=''; C_GREEN=''; C_YELLOW=''; C_BLUE=''; C_DIM=''; C_BOLD=''; C_OFF=''
fi

log()  { printf '%s\n' "$*"; }
dim()  { printf '%s%s%s\n' "$C_DIM" "$*" "$C_OFF"; }
ok()   { printf '  %sPASS%s %s\n' "$C_GREEN" "$C_OFF" "$*"; }
bad()  { printf '  %sFAIL%s %s\n' "$C_RED" "$C_OFF" "$*"; }
skip() { printf '  %sSKIP%s %s :: %s\n' "$C_YELLOW" "$C_OFF" "$*" "${2:-no reason given}"; }

section() {
    printf '\n%s== %s ==%s\n' "$C_BOLD" "$*" "$C_OFF"
}

# --- 断言原语 -------------------------------------------------------------

## assert_eq <名字> <期望> <实际>
assert_eq() {
    local name="$1" want="$2" got="$3"
    E_TOTAL=$((E_TOTAL + 1))
    if [ "$want" = "$got" ]; then
        E_PASS=$((E_PASS + 1))
        ok "${name} (= ${got})"
    else
        E_FAIL=$((E_FAIL + 1)); FAILED_NAMES+=("$name")
        bad "${name} :: want=[${want}] got=[${got}]"
    fi
}

## assert_ne <名字> <不得等于> <实际>
assert_ne() {
    local name="$1" notwant="$2" got="$3"
    E_TOTAL=$((E_TOTAL + 1))
    if [ "$notwant" != "$got" ]; then
        E_PASS=$((E_PASS + 1)); ok "${name} (≠ ${notwant}, got ${got})"
    else
        E_FAIL=$((E_FAIL + 1)); FAILED_NAMES+=("$name")
        bad "${name} :: must NOT equal [${notwant}]"
    fi
}

## assert_true <名字> <条件求值结果:0/1> [说明]
assert_true() {
    local name="$1" cond="$2" why="${3:-}"
    E_TOTAL=$((E_TOTAL + 1))
    if [ "$cond" = "0" ]; then
        E_PASS=$((E_PASS + 1)); ok "$name${why:+ :: $why}"
    else
        E_FAIL=$((E_FAIL + 1)); FAILED_NAMES+=("$name")
        bad "${name}${why:+ :: $why}"
    fi
}

## assert_contains <名字> <haystack> <needle>
assert_contains() {
    local name="$1" hay="$2" needle="$3"
    E_TOTAL=$((E_TOTAL + 1))
    case "$hay" in
        *"$needle"*) E_PASS=$((E_PASS + 1)); ok "${name} (contains '${needle}')" ;;
        *)
            E_FAIL=$((E_FAIL + 1)); FAILED_NAMES+=("$name")
            bad "${name} :: missing '${needle}' in: ${hay}"
            ;;
    esac
}

## assert_not_contains <名字> <haystack> <needle>
assert_not_contains() {
    local name="$1" hay="$2" needle="$3"
    E_TOTAL=$((E_TOTAL + 1))
    case "$hay" in
        *"$needle"*)
            E_FAIL=$((E_FAIL + 1)); FAILED_NAMES+=("$name")
            bad "${name} :: must NOT contain '${needle}' (found)" ;;
        *) E_PASS=$((E_PASS + 1)); ok "${name} (absent '${needle}')" ;;
    esac
}

## skip_assert <名字> <原因>
skip_assert() {
    E_TOTAL=$((E_TOTAL + 1)); E_SKIP=$((E_SKIP + 1))
    SKIPPED_NAMES+=("$1")
    skip "$1" "$2"
}

## require_tools <工具...>：缺任一即 fail（不 skip——这是环境问题不是契约问题）
require_tools() {
    local missing=()
    for t in "$@"; do
        command -v "$t" >/dev/null 2>&1 || missing+=("$t")
    done
    if [ "${#missing[@]}" -gt 0 ]; then
        printf '%sFAIL%s 缺少必需工具: %s\n' "$C_RED" "$C_OFF" "${missing[*]}"
        exit 2
    fi
}

# --- 源码证据断言（静态契约用）-------------------------------------------

## src_contains <名字> <相对路径> <正则> [includeGlob]
## 在仓库根下按正则匹配文件内容；命中即 PASS，否则 FAIL 并把行号打出来。
## 注意：grep -E 是**单行**匹配，跨行结构请拆成多条单行断言（不要写 \n）。
src_contains() {
    local name="$1" rel="$2" pat="$3" glob="${4:-*.rs}"
    E_TOTAL=$((E_TOTAL + 1))
    local hits
    hits="$(grep -rn --include="$glob" -E "$pat" "${REPO_ROOT}/${rel}" 2>/dev/null || true)"
    if [ -n "$hits" ]; then
        E_PASS=$((E_PASS + 1))
        ok "${name} :: $(printf '%s' "$hits" | head -1)"
    else
        E_FAIL=$((E_FAIL + 1)); FAILED_NAMES+=("$name")
        bad "${name} :: 在 ${rel} 中找不到正则: ${pat}"
    fi
}

## src_next_line <名字> <文件(相对路径)> <锚点行(字面文本，非正则)> <期望的「下一个非空行」>
## 用来断言「跨行的函数体首句」这类 grep -E 单行走不了的结构。
## 例：锚点 `pub fn allows_local_capture(&self) -> bool {` → 期望下一非空行 `true`。
## 注意：锚点用**字面文本**匹配（awk 动态正则会把 `\(` 当转义、行为不可移植）。
src_next_line() {
    local name="$1" rel="$2" anchor="$3" want="$4"
    E_TOTAL=$((E_TOTAL + 1))
    local got
    got="$(awk -v anchor="$anchor" '
        index($0, anchor) { found = 1; next }
        found {
            line = $0
            gsub(/^[ \t]+|[ \t]+$/, "", line)
            if (line != "") { print line; exit }
        }
    ' "${REPO_ROOT}/${rel}" 2>/dev/null | head -1 || true)"
    if [ "$got" = "$want" ]; then
        E_PASS=$((E_PASS + 1)); ok "${name} (body starts with '${got}')"
    else
        E_FAIL=$((E_FAIL + 1)); FAILED_NAMES+=("$name")
        bad "${name} :: 锚点后下一个非空行 want=[${want}] got=[${got}] (${rel})"
    fi
}

## src_fn_returns <名字> <文件(相对路径)> <fn 名> <期望返回表达式>
## 断言某个 `fn <name>` 的 return 表达式（跨行结构用，比 src_const 更通用）。
src_fn_returns() {
    local name="$1" rel="$2" fname="$3" want="$4"
    E_TOTAL=$((E_TOTAL + 1))
    local got
    got="$(awk -v fname="fn ${fname}" '
        index($0, fname) {
            in_fn = 1
            # 抓同一行里的 `-> Ret {` 之后的 return；这里只处理单行 return
            if (match($0, /-> [A-Za-z0-9_:<>, ]+ \{/)) { body_start = 1 }
            next
        }
        in_fn && body_start {
            line = $0
            gsub(/^[ \t]+|[ \t]+$/, "", line)
            if (line == "") { next }
            # 表达式体函数（无 `return` 关键字）取函数体首行；块体函数取 `return ...`
            if (line ~ /^return /) { sub(/^return /, "", line); print line; exit }
            if (line ~ /^\}/) { exit }
            print line; exit
        }
    ' "${REPO_ROOT}/${rel}" 2>/dev/null | head -1 | sed 's/^return //' || true)"
    if [ "$got" = "$want" ]; then
        E_PASS=$((E_PASS + 1)); ok "${name} (${fname} => ${got})"
    else
        E_FAIL=$((E_FAIL + 1)); FAILED_NAMES+=("$name")
        bad "${name} :: ${fname}() return want=[${want}] got=[${got}] (${rel})"
    fi
}

## src_fn_absent <名字> <文件(相对路径)> <fn 名> <正则>
## 只在**该函数的函数体范围内**断言模式不存在（比整文件 src_absent 更精确，
## 用来证明「某判定函数没有消费某个配置」）。
src_fn_absent() {
    local name="$1" rel="$2" fname="$3" pat="$4"
    E_TOTAL=$((E_TOTAL + 1))
    local body hit
    body="$(awk -v anchor="fn ${fname}" '
        index($0, anchor) { in_fn = 1
            if (match($0, /\{/)) { depth = 1; if (RSTART == length($0)) { next } }
            next }
        in_fn {
            for (i = 1; i <= length($0); i++) {}
            n = gsub(/\{/, "", $0); m = gsub(/\}/, "", $0); depth += n - m
            line = $0; gsub(/^[ \t]+|[ \t]+$/, "", line)
            if (line != "" && line !~ /^(pub|#|\[|\/\/|\/\/\/)/) { print line }
            if (depth <= 0) { exit }
        }
    ' "${REPO_ROOT}/${rel}" 2>/dev/null || true)"
    hit="$(printf '%s\n' "$body" | grep -E "$pat" || true)"
    if [ -z "$hit" ]; then
        E_PASS=$((E_PASS + 1)); ok "${name} (${fname} 函数体内无 '${pat}')"
    else
        E_FAIL=$((E_FAIL + 1)); FAILED_NAMES+=("$name")
        bad "${name} :: ${fname}() 体内出现 '${pat}':"
        printf '%s\n' "$hit" | sed 's/^/        /'
    fi
}

## src_absent <名字> <搜索根(相对路径)> <正则> [includeGlob]
## 断言某模式在目录下**不存在**；存在即 FAIL（用于「客户端无解绑入口」类红线扫描）。
## 若命中行位于 `#[cfg(test)]` / `mod tests` 内（即「反例测试在证明会拒绝」），
## 不算违禁——调用方可用 src_absent_non_test 排除测试区后再断言。
src_absent() {
    local name="$1" rel="$2" pat="$3" glob="${4:-*.rs}"
    E_TOTAL=$((E_TOTAL + 1))
    local hits
    hits="$(grep -rn --include="$glob" -i -E "$pat" "${REPO_ROOT}/${rel}" 2>/dev/null || true)"
    if [ -z "$hits" ]; then
        E_PASS=$((E_PASS + 1)); ok "${name} (无命中)"
    else
        E_FAIL=$((E_FAIL + 1)); FAILED_NAMES+=("$name")
        bad "${name} :: 出现违禁模式 '${pat}':"
        printf '%s\n' "$hits" | sed 's/^/        /'
    fi
}

## src_const <名字> <文件(相对路径)> <常量名> <期望值>
## 精确提取 `pub const NAME: T = VALUE;` 的 VALUE 并比对。
src_const() {
    local name="$1" rel="$2" cname="$3" want="$4"
    E_TOTAL=$((E_TOTAL + 1))
    local got
    got="$(grep -rhoE "(pub )?const[[:space:]]+${cname}[[:space:]]*:[[:space:]]*[A-Za-z0-9_::<>, ]*=[^;]*;" \
              "${REPO_ROOT}/${rel}" 2>/dev/null | grep -oE "=[^;]*;" | head -1 | tr -d '= ;' || true)"
    if [ "$got" = "$want" ]; then
        E_PASS=$((E_PASS + 1)); ok "${name} (${cname}=${got})"
    else
        E_FAIL=$((E_FAIL + 1)); FAILED_NAMES+=("$name")
        bad "${name} :: ${cname} want=[${want}] got=[${got}]"
    fi
}

# --- 汇总 -----------------------------------------------------------------

## report <脚本名>
report() {
    local script="$1" code=0
    printf '\n%s──────── %s 汇总 ────────%s\n' "$C_BOLD" "$script" "$C_OFF"
    printf '  PASS=%d  FAIL=%d  SKIP=%d  (共 %d 条断言)\n' "$E_PASS" "$E_FAIL" "$E_SKIP" "$E_TOTAL"
    if [ "${#FAILED_NAMES[@]}" -gt 0 ]; then
        printf '  %s失败项:%s\n' "$C_RED" "$C_OFF"
        printf '    - %s\n' "${FAILED_NAMES[@]}"
        code=1
    fi
    if [ "${#SKIPPED_NAMES[@]}" -gt 0 ]; then
        printf '  %s跳过项（必须人工补跑）:%s\n' "$C_YELLOW" "$C_OFF"
        printf '    - %s\n' "${SKIPPED_NAMES[@]}"
    fi
    if [ "$code" -eq 0 ]; then
        printf '  %s结论: PASS%s\n' "$C_GREEN" "$C_OFF"
    else
        printf '  %s结论: FAIL%s\n' "$C_RED" "$C_OFF"
    fi
    return $code
}

## 让 set -e 下的断言失败不会被中途 exit 吞掉；本脚本聚合多处，故最后统一 report。
trap 'rc=$?; if [ $rc -ne 0 ] && [ $rc -ne 1 ]; then
          printf "\n%s中断%s: 第 %d 行退出码 %d\n" "$C_RED" "$C_OFF" "$LINENO" "$rc"
        fi' ERR
