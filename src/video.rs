use std::{
    fmt,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::{bail, ensure, Context, Result};
use bytes::Bytes;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::{broadcast, Mutex, Notify},
    task::JoinHandle,
};
use tokio_rustls::{
    rustls::{
        client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
        crypto::ring,
        pki_types::{CertificateDer, ServerName, UnixTime},
        ClientConfig, DigitallySignedStruct, Error as RustlsError, SignatureScheme,
    },
    TlsConnector,
};
use tracing::{info, warn};

use crate::bambu::{BambuClient, CloudDevice};

pub const DEFAULT_VIDEO_PORT: u16 = 6000;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(15);
const RETRY_INITIAL_DELAY: Duration = Duration::from_secs(1);
const RETRY_MAX_DELAY: Duration = Duration::from_secs(30);
const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;
const MJPEG_BOUNDARY: &str = "frame";

#[derive(Clone, Debug)]
pub struct VideoConfig {
    pub host: Option<String>,
    pub port: u16,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            host: None,
            port: DEFAULT_VIDEO_PORT,
        }
    }
}

#[derive(Clone)]
pub struct VideoRuntime {
    inner: Arc<VideoInner>,
}

struct VideoInner {
    client: BambuClient,
    access_token: String,
    config: VideoConfig,
    tls: TlsConnector,
    parts: broadcast::Sender<Bytes>,
    clients: AtomicUsize,
    no_clients: Notify,
    worker: Mutex<Option<JoinHandle<()>>>,
}

pub struct VideoSubscription {
    receiver: broadcast::Receiver<Bytes>,
    _guard: VideoClientGuard,
}

struct VideoClientGuard {
    inner: Arc<VideoInner>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VideoSession {
    device_id: String,
    access_code: String,
}

pub fn mjpeg_content_type() -> String {
    format!("multipart/x-mixed-replace; boundary={MJPEG_BOUNDARY}")
}

pub fn mjpeg_part(frame: &[u8]) -> Bytes {
    let header = format!(
        "--{MJPEG_BOUNDARY}\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
        frame.len()
    );
    let mut part = Vec::with_capacity(header.len() + frame.len() + 2);
    part.extend_from_slice(header.as_bytes());
    part.extend_from_slice(frame);
    part.extend_from_slice(b"\r\n");
    Bytes::from(part)
}

impl VideoRuntime {
    pub fn new(client: BambuClient, access_token: String, config: VideoConfig) -> Result<Self> {
        let tls = bambu_tls_connector()?;
        let (parts, _) = broadcast::channel(4);
        Ok(Self {
            inner: Arc::new(VideoInner {
                client,
                access_token,
                config,
                tls,
                parts,
                clients: AtomicUsize::new(0),
                no_clients: Notify::new(),
                worker: Mutex::new(None),
            }),
        })
    }

    pub async fn subscribe(&self) -> Result<VideoSubscription> {
        if self.video_host().is_none() {
            bail!("video stream is disabled; set --video-host to the printer IP or hostname");
        }

        let receiver = self.inner.parts.subscribe();
        self.inner.clients.fetch_add(1, Ordering::SeqCst);
        let guard = VideoClientGuard {
            inner: Arc::clone(&self.inner),
        };
        self.ensure_worker().await;

        Ok(VideoSubscription {
            receiver,
            _guard: guard,
        })
    }

    fn video_host(&self) -> Option<&str> {
        self.inner
            .config
            .host
            .as_deref()
            .map(str::trim)
            .filter(|host| !host.is_empty())
    }

    async fn ensure_worker(&self) {
        let mut worker = self.inner.worker.lock().await;
        let should_start = match worker.as_ref() {
            Some(handle) => handle.is_finished(),
            None => true,
        };
        if should_start {
            *worker = Some(tokio::spawn(run_worker(Arc::clone(&self.inner))));
        }
    }
}

impl VideoSubscription {
    pub async fn recv(&mut self) -> Result<Bytes, broadcast::error::RecvError> {
        self.receiver.recv().await
    }
}

impl Drop for VideoClientGuard {
    fn drop(&mut self) {
        if let Ok(previous) =
            self.inner
                .clients
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |clients| {
                    clients.checked_sub(1)
                })
        {
            if previous == 1 {
                self.inner.no_clients.notify_waiters();
            }
        }
    }
}

async fn run_worker(inner: Arc<VideoInner>) {
    let mut delay = RETRY_INITIAL_DELAY;
    while inner.clients.load(Ordering::SeqCst) > 0 {
        match stream_once(&inner).await {
            Ok(()) => delay = RETRY_INITIAL_DELAY,
            Err(error) => {
                if inner.clients.load(Ordering::SeqCst) == 0 {
                    break;
                }
                warn!(error = %error_chain(&error), "video stream disconnected");
                sleep_or_no_clients(&inner, delay).await;
                delay = (delay + delay / 2).min(RETRY_MAX_DELAY);
            }
        }
    }
}

