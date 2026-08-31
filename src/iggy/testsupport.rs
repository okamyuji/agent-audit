//! 結合テスト・E2E 専用。実 Iggy コンテナを起動して `(addr, pat, container名)` を返す。
#![doc(hidden)]

use std::process::Command;
use std::time::Duration;

use iggy::prelude::*;

pub const IGGY_ADDR: &str = "127.0.0.1:8090";

/// `docker rm -f` on drop, so every early-return path (including bare `?`)
/// still tears the container down.
pub struct ContainerGuard(String);

impl ContainerGuard {
    pub fn new(container: String) -> Self {
        Self(container)
    }
}

impl Drop for ContainerGuard {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["rm", "-f", &self.0]).status();
    }
}

/// コンテナログから "Generated root user password: <pw>" 行を待って抽出する
async fn wait_for_generated_root_password(
    container: &str,
    deadline: &std::time::Instant,
) -> anyhow::Result<String> {
    const MARKER: &str = "Generated root user password: ";
    loop {
        let output = Command::new("docker").args(["logs", container]).output()?;
        let logs = String::from_utf8_lossy(&output.stdout);
        let logs_err = String::from_utf8_lossy(&output.stderr);
        if let Some(line) = logs
            .lines()
            .chain(logs_err.lines())
            .find(|l| l.contains(MARKER))
        {
            let pw = line[line.find(MARKER).unwrap() + MARKER.len()..].trim();
            return Ok(pw.to_string());
        }
        if std::time::Instant::now() >= *deadline {
            anyhow::bail!("did not find generated root password in container logs within deadline");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// apache/iggy:0.8.0 を立て、root ユーザーで PAT を作って `(addr, pat, container名)` を返す。
/// PAT の作成は iggy の Rust クライアントで行う（PersonalAccessTokenClient::create_personal_access_token）。
///
/// 呼び出し側がコンテナの後始末を担う（`ContainerGuard::new(container)` で包むこと）。
/// 結合テストと E2E は同じ固定ポート（host network）を使うため、cargo test が
/// 統合テストバイナリを直列実行することに依存して同時起動を避けている
/// （cargo は既定でテストバイナリを1つずつ実行するため、並列化するテストランナー
/// （nextest 等）を使わない限り安全）。
pub async fn start_iggy() -> anyhow::Result<(String, String, String)> {
    let container = format!("agent-audit-test-iggy-{}", std::process::id());
    // `--rm` is deliberately omitted: `docker stop` on a `--rm` container removes
    // it immediately, which would make a later `docker start` (used by the E2E
    // reconnect flow) impossible. `ContainerGuard` guarantees `docker rm -f` cleanup
    // instead.
    let status = Command::new("docker")
        .args([
            "run",
            "-d",
            "--name",
            &container,
            // iggy-server uses io_uring, which the default seccomp profile blocks
            // inside colima/Docker Desktop sandboxes.
            "--security-opt",
            "seccomp=unconfined",
            // colima のブリッジ + ポートフォワード (-p host:container) 経由だと、iggy-server が
            // TCP 接続を accept 直後に理由なく閉じる（生ソケットで再現確認済み・PING応答が空）。
            // --network host なら同じサーバー/カーネルで正常応答するため host network を使う。
            "--network",
            "host",
            "apache/iggy:0.8.0",
        ])
        .status()?;
    anyhow::ensure!(status.success(), "docker run failed");
    // Guard from here on: any `?` below tears the container down on drop if we
    // return early. On success we forget it — the caller now owns cleanup.
    let guard = ContainerGuard::new(container.clone());

    // TCP 8090 が開くまで最大30秒待つ
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if std::net::TcpStream::connect(IGGY_ADDR).is_ok() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("iggy did not open {IGGY_ADDR} within 30s");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // このイメージは初回起動時にランダムな root パスワードを生成し、
    // コンテナログに1行 "Generated root user password: <pw>" として出力する
    // （固定の "iggy"/"iggy" では Invalid credentials になる。生ソケットで確認済み）。
    let root_password = wait_for_generated_root_password(&container, &deadline).await?;

    // ポートは開いてもサーバーがログインを受け付けるまで少し遅延することがあるので数回リトライする
    let client = IggyClientBuilder::new()
        .with_tcp()
        .with_server_address(IGGY_ADDR.to_string())
        .build()?;
    client.connect().await?;

    let mut last_err = None;
    for _ in 0..50 {
        match client.login_user("iggy", &root_password).await {
            Ok(_) => {
                last_err = None;
                break;
            }
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
    if let Some(e) = last_err {
        anyhow::bail!("login_user failed: {e}");
    }

    let pat = client
        .create_personal_access_token("test", PersonalAccessTokenExpiry::NeverExpire)
        .await?;

    std::mem::forget(guard);
    Ok((IGGY_ADDR.to_string(), pat.token, container))
}
