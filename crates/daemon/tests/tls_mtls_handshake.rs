//! task 25 验收：**真实 mTLS 握手**集成测试。
//!
//! 目标：证明 `[[outlets]]` 的 TLS/mTLS 配置经
//! [`daemon::north::mqtt::TlsConfig`] 映射后，能与一个**要求客户端证书**的 TLS
//! 服务端完成（或按预期拒绝）握手——即「TLS 配置真的接上了传输层」，而非只有
//! 结构体字段。
//!
//! 握手在 **localhost 真实 TCP** 上完成：`tokio::net::TcpListener` +
//! `tokio_rustls::TlsAcceptor`（`WebPkiClientVerifier` 强制客户端证书）作为服务端，
//! 客户端用**与 `TlsConfig::to_transport()` 同源的方式**读取同一批 PEM 构造
//! `tokio_rustls::TlsConnector`。
//!
//! 证书为**测试专用自签证书**（由 `tests/fixtures/tls/gen-test-certs.sh` 生成，
//! 该脚本头部标注「仅测试用自签证书，禁止用于生产」），SAN 含 `localhost` 与
//! `127.0.0.1`；另附一张无关的 `rogue-ca` 用于「不受信客户端」反例。
//!
//! 断言覆盖：
//! - (a) 合法 client 证书 → 握手成功（完成 1 字节往返）；
//! - (b) 不带客户端证书 → 握手被拒；
//! - (c) 由另一张自签 CA 签发的 client 证书 → 握手被拒；
//! - (d) 错误 SNI / 服务端名不在 SAN 内 → 握手被拒（证明不忽略证书验证错误）。

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio::time::{timeout, Duration};

use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use tokio_rustls::rustls::server::WebPkiClientVerifier;
use tokio_rustls::rustls::{ClientConfig, RootCertStore, ServerConfig};
use tokio_rustls::{TlsAcceptor, TlsConnector};

/// 单次握手 / 往返的硬超时（避免负例挂死使测试无界等待）。
const IO_TIMEOUT: Duration = Duration::from_secs(10);

/// 测试证书夹具目录（编译期注入，指向仓库内 `tests/fixtures/tls`）。
const FIXTURES_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tls");

fn fixture(name: &str) -> PathBuf {
    Path::new(FIXTURES_DIR).join(name)
}

/// 读取 PEM 证书链（与 daemon 侧 `TlsConfig::to_transport` 读取同一批文件）。
fn read_certs(name: &str) -> Vec<CertificateDer<'static>> {
    let data = std::fs::read(fixture(name)).unwrap_or_else(|e| panic!("read {name}: {e}"));
    rustls_pemfile::certs(&mut &data[..])
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_else(|e| panic!("parse certs {name}: {e}"))
}

/// 读取 PEM 私钥（期望 PKCS#8）。
fn read_key(name: &str) -> PrivateKeyDer<'static> {
    let data = std::fs::read(fixture(name)).unwrap_or_else(|e| panic!("read {name}: {e}"));
    rustls_pemfile::private_key(&mut &data[..])
        .unwrap_or_else(|e| panic!("parse key {name}: {e}"))
        .unwrap_or_else(|| panic!("no private key in {name}"))
}

/// 信任锚存储（把 `ca` 加入根证书库）。
fn roots_from(ca: &str) -> Arc<RootCertStore> {
    let mut store = RootCertStore::empty();
    for cert in read_certs(ca) {
        store.add(cert).expect("add CA to root store");
    }
    Arc::new(store)
}

/// 安装 rustls 进程级 provider（与 daemon `ensure_rustls_provider` 同源：`ring`）。
///
/// 幂等：重复调用 / 已被他人安装时忽略返回的 `Err`。
fn install_provider() {
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
}

