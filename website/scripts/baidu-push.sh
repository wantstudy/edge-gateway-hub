#!/usr/bin/env bash
# 百度搜索资源平台「普通收录 → 主动推送」调用脚本
#
# 作用：把站点 URL 主动推送给百度，让蜘蛛立即获知内容（新站最有效的收录加速通道）。
# 与 index.html 内嵌的 push.js「自动推送」互补 —— 自动推送仅在**有真实用户访问**时触发，
# 新站零访问等于零推送；本脚本由发布流程主动调用，不依赖访问量。
#
# 准入密钥（token）在百度搜索资源平台「普通收录 → 主动推送」页获取，
# **只经环境变量注入，绝不写入本仓库**。
#
# 用法：
#   BAIDU_PUSH_TOKEN=<token> ./baidu-push.sh                       # 推送 sitemap.xml 内全部 URL
#   BAIDU_PUSH_TOKEN=<token> ./baidu-push.sh --dry-run             # 只打印，不发送请求
#   BAIDU_PUSH_TOKEN=<token> ./baidu-push.sh --limit 10            # 只推前 N 条（日配额保护）
#   BAIDU_PUSH_TOKEN=<token> ./baidu-push.sh --urls urls.txt       # 从文件读（每行一个 URL）
#   BAIDU_PUSH_TOKEN=<token> ./baidu-push.sh https://edge.webscad.cn/faq/   # 直接传 URL
#
# 可覆盖的环境变量：BAIDU_PUSH_SITE（默认 https://edge.webscad.cn）、
#                   BAIDU_PUSH_SITEMAP（默认 website/sitemap.xml）、
#                   BAIDU_PUSH_ENDPOINT（默认官方接口 https://data.zz.baidu.com/urls）。
#
# 退出码：0 = 成功；2 = 前置不满足（token 缺失 / URL 列表为空 / 依赖缺失）；3 = 接口返回失败。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WEBSITE_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

SITE="${BAIDU_PUSH_SITE:-https://edge.webscad.cn}"
SITEMAP="${BAIDU_PUSH_SITEMAP:-${WEBSITE_DIR}/sitemap.xml}"
ENDPOINT="${BAIDU_PUSH_ENDPOINT:-https://data.zz.baidu.com/urls}"

DRY_RUN=0
LIMIT=0
URLS_FILE=""
POSITIONAL_URLS=()

usage() {
  cat <<'EOF'
用法：BAIDU_PUSH_TOKEN=<token> ./baidu-push.sh [选项] [URL ...]

选项：
  --dry-run        只打印待推送 URL，不发送请求
  --limit N        只推送前 N 条（保护百度每日推送配额）
  --urls FILE      从文件读取 URL（每行一个；忽略空行与 # 注释行）
  -h, --help       显示本帮助

位置参数：
  URL ...          直接指定待推送 URL（优先于 --urls 与 sitemap）

未指定 URL 来源时，默认从同目录上层的 sitemap.xml 提取 <loc> 列表。
EOF
}

die() { printf '[baidu-push] 错误：%s\n' "$1" >&2; exit "${2:-2}"; }

while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) DRY_RUN=1; shift ;;
    --limit)   [ $# -ge 2 ] || die "--limit 需要参数"; LIMIT="$2"; shift 2 ;;
    --urls)    [ $# -ge 2 ] || die "--urls 需要参数"; URLS_FILE="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    -*)        die "未知参数：$1" ;;
    *)         POSITIONAL_URLS+=("$1"); shift ;;
  esac
done

case "${LIMIT}" in
  ''|*[!0-9]*) die "--limit 必须为非负整数，当前：${LIMIT}" ;;
esac

command -v curl >/dev/null 2>&1 || die "缺少 curl"

