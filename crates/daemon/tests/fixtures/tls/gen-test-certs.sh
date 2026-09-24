#!/usr/bin/env bash
#
# TEST_ONLY —— 仅测试用自签证书，禁止用于生产！
# ============================================================================
# 本脚本生成的证书 / 私钥**仅供 crates/daemon/tests/tls_mtls_handshake.rs 的
# 本地握手单测使用**。它们是自签的、无任何真实信任链、私钥公开在仓库里——
# 严禁导入任何真实环境、严禁用于任何生产 / 预生产 / 共享环境。
# ============================================================================
#
# 生成物（本目录，ECDSA P-256 / PKCS#8 私钥）：
#   ca.crt / ca.key                    —— 测试根 CA
#   server.crt / server.key            —— 服务端证书（SAN 含 localhost + 127.0.0.1）
#   client.crt / client.key            —— 合法客户端证书（mTLS 正例）
#   rogue-ca.crt / rogue-ca.key        —— 无关的「另一张」自签 CA（反例）
#   rogue-client.crt / rogue-client.key —— 由 rogue-ca 签发（mTLS 反例：不受信）
#
# 幂等：重复执行会覆盖全部生成物（先清理 *.srl / 中间 CSR）。
#
set -euo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$DIR"

DAYS=3650

# 清理旧产物，保证幂等 / 干净重建。
rm -f ca.crt ca.key ca.srl \
      server.crt server.key server.csr server.ext \
      client.crt client.key client.csr client.ext \
      rogue-ca.crt rogue-ca.key rogue-ca.srl \
      rogue-client.crt rogue-client.key rogue-client.csr rogue-client.ext

# ── 1. 测试根 CA（自签，CA:TRUE）────────────────────────────────────────────
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 \
  -keyout ca.key -out ca.crt -days "$DAYS" -nodes \
  -subj "/CN=iot-daq TEST-ONLY Root CA" \
  -addext "basicConstraints=critical,CA:TRUE" \
  -addext "keyUsage=critical,keyCertSign,cRLSign"

# ── 2. 服务端证书（SAN: localhost + 127.0.0.1；serverAuth）──────────────────
openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 \
  -keyout server.key -out server.csr -nodes \
  -subj "/CN=localhost"

cat > server.ext <<'EOF'
basicConstraints=CA:FALSE
keyUsage=critical,digitalSignature,keyEncipherment
extendedKeyUsage=serverAuth
subjectAltName=DNS:localhost,IP:127.0.0.1
EOF

openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial \
  -out server.crt -days "$DAYS" -extfile server.ext

# ── 3. 合法客户端证书（clientAuth，由 ca.crt 签发）──────────────────────────
openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 \
  -keyout client.key -out client.csr -nodes \
  -subj "/CN=iot-daq TEST-ONLY Client"

cat > client.ext <<'EOF'
basicConstraints=CA:FALSE
keyUsage=critical,digitalSignature
extendedKeyUsage=clientAuth
EOF

openssl x509 -req -in client.csr -CA ca.crt -CAkey ca.key -CAcreateserial \
  -out client.crt -days "$DAYS" -extfile client.ext

# ── 4. 无关的第二张自签 CA + 其签发的客户端证书（反例：不受信）──────────────
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 \
  -keyout rogue-ca.key -out rogue-ca.crt -days "$DAYS" -nodes \
  -subj "/CN=iot-daq TEST-ONLY Rogue CA" \
  -addext "basicConstraints=critical,CA:TRUE" \
  -addext "keyUsage=critical,keyCertSign,cRLSign"

openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 \
  -keyout rogue-client.key -out rogue-client.csr -nodes \
  -subj "/CN=iot-daq TEST-ONLY Rogue Client"

cat > rogue-client.ext <<'EOF'
basicConstraints=CA:FALSE
keyUsage=critical,digitalSignature
extendedKeyUsage=clientAuth
EOF

openssl x509 -req -in rogue-client.csr -CA rogue-ca.crt -CAkey rogue-ca.key \
  -CAcreateserial -out rogue-client.crt -days "$DAYS" -extfile rogue-client.ext

# ── 清理中间产物（保留最终 PEM）─────────────────────────────────────────────
rm -f ca.srl server.csr server.ext client.csr client.ext \
      rogue-ca.srl rogue-client.csr rogue-client.ext

echo "TEST_ONLY test certificates generated in: $DIR"
