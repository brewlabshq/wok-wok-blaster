use anyhow::Result;
use quinn::crypto::rustls::QuicClientConfig;
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use std::net::UdpSocket;
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

/// Enlarge the OS-level UDP socket buffers (SO_SNDBUF / SO_RCVBUF).
/// The kernel default (~208 KB) is a bottleneck for high-throughput QUIC.
/// Errors are logged but not fatal — the kernel may cap to rmem_max/wmem_max.
pub fn enlarge_socket_buffers(socket: &UdpSocket, size: usize) {
    use std::os::unix::io::AsRawFd;
    let fd = socket.as_raw_fd();
    for opt in [libc::SO_SNDBUF, libc::SO_RCVBUF] {
        let val = size as libc::c_int;
        let ret = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                opt,
                &val as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if ret != 0 {
            tracing::warn!(
                "setsockopt({}) failed: {}",
                if opt == libc::SO_SNDBUF { "SO_SNDBUF" } else { "SO_RCVBUF" },
                std::io::Error::last_os_error(),
            );
        }
    }
}

/// Endpoint config tuned for large payloads.
pub fn blaster_endpoint_config() -> quinn::EndpointConfig {
    let mut config = quinn::EndpointConfig::default();
    // Raise max UDP payload from 1472 to 65527 (max allowed).
    // PMTU discovery will negotiate the actual path MTU down if needed.
    let _ = config.max_udp_payload_size(65527);
    config
}

fn blaster_transport_config() -> quinn::TransportConfig {
    let mut transport = quinn::TransportConfig::default();

    // ── Flow-control windows (sized for 10 MB+ payloads) ─────────────
    // 128 MB stream receive window (default ~1 MB)
    transport.stream_receive_window(quinn::VarInt::from_u32(128 * 1024 * 1024));
    // 256 MB connection-level receive window (covers multiple concurrent streams)
    transport.receive_window(quinn::VarInt::from_u32(256 * 1024 * 1024));
    // 128 MB send window
    transport.send_window(128 * 1024 * 1024);

    // ── Congestion control ───────────────────────────────────────────
    // BBR is rate-based (not loss-based like Cubic) — it probes available
    // bandwidth directly, so it fills the pipe on the first RTT instead of
    // slowly ramping up.  Much better for large payloads on fast links.
    let mut bbr = quinn::congestion::BbrConfig::default();
    bbr.initial_window(32 * 1024 * 1024);
    transport.congestion_controller_factory(Arc::new(bbr));

    // ── Timeouts & keep-alive ────────────────────────────────────────
    // 60 s idle timeout — large transfers may stall briefly between chunks
    transport.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(std::time::Duration::from_secs(60)).unwrap(),
    ));
    // Keep-alive every 15 s to prevent NAT/firewall timeouts
    transport.keep_alive_interval(Some(std::time::Duration::from_secs(15)));

    // ── MTU ──────────────────────────────────────────────────────────
    // Use 1400-byte initial MTU (safe for most paths) with PMTU discovery
    transport.initial_mtu(1400);
    transport.min_mtu(1200);

    // ── Streams & misc ───────────────────────────────────────────────
    // Disable datagram buffer (we use streams only)
    transport.datagram_receive_buffer_size(None);
    // Enable GSO if available
    transport.enable_segmentation_offload(true);
    // Allow up to 2048 concurrent unidirectional streams per connection
    transport.max_concurrent_uni_streams(quinn::VarInt::from_u32(2048));

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