async fn stream_once(inner: &VideoInner) -> Result<()> {
    let session = resolve_session(inner).await?;
    let host = inner
        .config
        .host
        .as_deref()
        .map(str::trim)
        .filter(|host| !host.is_empty())
        .context("video stream is disabled; set --video-host to the printer IP or hostname")?;
    let address = format!("{host}:{}", inner.config.port);

    let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&address))
        .await
        .with_context(|| format!("timed out connecting to video server at {address}"))?
        .with_context(|| format!("failed to connect to video server at {address}"))?;

    let server_name = ServerName::try_from(session.device_id.clone()).with_context(|| {
        format!(
            "invalid printer device id `{}` for TLS SNI",
            session.device_id
        )
    })?;
    let mut stream = inner
        .tls
        .connect(server_name, stream)
        .await
        .with_context(|| format!("failed TLS handshake with video server at {address}"))?;

    stream
        .write_all(&auth_packet(&session.access_code)?)
        .await
        .context("failed to send video authentication packet")?;
    stream
        .flush()
        .await
        .context("failed to flush video authentication packet")?;

    info!(
        device_id = %session.device_id,
        address = %address,
        "connected to printer video stream"
    );

    let mut header = [0_u8; 16];
    while inner.clients.load(Ordering::SeqCst) > 0 {
        if !read_exact_with_timeout(inner, &mut stream, &mut header, "video frame header").await? {
            break;
        }
        let frame_size = u32::from_le_bytes(header[0..4].try_into().expect("u32 slice")) as usize;
        ensure!(
            (1..=MAX_FRAME_SIZE).contains(&frame_size),
            "invalid video frame size {frame_size}"
        );

        let mut frame = vec![0_u8; frame_size];
        if !read_exact_with_timeout(inner, &mut stream, &mut frame, "video frame").await? {
            break;
        }
        if is_jpeg(&frame) {
            let _ = inner.parts.send(mjpeg_part(&frame));
        } else {
            warn!("discarding video frame without JPEG magic bytes");
        }
    }

    Ok(())
}

async fn sleep_or_no_clients(inner: &VideoInner, delay: Duration) {
    let no_clients = inner.no_clients.notified();
    tokio::pin!(no_clients);
    if inner.clients.load(Ordering::SeqCst) == 0 {
        return;
    }
    tokio::select! {
        _ = tokio::time::sleep(delay) => {}
        _ = &mut no_clients => {}
    }
}

async fn read_exact_with_timeout<S>(
    inner: &VideoInner,
    stream: &mut S,
    buffer: &mut [u8],
    label: &str,
) -> Result<bool>
where
    S: AsyncRead + Unpin,
{
    let no_clients = inner.no_clients.notified();
    tokio::pin!(no_clients);
    if inner.clients.load(Ordering::SeqCst) == 0 {
        return Ok(false);
    }

    tokio::select! {
        read = tokio::time::timeout(READ_TIMEOUT, stream.read_exact(buffer)) => {
            read
                .with_context(|| format!("timed out reading {label}"))?
                .with_context(|| format!("failed to read {label}"))?;
            Ok(true)
        }
        _ = &mut no_clients => Ok(false),
    }
}

async fn resolve_session(inner: &VideoInner) -> Result<VideoSession> {
    let current_print = inner
        .client
        .current_print(&inner.access_token)
        .await
        .context("failed to fetch video access code from Bambu Cloud")?;
    select_session(current_print.devices)
}

fn select_session(devices: Vec<CloudDevice>) -> Result<VideoSession> {
    let mut matches = devices
        .into_iter()
        .filter_map(video_session)
        .collect::<Vec<_>>();

    match matches.len() {
        0 => bail!("no devices with dev_access_code were returned by Bambu Cloud"),
        1 => Ok(matches.remove(0)),
        _ => bail!("multiple devices have video access codes; video streaming requires exactly one printer in the token account"),
    }
}

fn video_session(device: CloudDevice) -> Option<VideoSession> {
    let device_id = device.id?.trim().to_owned();
    let access_code = device.access_code?.trim().to_owned();
    if device_id.is_empty() || access_code.is_empty() {
        return None;
    }
    Some(VideoSession {
        device_id,
        access_code,
    })
}

fn auth_packet(access_code: &str) -> Result<[u8; 80]> {
    let mut packet = [0_u8; 80];
    packet[0..4].copy_from_slice(&0x40_u32.to_le_bytes());
    packet[4..8].copy_from_slice(&0x3000_u32.to_le_bytes());
    packet[8..12].copy_from_slice(&0_u32.to_le_bytes());
    packet[12..16].copy_from_slice(&0_u32.to_le_bytes());
    write_auth_field(&mut packet[16..48], "bblp", "video username")?;
    write_auth_field(&mut packet[48..80], access_code.trim(), "video access code")?;
    Ok(packet)
}

