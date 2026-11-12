//! Alternate SNI from tg-ws-proxy, while authenticating the Telegram hostname.
//! Only ClientHello changes; trust, hostname and signature checks stay enabled.
use rustls::{
    client::{
        danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
        WebPkiServerVerifier,
    },
    pki_types::{CertificateDer, ServerName, UnixTime},
    DigitallySignedStruct, SignatureScheme,
};
use std::sync::Arc;

pub(super) const FRONT_SNI: &str = "sprinthost.ru";

#[derive(Debug)]
struct TelegramVerifier {
    inner: Arc<WebPkiServerVerifier>,
    telegram_name: ServerName<'static>,
}

impl ServerCertVerifier for TelegramVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _sni_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.inner.verify_server_cert(
            end_entity,
            intermediates,
            &self.telegram_name,
            ocsp_response,
            now,
        )
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

pub(super) fn fronted_config(
    base: &rustls::ClientConfig,
    verifier: Arc<WebPkiServerVerifier>,
    telegram_domain: &str,
) -> anyhow::Result<rustls::ClientConfig> {
    let mut config = base.clone();
    config
        .dangerous()
        .set_certificate_verifier(Arc::new(TelegramVerifier {
            inner: verifier,
            telegram_name: ServerName::try_from(telegram_domain.to_owned())?,
        }));
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn alternate_sni_requires_a_trusted_certificate_for_the_telegram_host() {
        // Public test certificates only. Never included in production roots.
        // Fixed time keeps verification independent of the machine's calendar.
        let root = CertificateDer::from(include_bytes!("testdata/root.der").as_slice());
        let leaf = CertificateDer::from(include_bytes!("testdata/telegram.der").as_slice());
        let mut roots = rustls::RootCertStore::empty();
        roots.add(root).unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let inner = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone())
            .build()
            .unwrap();
        let front = ServerName::try_from(FRONT_SNI).unwrap();
        let time = UnixTime::since_unix_epoch(Duration::from_secs(1_767_225_600));
        assert!(inner
            .verify_server_cert(&leaf, &[], &front, &[], time)
            .is_err());
        let verifier = TelegramVerifier {
            inner,
            telegram_name: ServerName::try_from("kws2.web.telegram.org").unwrap(),
        };
        assert!(verifier
            .verify_server_cert(&leaf, &[], &front, &[], time)
            .is_ok());
        let wrong_host = TelegramVerifier {
            telegram_name: ServerName::try_from("kws4.web.telegram.org").unwrap(),
            inner: verifier.inner.clone(),
        };
        assert!(wrong_host
            .verify_server_cert(&leaf, &[], &front, &[], time)
            .is_err());
        assert!(verifier
            .verify_server_cert(
                &leaf,
                &[],
                &front,
                &[],
                UnixTime::since_unix_epoch(Duration::from_secs(1))
            )
            .is_err());
        let public_roots =
            rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let untrusted = TelegramVerifier {
            inner: WebPkiServerVerifier::builder_with_provider(Arc::new(public_roots), provider)
                .build()
                .unwrap(),
            telegram_name: verifier.telegram_name,
        };
        assert!(untrusted
            .verify_server_cert(&leaf, &[], &front, &[], time)
            .is_err());
    }
}
