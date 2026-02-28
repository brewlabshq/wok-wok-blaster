use anyhow::Result;
use quinn::crypto::rustls::QuicClientConfig;
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use std::sync::Arc;

pub struct CertPair {
    pub cert: CertificateDer<'static>,
    pub key: PrivatePkcs8KeyDer<'static>,
}

pub fn generate_self_signed() -> Result<CertPair> {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])?;
    let key = PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());
    let cert_der = CertificateDer::from(cert.cert);
    Ok(CertPair {
        cert: cert_der,
        key,
    })
}

fn blaster_transport_config() -> quinn::TransportConfig {
    let mut transport = quinn::TransportConfig::default();
    // 64MB stream receive window (default ~1MB) — prevents flow control stalls on large payloads
    transport.stream_receive_window(quinn::VarInt::from_u32(64 * 1024 * 1024));
    // 64MB connection-level receive window
    transport.receive_window(quinn::VarInt::from_u32(64 * 1024 * 1024));
    // 64MB send window
    transport.send_window(64 * 1024 * 1024);
    // Large initial congestion window via Cubic config (default ~14KB)
    let mut cubic = quinn::congestion::CubicConfig::default();
    cubic.initial_window(10 * 1024 * 1024);
    transport.congestion_controller_factory(Arc::new(cubic));
    // Disable datagram buffer (we use streams only)
    transport.datagram_receive_buffer_size(None);
    // Enable GSO if available
    transport.enable_segmentation_offload(true);
    // Allow up to 1024 concurrent bidirectional streams per connection
    transport.max_concurrent_bidi_streams(quinn::VarInt::from_u32(1024));
    transport
}

pub fn server_config(pair: &CertPair) -> Result<quinn::ServerConfig> {
    let mut server_crypto = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![pair.cert.clone()], pair.key.clone_key().into())?;
    server_crypto.alpn_protocols = vec![b"blaster".to_vec()];
    // Enable 0-RTT early data (max u32 = unlimited)
    server_crypto.max_early_data_size = u32::MAX;
    // Send two session tickets per connection for 0-RTT resumption
    server_crypto.send_tls13_tickets = 2;

    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(server_crypto)?,
    ));
    server_config.transport_config(Arc::new(blaster_transport_config()));
    Ok(server_config)
}

pub fn client_config() -> Result<quinn::ClientConfig> {
    let mut client_crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipVerification))
        .with_no_client_auth();
    client_crypto.alpn_protocols = vec![b"blaster".to_vec()];
    // Enable session resumption with in-memory cache (needed for 0-RTT)
    client_crypto.resumption = rustls::client::Resumption::in_memory_sessions(256);
    // Enable early data (0-RTT) on the client
    client_crypto.enable_early_data = true;

    let mut client_config =
        quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(client_crypto)?));
    client_config.transport_config(Arc::new(blaster_transport_config()));
    Ok(client_config)
}

#[derive(Debug)]
struct SkipVerification;

impl rustls::client::danger::ServerCertVerifier for SkipVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            rustls::SignatureScheme::ED25519,
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::RSA_PSS_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA512,
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::RSA_PKCS1_SHA384,
            rustls::SignatureScheme::RSA_PKCS1_SHA512,
        ]
    }
}
