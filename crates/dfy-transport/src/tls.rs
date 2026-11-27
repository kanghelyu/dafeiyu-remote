//! TLS 身份：自签证书、SPKI 指纹、QuicServerConfig/QuicClientConfig。
//!
//! 设计要点（对应 `spec/DFYP-v1.md` 与会话安全模型）：
//! - Agent 每次启动生成一次性自签证书（`rcgen`），证书指纹 = **SPKI DER 的 SHA-256**，
//!   比整张证书指纹更稳定（换有效期/序列号不影响）。
//! - Client 未指定 pin 时仅观察指纹；传入 `--expect-fingerprint` 时严格比对 SPKI 指纹。
//!   观察结果未持久保存，不构成已实现的 TOFU；应用握手另作 TLS 通道绑定的口令互证。
//! - ALPN 固定 `dfyp/1`，与 `dfy_proto::ALPN` 保持一致。

use std::{
    fmt,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use dfy_proto::ALPN;
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls::{
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::WebPkiSupportedAlgorithms,
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime},
    DigitallySignedStruct, SignatureScheme,
};

use crate::util::{ct_eq, sha256};

/// QUIC 连接用的 SNI（真实身份由指纹校验承担，SNI 仅占位）。
pub const SERVER_NAME: &str = "localhost";

/// 自签证书 + 私钥 + SPKI 指纹。
pub struct TlsIdentity {
    /// 证书 DER（leaf）。
    pub cert_der: CertificateDer<'static>,
    /// PKCS#8 私钥 DER。
    pub key_der: PrivateKeyDer<'static>,
    /// SubjectPublicKeyInfo DER（指纹原像）。
    pub spki_der: Vec<u8>,
    /// `SHA-256(spki_der)`。
    pub fingerprint: [u8; 32],
}

impl fmt::Debug for TlsIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlsIdentity")
            .field("cert_len", &self.cert_der.as_ref().len())
            .field("fingerprint", &self.fingerprint_hex())
            .finish()
    }
}

impl Clone for TlsIdentity {
    fn clone(&self) -> Self {
        Self {
            cert_der: self.cert_der.clone(),
            // `PrivateKeyDer` 本身不是 Clone，必须走 `clone_key()`（显式意图）。
            key_der: self.key_der.clone_key(),
            spki_der: self.spki_der.clone(),
            fingerprint: self.fingerprint,
        }
    }
}

impl TlsIdentity {
    /// 生成一次性自签身份（含 `hostname`、`localhost`、`dfy.local` 三个 SAN）。
    pub fn generate(hostname: &str) -> Result<Self> {
        install_crypto_provider();
        Self::from_key_pair(rcgen::KeyPair::generate()?, hostname)
    }

    /// 从持久化 PKCS#8 私钥重建自签证书，SPKI 指纹不随重启变化。
    pub fn from_pkcs8(key_der: &[u8], hostname: &str) -> Result<Self> {
        install_crypto_provider();
        let key = PrivateKeyDer::try_from(key_der)
            .map_err(|_| anyhow::anyhow!("设备身份不是有效 PKCS#8 私钥"))?;
        let key_pair = rcgen::KeyPair::try_from(&key).context("解析设备身份私钥失败")?;
        Self::from_key_pair(key_pair, hostname)
    }

    fn from_key_pair(key_pair: rcgen::KeyPair, hostname: &str) -> Result<Self> {
        let mut names = vec![hostname.to_string()];
        for extra in ["localhost", "dfy.local"] {
            if !names.iter().any(|n| n == extra) {
                names.push(extra.to_string());
            }
        }
        let params = rcgen::CertificateParams::new(names).context("创建设备证书参数失败")?;
        let certificate = params.self_signed(&key_pair).context("签建设备证书失败")?;
        let spki_der = key_pair.public_key_der();
        let fingerprint = sha256(&spki_der);
        let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pair.serialize_der()));
        Ok(Self {
            cert_der: certificate.der().clone(),
            key_der,
            spki_der,
            fingerprint,
        })
    }

    /// SPKI 指纹（32 字节）。
    pub fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }

    /// SPKI 指纹的小写十六进制（`--expect-fingerprint` 的格式）。
    pub fn fingerprint_hex(&self) -> String {
        hex::encode(self.fingerprint)
    }

    /// 便于终端识别的短指纹（前 8 字节）。
    pub fn fingerprint_short(&self) -> String {
        hex::encode(&self.fingerprint[..8])
    }
}

/// 计算任意证书的 SPKI 指纹（与 [`TlsIdentity::fingerprint`] 同一算法）。
pub fn fingerprint_of_cert(cert: &CertificateDer<'_>) -> Result<[u8; 32]> {
    let parsed = rustls::server::ParsedCertificate::try_from(cert)
        .context("解析对端证书失败（DER 不合法）")?;
    let spki = parsed.subject_public_key_info();
    Ok(sha256(spki.as_ref()))
}

