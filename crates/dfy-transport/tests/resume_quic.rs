//! 真实 loopback QUIC 会话迁移：新连接重新认证，再完成签名 PREPARE/COMMIT。

use std::time::Duration;

use anyhow::{Context, Result};
use dfy_proto::{envelope, Envelope, PERM_VIEW, SESSION_RESUME_COMMIT, SESSION_RESUME_PREPARE};
use dfy_transport::{
    bind_agent_random, client_handshake, client_resume_handshake, client_tls, connect,
    server_handshake_context, ServerAuth,
};
use ed25519_dalek::SigningKey;
use quinn::{Connection, Endpoint};

const PASSWORD: &str = "migration-test-password";
const TIMEOUT: Duration = Duration::from_secs(20);

struct Pair {
    client: Connection,
    server: Connection,
    client_ep: Endpoint,
    server_ep: Endpoint,
}

impl Pair {
    async fn new(pin: bool) -> Result<Self> {
        let (server_ep, identity, addr) = bind_agent_random()?;
        let tls = client_tls(pin.then_some(identity.fingerprint()))?;
        let (server, client) = tokio::time::timeout(TIMEOUT, async {
            tokio::join!(accept(&server_ep), connect(addr, tls))
        })
        .await?;
        let (client_ep, client) = client?;
        Ok(Self {
            client,
            server: server?,
            client_ep,
            server_ep,
        })
    }
}

impl Drop for Pair {
    fn drop(&mut self) {
        self.client_ep.close(0u32.into(), b"done");
        self.server_ep.close(0u32.into(), b"done");
    }
}

async fn accept(endpoint: &Endpoint) -> Result<Connection> {
    endpoint
        .accept()
        .await
        .context("endpoint closed")?
        .await
        .context("accept failed")
}

#[tokio::test]
async fn authenticated_new_path_preserves_session_and_rotates_token() -> Result<()> {
    let auth = ServerAuth::new(PASSWORD, "migration-agent");
    let key = SigningKey::from_bytes(&[42; 32]);
    let pubkey = key.verifying_key().to_bytes().to_vec();
    let first = Pair::new(true).await?;
    let (server_result, client_result) = tokio::time::timeout(TIMEOUT, async {
        tokio::join!(
            server_handshake_context(&first.server, &auth),
            client_handshake(&first.client, PASSWORD, "migration-client", pubkey.clone()),
        )
    })
    .await?;
    let initial = server_result?;
    let (previous, _first_client_chan) = client_result?;
    assert_eq!(previous, initial.welcome);
    assert_eq!(previous.permission_bitmap, PERM_VIEW);
    assert_eq!(previous.resume_token.len(), 32);

    let second = Pair::new(true).await?;
    let (server_result, client_result) = tokio::time::timeout(TIMEOUT, async {
        tokio::join!(
            async {
                let context = server_handshake_context(&second.server, &auth).await?;
                let mut chan = context.chan;
                for expected in [SESSION_RESUME_PREPARE, SESSION_RESUME_COMMIT] {
                    let env = chan.recv_ctrl().await?.context("missing resume request")?;
                    let request = match env.body {
                        Some(envelope::Body::SessionResume(request)) => request,
                        _ => anyhow::bail!("expected SessionResume"),
                    };
                    assert_eq!(request.phase, expected);
                    let reply = dfy_transport::server_resume_request(
                        &auth,
                        &context.hello,
                        &context.exporter,
                        &request,
                        777,
                    )?;
                    chan.send_ctrl(&Envelope {
                        body: Some(envelope::Body::SessionResumeOk(reply)),
                    })
                    .await?;
                }
                anyhow::Ok(context.welcome)
            },
            client_resume_handshake(
                &second.client,
                PASSWORD,
                "migration-client",
                &key,
                &previous,
                700,
                3,
            ),
        )
    })
    .await?;
    let _temporary_welcome = server_result?;
    let (resumed, _chan) = client_result?;
    assert_eq!(resumed.session_id, previous.session_id);
    assert_eq!(resumed.session_epoch, previous.session_epoch + 1);
    assert_eq!(resumed.permission_bitmap, previous.permission_bitmap);
    assert_eq!(resumed.capability, previous.capability);
    assert_ne!(resumed.resume_token, previous.resume_token);
    Ok(())
}
