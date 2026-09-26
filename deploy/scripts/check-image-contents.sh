#!/usr/bin/env bash
# =============================================================================
# iot-daq · 容器镜像交付物内容断言（task 60 · fail-closed）
# =============================================================================
# 守的红线（D-09 教训的直接产物）：
#   容器镜像最危险的事故不是「构建失败」，而是「构建成功但层里没有东西」——
#   构建命令与产物路径一旦错位，COPY 的 glob 会静默产出几 KB 的空层，
#   docker build 全绿，容器随后 exec 失败或直接没有前端。
#   本脚本在**镜像层内容**层面逐条断言交付物存在且体积合理，任何一条不满足即非零退出。
#
# 为什么必须解到 layer 这一层：
#   `docker save` 的输出不是文件系统树，而是 OCI/legacy 布局的容器包
#   （docker 25+ 默认 OCI：blobs/sha256/<hex>；旧布局：<sha>/layer.tar）。
#   交付物路径藏在各自的 layer.tar（可能是 gzip）内部，只看顶层条目必然误判。
#   故本脚本逐层解压、在**内层** tar 里查找目标路径。
#
# 用法：
#   deploy/scripts/check-image-contents.sh <镜像:tag>
#
# 退出码：
#   0  四条交付物均存在且体积达标
#   1  任一交付物缺失 / 体积不达标 / 环境前置不满足
# =============================================================================

set -euo pipefail

readonly SCRIPT_NAME="check-image-contents"
readonly SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

log()  { printf '[%s] %s\n' "${SCRIPT_NAME}" "$*"; }
fail() { printf '[%s] 致命错误：%s\n' "${SCRIPT_NAME}" "$*" >&2; exit 1; }

IMAGE="${1:-}"
[[ -n "${IMAGE}" ]] || fail "用法：${0#*/} <镜像:tag>"

for cmd in docker python3; do
    command -v "${cmd}" >/dev/null 2>&1 || fail "缺少必要命令：${cmd}"
done

TAR="${IOT_DAQ_IMAGE_TAR:-}"
if [[ -z "${TAR}" ]]; then
    TAR="$(mktemp -t iot-daq-image-check.XXXXXX)"
    trap 'rm -f "${TAR}"' EXIT
fi
log "docker save ${IMAGE} -> ${TAR}"
docker save "${IMAGE}" -o "${TAR}" || fail "docker save 失败：${IMAGE}"

# -----------------------------------------------------------------------------
# 逐层解包并断言。python3 用内联脚本，避免额外依赖（ubuntu / debian 均预装）。
# -----------------------------------------------------------------------------
python3 - "${TAR}" <<'PYEOF'
import io
import sys
import tarfile
import gzip

tar_path = sys.argv[1]

# 目标交付物 → 最小字节数。下限用于拦住「空层 / 空壳二进制」这类静默事故。
REQUIRED = {
    "usr/local/bin/iot-daq-daemon": 1024,
    "usr/local/bin/healthprobe": 1024,
    "srv/web-console/index.html": 1,
    "etc/iot-daq/gateway.toml": 1,
}

found = {}
with tarfile.open(tar_path) as outer:
    for member in outer:
        if not member.isfile():
            continue
        raw = outer.extractfile(member).read()
        inner = None
        # 逐 blob 试解、不依赖命名：docker save 的 OCI 布局里 layer blob 就叫
        # blobs/sha256/<hex>（无 .tar 后缀，docker 25+），旧布局才是 <sha>/layer.tar。
        # 先试 gzip（OCI layer 通常压缩），再试裸 tar（旧布局）；JSON 清单会自然解析失败。
        for opener in (gzip.decompress, lambda b: b):
            try:
                inner = tarfile.open(fileobj=io.BytesIO(opener(raw)))
                break
            except Exception:
                continue
        if inner is None:
            continue
        with inner:
            for entry in inner:
                if entry.isfile() and entry.name in REQUIRED:
                    found[entry.name] = entry.size

missing = sorted(set(REQUIRED) - set(found))
if missing:
    sys.exit("镜像层内缺失交付物：%s" % ", ".join(missing))

for path, size in sorted(found.items()):
    limit = REQUIRED[path]
    if size < limit:
        sys.exit("交付物体积过小（疑似空层）：%s = %d 字节，要求 >= %d" % (path, size, limit))
    print("OK  %-38s %10d bytes" % (path, size))

print("镜像层交付物断言通过。")
PYEOF