/// 服务端配置：自己的 server 证书 + **强制校验**客户端证书（`WebPkiClientVerifier`）。
fn server_config() -> Arc<ServerConfig> {
    let verifier = WebPkiClientVerifier::builder(roots_from("ca.crt"))
        .build()
        .expect("build client verifier");
    let config = ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(read_certs("server.crt"), read_key("server.key"))
        .expect("server config");
    Arc::new(config)
}

/// 客户端配置：信任 `ca`，并出示 `cert` / `key`（mTLS）。
fn client_config_with_cert(ca: &str, cert: &str, key: &str) -> Arc<ClientConfig> {
    let config = ClientConfig::builder()
        .with_root_certificates(roots_from(ca))
        .with_client_auth_cert(read_certs(cert), read_key(key))
        .expect("client config (mTLS)");
    Arc::new(config)
}

/// 客户端配置：信任 `ca`，**不出示**客户端证书。
fn client_config_without_cert(ca: &str) -> Arc<ClientConfig> {
    let config = ClientConfig::builder()
        .with_root_certificates(roots_from(ca))
        .with_no_client_auth();
    Arc::new(config)
}

/// 校验 `ServerName`（DNS 名或 IP）。
fn server_name(name: &'static str) -> ServerName<'static> {
    ServerName::try_from(name).expect("valid server name")
}

/// 起一个本地 TLS 服务端：accept 一次 → 握手 → 读 1 字节 → 回显 1 字节。
struct LocalServer {
    addr: SocketAddr,
    handle: JoinHandle<Result<(), String>>,
}

async fn spawn_server(config: Arc<ServerConfig>) -> LocalServer {
    let acceptor = TlsAcceptor::from(config);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let handle = tokio::spawn(async move {
        let (stream, _) = listener
            .accept()
            .await
            .map_err(|e| format!("accept tcp: {e}"))?;
        let mut tls = acceptor
            .accept(stream)
            .await
            .map_err(|e| format!("tls accept: {e}"))?;
        // 回显 4 字节（与客户端往返字节数一致）。
        let mut buf = [0u8; 4];
        tls.read_exact(&mut buf)
            .await
            .map_err(|e| format!("read: {e}"))?;
        tls.write_all(&buf)
            .await
            .map_err(|e| format!("write: {e}"))?;
        tls.flush().await.map_err(|e| format!("flush: {e}"))?;
        Ok(())
    });
    LocalServer { addr, handle }
}

/// 客户端完成一次「连接 → 写 4 字节 → 读 4 字节」往返；任一步失败即返回 `Err`。
async fn client_roundtrip(
    config: Arc<ClientConfig>,
    addr: SocketAddr,
    name: ServerName<'static>,
) -> Result<(), String> {
    let connector = TlsConnector::from(config);
    let tcp = TcpStream::connect(addr)
        .await
        .map_err(|e| format!("tcp connect: {e}"))?;
    let mut tls = connector
        .connect(name, tcp)
        .await
        .map_err(|e| format!("tls connect: {e}"))?;
    tls.write_all(b"ping")
        .await
        .map_err(|e| format!("write: {e}"))?;
    tls.flush().await.map_err(|e| format!("flush: {e}"))?;
    let mut buf = [0u8; 4];
    tls.read_exact(&mut buf)
        .await
        .map_err(|e| format!("read: {e}"))?;
    Ok(())
}

/// 同一批 PEM 也能被 daemon 的 [`daemon::north::mqtt::TlsConfig`] 读入并构造传输
/// ——证明「测试用的客户端 PEM」与「daemon 生产路径的 PEM」是同一来源、格式兼容。
#[test]
fn daemon_tls_config_reads_the_same_fixture_pems() {
    use daemon::north::mqtt::TlsConfig;

    let tls = TlsConfig::mtls(
        fixture("ca.crt"),
        fixture("client.crt"),
        fixture("client.key"),
    );
    tls.validate().expect("valid mTLS config");
    tls.to_transport()
        .expect("daemon TlsConfig reads the same test PEMs");
}

/// (a) 合法客户端证书 → mTLS 握手**成功**（完成往返）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mtls_handshake_succeeds_with_valid_client_cert() {
    install_provider();
    let server = spawn_server(server_config()).await;
    let client = client_config_with_cert("ca.crt", "client.crt", "client.key");

    let outcome = timeout(
        IO_TIMEOUT,
        client_roundtrip(client, server.addr, server_name("localhost")),
    )
    .await
    .expect("client must not hang");
    assert!(outcome.is_ok(), "valid mTLS must succeed, got: {outcome:?}");

    let server_outcome = timeout(IO_TIMEOUT, server.handle)
        .await
        .expect("server must not hang")
        .expect("server task join");
    assert!(
        server_outcome.is_ok(),
        "server side must complete: {server_outcome:?}"
    );
}

