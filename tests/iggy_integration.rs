use std::process::Command;
use std::time::Duration;

use agent_audit::iggy::{fetch_all, Backend, IggyBackend};
use iggy::prelude::*;

const IGGY_ADDR: &str = "127.0.0.1:8090";

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

fn docker_available() -> bool {
    Command::new("docker")
        .arg("info")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// apache/iggy:0.8.0 を立て、root ユーザーで PAT を作って返す。
/// PAT の作成は iggy の Rust クライアントで行う（PersonalAccessTokenClient::create_personal_access_token）
async fn start_iggy() -> anyhow::Result<(String, String)> {
    let container = format!("agent-audit-test-iggy-{}", std::process::id());
    let status = Command::new("docker")
        .args([
            "run",
            "-d",
            "--rm",
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

    // TCP 8090 が開くまで最大30秒待つ
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if std::net::TcpStream::connect(IGGY_ADDR).is_ok() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            let _ = Command::new("docker")
                .args(["rm", "-f", &container])
                .status();
            anyhow::bail!("iggy did not open {IGGY_ADDR} within 30s");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // このイメージは初回起動時にランダムな root パスワードを生成し、
    // コンテナログに1行 "Generated root user password: <pw>" として出力する
    // （固定の "iggy"/"iggy" では Invalid credentials になる。生ソケットで確認済み）。
    let root_password = wait_for_generated_root_password(&container, &deadline).await;
    let root_password = match root_password {
        Ok(pw) => pw,
        Err(e) => {
            let _ = Command::new("docker")
                .args(["rm", "-f", &container])
                .status();
            return Err(e);
        }
    };

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
        let _ = Command::new("docker")
            .args(["rm", "-f", &container])
            .status();
        anyhow::bail!("login_user failed: {e}");
    }

    let pat = client
        .create_personal_access_token("test", PersonalAccessTokenExpiry::NeverExpire)
        .await?;

    Ok((pat.token, container))
}

#[tokio::test]
async fn round_trip_through_real_iggy() -> anyhow::Result<()> {
    if !docker_available() {
        eprintln!("skip: docker not available");
        return Ok(());
    }
    let (pat, container) = start_iggy().await?;
    let result = run_round_trip(&pat).await;
    let _ = Command::new("docker")
        .args(["rm", "-f", &container])
        .status();
    result
}

async fn run_round_trip(pat: &str) -> anyhow::Result<()> {
    let be = IggyBackend::connect(IGGY_ADDR, "agent-audit-test", pat).await?;
    // stream と topic を作り、3 件 produce する（iggy クライアントの create_stream / create_topic / send_messages）
    be.ensure_stream_for_test().await?;
    be.produce_for_test("sess-1", &[b"a".to_vec(), b"b".to_vec(), b"c".to_vec()])
        .await?;
    assert_eq!(be.list_sessions().await?, vec!["sess-1".to_string()]);
    let (payloads, next) = fetch_all(&be, "sess-1").await?;
    assert_eq!(payloads, vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
    assert_eq!(next, 3);
    // IggyMessageHeader の offset フィールド名は 0.10.0 で message.header.offset。
    // コンパイルが通ればこの前提は確認済みになる
    Ok(())
}