# 收集待推送 URL：位置参数 > --urls 文件 > sitemap.xml
collect_urls() {
  if [ "${#POSITIONAL_URLS[@]}" -gt 0 ]; then
    printf '%s\n' "${POSITIONAL_URLS[@]}"
    return 0
  fi
  if [ -n "${URLS_FILE}" ]; then
    [ -f "${URLS_FILE}" ] || die "URL 文件不存在：${URLS_FILE}"
    sed -e 's/\r$//' -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//' "${URLS_FILE}" \
      | grep -v '^$' | grep -v '^#' || true
    return 0
  fi
  [ -f "${SITEMAP}" ] || die "sitemap 不存在：${SITEMAP}（可用 --urls 指定 URL 文件）"
  grep -oE '<loc>[^<]+</loc>' "${SITEMAP}" \
    | sed -e 's|^<loc>||' -e 's|</loc>$||' -e 's/\r$//' | grep -v '^$' || true
}

URL_LIST="$(collect_urls)"
[ -n "${URL_LIST}" ] || die "待推送 URL 列表为空（检查 ${SITEMAP}）"

if [ "${LIMIT}" -gt 0 ]; then
  URL_LIST="$(printf '%s\n' "${URL_LIST}" | head -n "${LIMIT}")"
fi

COUNT="$(printf '%s\n' "${URL_LIST}" | grep -c . || true)"
printf '[baidu-push] 站点=%s 待推送 URL=%s 条\n' "${SITE}" "${COUNT}"
printf '%s\n' "${URL_LIST}" | sed 's/^/  - /'

if [ "${DRY_RUN}" -eq 1 ]; then
  printf '[baidu-push] --dry-run：未发送任何请求\n'
  exit 0
fi

TOKEN="${BAIDU_PUSH_TOKEN:-}"
[ -n "${TOKEN}" ] || die "缺少环境变量 BAIDU_PUSH_TOKEN（在百度搜索资源平台「普通收录 → 主动推送」获取准入密钥）"

# 官方接口：POST text/plain，正文每行一个 URL。
# token 仅出现在命令行参数中，不写入任何文件，异常输出也不回显 token。
if ! RESP="$(printf '%s\n' "${URL_LIST}" \
  | curl -sS --max-time 30 -H 'Content-Type:text/plain' --data-binary @- \
      "${ENDPOINT}?site=${SITE}&token=${TOKEN}" 2>&1)"; then
  # 本机若经抓包/企业代理出网，代理证书不被信任，schannel 会报 SNI/certificate 失败
  case "${RESP}" in
    *schannel*|*certificate*|*SSL*)
      printf '[baidu-push] 提示：TLS 证书校验失败。本机若经抓包代理出网，代理证书不被系统信任；\n' >&2
      printf '[baidu-push]       请在服务器 / CI 直连执行，或设置 CURL_CA_BUNDLE 指向可信根证书。\n' >&2
      ;;
  esac
  die "curl 调用失败：${RESP:-无输出}" 3
fi

field() {
  printf '%s' "${1}" | grep -oE "\"${2}\":\"?[^\",}]*\"?" | head -1 \
    | sed -e "s/\"${2}\"://" -e 's/^"//' -e 's/"$//'
}

SUCCESS="$(field "${RESP}" success)"
REMAIN="$(field "${RESP}" remain)"
ERROR="$(field "${RESP}" error)"
MESSAGE="$(field "${RESP}" message)"

if [ -n "${ERROR}" ]; then
  printf '[baidu-push] 接口返回失败：error=%s message=%s\n' "${ERROR}" "${MESSAGE}" >&2
  printf '[baidu-push] 原始响应：%s\n' "${RESP}" >&2
  exit 3
fi
if [ -z "${SUCCESS}" ]; then
  printf '[baidu-push] 无法解析接口响应（未返回 success 字段）\n' >&2
  printf '[baidu-push] 原始响应：%s\n' "${RESP}" >&2
  exit 3
fi

printf '[baidu-push] 成功推送 %s 条；当日剩余配额 %s\n' "${SUCCESS}" "${REMAIN:-未知}"

# 部分 URL 与站点不同源 / 不合法时如实列出，不静默吞掉
for kind in not_same_site not_valid; do
  items="$(printf '%s' "${RESP}" | grep -oE "\"${kind}\":\[[^]]*\]" || true)"
  [ -n "${items}" ] && printf '[baidu-push] %s：%s\n' "${kind}" "${items#*:}" >&2
done
exit 0
