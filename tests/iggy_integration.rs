use std::process::Command;

use agent_audit::iggy::testsupport::{start_iggy, ContainerGuard};
use agent_audit::iggy::{fetch_all, Backend, IggyBackend};

fn docker_available() -> bool {
    Command::new("docker")
        .arg("info")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[tokio::test]
async fn round_trip_through_real_iggy() -> anyhow::Result<()> {
    if !docker_available() {
        eprintln!("skip: docker not available");
        return Ok(());
    }
    let (addr, pat, container) = start_iggy().await?;
    let _guard = ContainerGuard::new(container);
    run_round_trip(&addr, &pat).await
}

async fn run_round_trip(addr: &str, pat: &str) -> anyhow::Result<()> {
    let be = IggyBackend::connect(addr, "agent-audit-test", pat, false).await?;
    // stream と topic を作り、3 件 produce する（iggy クライアントの create_stream / create_topic / send_messages）
    be.ensure_stream_for_test().await?;
    be.produce_for_test("sess-1", &[b"a".to_vec(), b"b".to_vec(), b"c".to_vec()])
        .await?;
    assert_eq!(be.list_sessions().await?, vec!["sess-1".to_string()]);
    let (payloads, next) = fetch_all(&be, "sess-1", std::time::Duration::from_secs(30)).await?;
    assert_eq!(payloads, vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
    assert_eq!(next, 3);
    // IggyMessageHeader の offset フィールド名は 0.10.0 で message.header.offset。
    // コンパイルが通ればこの前提は確認済みになる
    Ok(())
}
