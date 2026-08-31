use anyhow::Context;
use async_trait::async_trait;
#[cfg(feature = "testsupport")]
use bytes::Bytes;
use iggy::prelude::*;

/// 結合テスト・E2E から実 Iggy コンテナを起動するためのヘルパー。
/// 本番コードからは使わない。リリースバイナリに書き込み系APIを含めないよう
/// feature gateする（`cargo build --release` はtestsupportを付けない）。
#[cfg(feature = "testsupport")]
pub mod testsupport;

pub const POLL_BATCH: u32 = 100;

#[async_trait]
pub trait Backend: Send + Sync {
    async fn list_sessions(&self) -> anyhow::Result<Vec<String>>;
    async fn fetch_from(&self, session: &str, offset: u64) -> anyhow::Result<(Vec<Vec<u8>>, u64)>;
}

pub struct IggyBackend {
    client: IggyClient,
    stream: String,
}

impl IggyBackend {
    pub async fn connect(addr: &str, stream: &str, pat: &str) -> anyhow::Result<Self> {
        let client = IggyClientBuilder::new()
            .with_tcp()
            .with_server_address(addr.to_string())
            .build()?;
        client.connect().await.context("connect to iggy")?;
        client
            .login_with_personal_access_token(pat)
            .await
            .context("login with IGGY_PAT")?;
        Ok(Self {
            client,
            stream: stream.to_string(),
        })
    }

    fn stream_id(&self) -> anyhow::Result<Identifier> {
        Ok(Identifier::named(&self.stream)?)
    }

    /// 結合テスト用。stream が無ければ作る
    #[cfg(feature = "testsupport")]
    pub async fn ensure_stream_for_test(&self) -> anyhow::Result<()> {
        if self.client.get_stream(&self.stream_id()?).await?.is_none() {
            self.client.create_stream(&self.stream).await?;
        }
        Ok(())
    }

    /// 結合テスト用。topic が無ければ作り、payload を順に送る
    #[cfg(feature = "testsupport")]
    pub async fn produce_for_test(
        &self,
        session: &str,
        payloads: &[Vec<u8>],
    ) -> anyhow::Result<()> {
        let sid = self.stream_id()?;
        let tid = Identifier::named(session)?;
        if self.client.get_topic(&sid, &tid).await?.is_none() {
            self.client
                .create_topic(
                    &sid,
                    session,
                    1,
                    CompressionAlgorithm::None,
                    None,
                    IggyExpiry::NeverExpire,
                    MaxTopicSize::ServerDefault,
                )
                .await?;
        }
        let mut msgs: Vec<IggyMessage> = payloads
            .iter()
            .map(|p| {
                IggyMessage::builder()
                    .payload(Bytes::from(p.clone()))
                    .build()
            })
            .collect::<Result<_, _>>()?;
        self.client
            .send_messages(&sid, &tid, &Partitioning::partition_id(0), &mut msgs)
            .await?;
        Ok(())
    }
}

#[async_trait]
impl Backend for IggyBackend {
    async fn list_sessions(&self) -> anyhow::Result<Vec<String>> {
        let topics = self.client.get_topics(&self.stream_id()?).await?;
        Ok(topics.into_iter().map(|t| t.name).collect())
    }

    async fn fetch_from(&self, session: &str, offset: u64) -> anyhow::Result<(Vec<Vec<u8>>, u64)> {
        let polled = self
            .client
            .poll_messages(
                &self.stream_id()?,
                &Identifier::named(session)?,
                Some(0),
                &Consumer::default(),
                &PollingStrategy::offset(offset),
                POLL_BATCH,
                false,
            )
            .await?;
        let mut next = offset;
        let mut out = Vec::with_capacity(polled.messages.len());
        for m in polled.messages {
            next = m.header.offset + 1;
            out.push(m.payload.to_vec());
        }
        Ok((out, next))
    }
}

/// offset 0 から空になるまで読む
pub async fn fetch_all(b: &dyn Backend, session: &str) -> anyhow::Result<(Vec<Vec<u8>>, u64)> {
    let mut all = Vec::new();
    let mut off = 0;
    loop {
        let (batch, next) = b.fetch_from(session, off).await?;
        if batch.is_empty() {
            return Ok((all, off));
        }
        all.extend(batch);
        off = next;
    }
}