fn write_auth_field(target: &mut [u8], value: &str, label: &str) -> Result<()> {
    ensure!(value.is_ascii(), "{label} must be ASCII");
    ensure!(
        value.len() <= target.len(),
        "{label} must fit in {} bytes",
        target.len()
    );
    target[..value.len()].copy_from_slice(value.as_bytes());
    Ok(())
}

fn is_jpeg(frame: &[u8]) -> bool {
    frame.starts_with(&[0xff, 0xd8]) && frame.ends_with(&[0xff, 0xd9])
}

fn error_chain(error: &anyhow::Error) -> String {
    error
        .chain()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(": ")
}

fn bambu_tls_connector() -> Result<TlsConnector> {
    let supported_schemes = ring::default_provider()
        .signature_verification_algorithms
        .supported_schemes();
    let verifier = Arc::new(BambuCertificateVerifier { supported_schemes });
    let config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    Ok(TlsConnector::from(Arc::new(config)))
}

struct BambuCertificateVerifier {
    supported_schemes: Vec<SignatureScheme>,
}

impl fmt::Debug for BambuCertificateVerifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BambuCertificateVerifier")
            .field("supported_schemes", &self.supported_schemes)
            .finish_non_exhaustive()
    }
}

impl ServerCertVerifier for BambuCertificateVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, RustlsError> {
        // Bambu printer certificates are not consistently accepted by rustls/WebPKI:
        // they are CN-only, and at least some firmware serves certificates with a
        // version shape WebPKI rejects as UnsupportedCertVersion. Still send the
        // printer serial as SNI so the printer selects the expected certificate, but
        // do not rely on WebPKI parsing for this local-only video transport.
        let _ = (end_entity, intermediates, now);
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        let _ = (message, cert, dss);
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        let _ = (message, cert, dss);
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.supported_schemes.clone()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{auth_packet, bambu_tls_connector, is_jpeg, mjpeg_part, select_session};
    use crate::bambu::CloudDevice;

    fn device(value: serde_json::Value) -> CloudDevice {
        serde_json::from_value(value).expect("device should deserialize")
    }

    #[test]
    fn auth_packet_matches_a1_p1_protocol_layout() {
        let packet = auth_packet("12345678").expect("access code should fit");

        assert_eq!(&packet[0..4], &0x40_u32.to_le_bytes());
        assert_eq!(&packet[4..8], &0x3000_u32.to_le_bytes());
        assert_eq!(&packet[8..12], &0_u32.to_le_bytes());
        assert_eq!(&packet[12..16], &0_u32.to_le_bytes());
        assert_eq!(&packet[16..20], b"bblp");
        assert!(packet[20..48].iter().all(|byte| *byte == 0));
        assert_eq!(&packet[48..56], b"12345678");
        assert!(packet[56..80].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn auth_packet_rejects_fields_that_do_not_fit() {
        let error = auth_packet("123456789012345678901234567890123").unwrap_err();
        assert!(error.to_string().contains("video access code"));
    }

    #[test]
    fn selected_session_uses_real_cloud_field_names() {
        let session = select_session(vec![device(json!({
            "dev_id": "printer-a",
            "dev_access_code": "12345678\n"
        }))])
        .expect("single device should be selected");

        assert_eq!(session.device_id, "printer-a");
        assert_eq!(session.access_code, "12345678");
    }

    #[test]
    fn selected_session_rejects_multiple_printers_with_video() {
        let error = select_session(vec![
            device(json!({"dev_id": "printer-a", "dev_access_code": "11111111"})),
            device(json!({"dev_id": "printer-b", "dev_access_code": "22222222"})),
        ])
        .unwrap_err();

        assert!(error.to_string().contains("exactly one printer"));
    }

    #[test]
    fn mjpeg_part_contains_boundary_headers_and_frame() {
        let part = mjpeg_part(&[0xff, 0xd8, 0xff, 0xd9]);

        assert!(
            part.starts_with(b"--frame\r\nContent-Type: image/jpeg\r\nContent-Length: 4\r\n\r\n")
        );
        assert!(part.ends_with(&[0xff, 0xd8, 0xff, 0xd9, b'\r', b'\n']));
    }

    #[test]
    fn jpeg_check_requires_soi_and_eoi_markers() {
        assert!(is_jpeg(&[0xff, 0xd8, 0x00, 0xff, 0xd9]));
        assert!(!is_jpeg(&[0xff, 0xd8, 0x00]));
        assert!(!is_jpeg(&[0x00, 0xff, 0xd9]));
    }

    #[test]
    fn bambu_tls_connector_builds_with_supported_signature_schemes() {
        bambu_tls_connector().expect("Bambu TLS connector should build");
    }
}