/// 安装 rustls 加密提供者（*ring*）。重复调用安全。
pub fn install_crypto_provider() {
    // 返回 Err 说明已安装过别的 provider，MVP 场景忽略即可。
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// QUIC 传输参数：放宽 DATAGRAM 缓冲，空闲 30s 回收（心跳 5s/15s 更早会先触发）。
pub fn transport_config() -> Arc<quinn::TransportConfig> {
    let mut cfg = quinn::TransportConfig::default();
    cfg.max_concurrent_bidi_streams(16u32.into());
    cfg.max_idle_timeout(Some(
        std::time::Duration::from_secs(30)
            .try_into()
            .expect("30s 在 QUIC idle timeout 范围内"),
    ));
    cfg.keep_alive_interval(None);
    cfg.datagram_send_buffer_size(2 * 1024 * 1024);
    cfg.datagram_receive_buffer_size(Some(2 * 1024 * 1024));
    Arc::new(cfg)
}

/// 由自签身份构造 quinn 服务端配置（ALPN = `dfyp/1`）。
pub fn server_config(identity: &TlsIdentity) -> Result<quinn::ServerConfig> {
    install_crypto_provider();
    let mut rustls_cfg = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![identity.cert_der.clone()],
            identity.key_der.clone_key(),
        )
        .context("构造 rustls ServerConfig 失败（证书/私钥不匹配？）")?;
    rustls_cfg.alpn_protocols = vec![ALPN.to_vec()];
    let quic_cfg = QuicServerConfig::try_from(rustls_cfg)
        .context("rustls ServerConfig 转换为 QUIC 配置失败")?;
    let mut server_cfg = quinn::ServerConfig::with_crypto(Arc::new(quic_cfg));
    server_cfg.transport_config(transport_config());
    Ok(server_cfg)
}

/// 客户端 TLS 侧信息：QUIC 配置 + 实际观察到的服务端指纹（不持久保存）。
#[derive(Clone)]
pub struct ClientTls {
    /// quinn 客户端配置。
    pub config: quinn::ClientConfig,
    /// 连接建立后回填的服务端 SPKI 指纹。
    pub observed: Arc<Mutex<Option<[u8; 32]>>>,
}

impl ClientTls {
    /// 观察到的服务端指纹（十六进制）。
    pub fn observed_hex(&self) -> Option<String> {
        self.observed
            .lock()
            .ok()
            .and_then(|slot| *slot)
            .map(hex::encode)
    }
}

impl fmt::Debug for ClientTls {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientTls")
            .field("observed", &self.observed_hex())
            .finish()
    }
}

/// 构造 quinn 客户端配置。
///
/// `expect` 为 `None` 时接受任意证书（打印指纹）；`Some(fp)` 时严格比对，不匹配即握手失败。
pub fn client_tls(expect: Option<[u8; 32]>) -> Result<ClientTls> {
    install_crypto_provider();
    let observed = Arc::new(Mutex::new(None));
    let verifier = Arc::new(FingerprintVerifier {
        expect,
        observed: observed.clone(),
    });
    let mut rustls_cfg = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    rustls_cfg.alpn_protocols = vec![ALPN.to_vec()];
    let quic_cfg = QuicClientConfig::try_from(rustls_cfg)
        .context("rustls ClientConfig 转换为 QUIC 配置失败")?;
    let mut client_cfg = quinn::ClientConfig::new(Arc::new(quic_cfg));
    client_cfg.transport_config(transport_config());
    Ok(ClientTls {
        config: client_cfg,
        observed,
    })
}

/// 返回当前 rustls provider 的签名算法集合。
fn provider_algorithms() -> WebPkiSupportedAlgorithms {
    rustls::crypto::ring::default_provider().signature_verification_algorithms
}

/// 基于可选 SPKI pin 的证书校验器；无 pin 时仅观察，不保存信任关系。
#[derive(Debug)]
struct FingerprintVerifier {
    expect: Option<[u8; 32]>,
    observed: Arc<Mutex<Option<[u8; 32]>>>,
}

impl ServerCertVerifier for FingerprintVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let fingerprint = fingerprint_of_cert(end_entity).map_err(|_| {
            rustls::Error::InvalidCertificate(rustls::CertificateError::BadEncoding)
        })?;
        if let Ok(mut slot) = self.observed.lock() {
            *slot = Some(fingerprint);
        }
        match self.expect {
            Some(expect) if !ct_eq(&expect, &fingerprint) => Err(
                rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer),
            ),
            _ => Ok(ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &provider_algorithms())
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &provider_algorithms())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        provider_algorithms().supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_stable_and_self_consistent() {
        let id = TlsIdentity::generate("test-agent").expect("生成身份");
        assert_eq!(id.fingerprint_hex().len(), 64);
        // 证书本身解析出的 SPKI 必须与私钥导出的 SPKI 一致
        let from_cert = fingerprint_of_cert(&id.cert_der).expect("从证书取指纹");
        assert_eq!(from_cert, id.fingerprint);
        // SPKI DER 结构：至少是 32 字节公钥 + 头部
        assert!(id.spki_der.len() > 32);
    }

    #[test]
    fn persisted_key_rebuilds_the_same_identity() {
        let first = TlsIdentity::generate("test-agent").expect("生成身份");
        let restored = TlsIdentity::from_pkcs8(first.key_der.secret_der(), "dfy-agent")
            .expect("从私钥恢复身份");
        assert_eq!(first.fingerprint, restored.fingerprint);
        assert_eq!(first.spki_der, restored.spki_der);
        assert_eq!(
            fingerprint_of_cert(&restored.cert_der).unwrap(),
            first.fingerprint
        );
        let _ = server_config(&restored).expect("恢复后的服务端配置");
    }

    #[test]
    fn configs_build() {
        let id = TlsIdentity::generate("test-agent").expect("生成身份");
        let _server = server_config(&id).expect("服务端配置");
        let _client = client_tls(None).expect("客户端配置");
    }

    #[test]
    fn client_tls_is_clonable_and_reports_none_before_connect() {
        let tls = client_tls(Some([7u8; 32])).expect("客户端配置");
        assert!(tls.observed_hex().is_none());
        let cloned = tls.clone();
        assert!(cloned.observed_hex().is_none());
    }
}