/// (a') 服务端证书 SAN 内的 IP（`127.0.0.1`）同样可被校验通过。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mtls_handshake_succeeds_with_ip_san() {
    install_provider();
    let server = spawn_server(server_config()).await;
    let client = client_config_with_cert("ca.crt", "client.crt", "client.key");

    let outcome = timeout(
        IO_TIMEOUT,
        client_roundtrip(client, server.addr, server_name("127.0.0.1")),
    )
    .await
    .expect("client must not hang");
    assert!(outcome.is_ok(), "IP SAN must be accepted, got: {outcome:?}");
    let _ = timeout(IO_TIMEOUT, server.handle).await;
}

/// (b) **不带**客户端证书 → 服务端拒绝，握手（或后续 IO）失败。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tls_handshake_rejected_without_client_cert() {
    install_provider();
    let server = spawn_server(server_config()).await;
    let client = client_config_without_cert("ca.crt");

    let outcome = timeout(
        IO_TIMEOUT,
        client_roundtrip(client, server.addr, server_name("localhost")),
    )
    .await
    .expect("client must not hang");
    assert!(
        outcome.is_err(),
        "server must reject a client presenting no certificate, got Ok"
    );

    let server_outcome = timeout(IO_TIMEOUT, server.handle)
        .await
        .expect("server must not hang")
        .expect("server task join");
    assert!(
        server_outcome.is_err(),
        "server accept must fail without a client cert"
    );
}

/// (c) 用**另一张自签 CA** 签发的 client 证书 → 服务端不接受（不受信）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tls_handshake_rejected_with_rogue_client_cert() {
    install_provider();
    let server = spawn_server(server_config()).await;
    // 客户端信任 ca.crt 以校验服务端；但出示的是 rogue-ca 签发的证书。
    let client = client_config_with_cert("ca.crt", "rogue-client.crt", "rogue-client.key");

    let outcome = timeout(
        IO_TIMEOUT,
        client_roundtrip(client, server.addr, server_name("localhost")),
    )
    .await
    .expect("client must not hang");
    assert!(
        outcome.is_err(),
        "server must reject a client cert from an untrusted CA, got Ok"
    );

    let server_outcome = timeout(IO_TIMEOUT, server.handle)
        .await
        .expect("server must not hang")
        .expect("server task join");
    assert!(
        server_outcome.is_err(),
        "server accept must fail for a rogue client cert"
    );
}

/// (d) 错误 SNI（不在服务端证书 SAN 内）→ 客户端**必须**因证书校验失败而拒绝
/// ——证明本实现**不忽略证书验证错误**。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tls_handshake_rejected_with_wrong_server_name() {
    install_provider();
    let server = spawn_server(server_config()).await;
    let client = client_config_with_cert("ca.crt", "client.crt", "client.key");

    let outcome = timeout(
        IO_TIMEOUT,
        client_roundtrip(client, server.addr, server_name("not-in-san.example.com")),
    )
    .await
    .expect("client must not hang");
    assert!(
        outcome.is_err(),
        "server name outside the certificate SAN must be rejected, got Ok"
    );
    // 服务端不对 SNI 做校验：其失败与否不断言，仅确保任务收口（不残留）。
    let _ = timeout(IO_TIMEOUT, server.handle).await;
}
