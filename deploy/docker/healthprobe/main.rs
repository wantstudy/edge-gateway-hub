// =============================================================================
// iot-daq · distroless 容器健康探针（task 60 → D-06 修复，deploy-fix）
// =============================================================================
// 定位：runtime 镜像为 gcr.io/distroless/static-debian12（无 shell、无 curl/wget），
//   compose 的 healthcheck 无法用 CMD-SHELL + /dev/tcp（D-06：健康检查必失败）。
//   本文件提供一个纯 std、零依赖的静态探针二进制，随镜像分发：
//     - 每次执行做一次 TCP connect 到管理面端口
//     - 成功 → exit 0；失败 → exit 1（非零即 unhealthy）
//   由 compose healthcheck 以 exec-form 调用：test: ["CMD", "/usr/local/bin/healthprobe"]
//
// 端口来源（容器内环境变量，与 docker-compose.yml 注入保持一致）：
//   IOT_DAQ_HTTP_PORT（默认 8080）
//   说明：daemon 管理面绑定消费 IOT_DAQ_HTTP_BIND（默认 127.0.0.1:8080，见
//   runtime-fix 契约）；探针始终探测容器内 loopback，因此只关心端口号。
//
// 构建方式（见 deploy/docker/Dockerfile builder 阶段）：
//   rustc --target <triple> -C target-feature=+crt-static -O main.rs -o healthprobe
//   —— 无任何外部 crate 依赖，不触碰 Cargo.lock / crates.io 红线。
// =============================================================================

use std::env;
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

/// 连接超时（秒）。须小于 compose healthcheck 的 timeout（默认 5s）。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

fn main() {
    // 端口：优先 IOT_DAQ_HTTP_PORT，非法/缺失时回退默认 8080（与 compose 默认一致）。
    let port: u16 = env::var("IOT_DAQ_HTTP_PORT")
        .ok()
        .and_then(|v| v.trim().parse::<u16>().ok())
        .unwrap_or(8080);

    // 固定探测容器内 loopback：管理面无论绑定 0.0.0.0 还是具体网卡，loopback 均可达。
    let addr = SocketAddr::from(([127, 0, 0, 1], port));

    match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
        Ok(_stream) => {
            // 连接成功即视为管理面 TCP 栈存活；HTTP 语义校验（/healthz）由
            // install.sh 在宿主侧补做（container-deploy.md §3.1）。
            std::process::exit(0);
        }
        Err(err) => {
            // 输出到 stderr 便于 `docker inspect` 的 Health.Log 排查；无 shell 也可读。
            eprintln!(
                "healthprobe: TCP connect {}:{} failed: {}",
                addr.ip(),
                addr.port(),
                err
            );
            std::process::exit(1);
        }
    }
}
