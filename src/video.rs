use std::{
    collections::HashMap,
    str,
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
use tokio_native_tls::TlsConnector;
use tracing::{info, warn};

use crate::{
    bambu::{BambuClient, CloudDevice},
    device_tls,
    devices::DeviceRegistry,
};

pub const DEFAULT_VIDEO_PORT: u16 = 6000;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(15);
const RETRY_INITIAL_DELAY: Duration = Duration::from_secs(1);
const RETRY_MAX_DELAY: Duration = Duration::from_secs(30);
const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;
const MJPEG_BOUNDARY: &str = "frame";

#[derive(Clone, Debug)]
pub struct VideoConfig {
    pub hosts: Vec<String>,
    pub port: u16,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            hosts: Vec::new(),
            port: DEFAULT_VIDEO_PORT,
        }
    }
}

#[derive(Clone)]
pub struct VideoRuntime {
    inner: Arc<VideoRuntimeInner>,
}

struct VideoRuntimeInner {
    client: BambuClient,
    access_token: String,
    config: VideoConfig,
    tls: TlsConnector,
    devices: DeviceRegistry,
    streams: Mutex<HashMap<String, Arc<VideoStream>>>,
    host_map: Mutex<HashMap<String, String>>,
}

struct VideoStream {
    device_id: String,
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
    stream: Arc<VideoStream>,
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
    pub fn new(
        client: BambuClient,
        access_token: String,
        config: VideoConfig,
        devices: DeviceRegistry,
    ) -> Result<Self> {
        let tls = device_tls::tokio_connector()?;
        Ok(Self {
            inner: Arc::new(VideoRuntimeInner {
                client,
                access_token,
                config,
                tls,
                devices,
                streams: Mutex::new(HashMap::new()),
                host_map: Mutex::new(HashMap::new()),
            }),
        })
    }

    pub async fn subscribe(&self, device_id: Option<&str>) -> Result<VideoSubscription> {
        if video_hosts(&self.inner.config).is_empty() {
            bail!("video stream is disabled; set at least one --video-host");
        }

        let session = resolve_session(&self.inner, device_id).await?;
        let stream = self.stream_for_device(&session.device_id).await;
        let receiver = stream.parts.subscribe();
        stream.clients.fetch_add(1, Ordering::SeqCst);
        let guard = VideoClientGuard {
            stream: Arc::clone(&stream),
        };
        self.ensure_worker(stream).await;

        Ok(VideoSubscription {
            receiver,
            _guard: guard,
        })
    }

    async fn stream_for_device(&self, device_id: &str) -> Arc<VideoStream> {
        let mut streams = self.inner.streams.lock().await;
        if let Some(stream) = streams.get(device_id) {
            return Arc::clone(stream);
        }

        let (parts, _) = broadcast::channel(4);
        let stream = Arc::new(VideoStream {
            device_id: device_id.to_owned(),
            parts,
            clients: AtomicUsize::new(0),
            no_clients: Notify::new(),
            worker: Mutex::new(None),
        });
        streams.insert(device_id.to_owned(), Arc::clone(&stream));
        stream
    }

    async fn ensure_worker(&self, stream: Arc<VideoStream>) {
        let mut worker = stream.worker.lock().await;
        let should_start = match worker.as_ref() {
            Some(handle) => handle.is_finished(),
            None => true,
        };
        if should_start {
            *worker = Some(tokio::spawn(run_worker(
                Arc::clone(&self.inner),
                Arc::clone(&stream),
            )));
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
            self.stream
                .clients
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |clients| {
                    clients.checked_sub(1)
                })
        {
            if previous == 1 {
                self.stream.no_clients.notify_waiters();
            }
        }
    }
}

async fn run_worker(inner: Arc<VideoRuntimeInner>, stream: Arc<VideoStream>) {
    let mut delay = RETRY_INITIAL_DELAY;
    while stream.clients.load(Ordering::SeqCst) > 0 {
        match stream_once(&inner, &stream).await {
            Ok(()) => delay = RETRY_INITIAL_DELAY,
            Err(error) => {
                if stream.clients.load(Ordering::SeqCst) == 0 {
                    break;
                }
                warn!(
                    device_id = %stream.device_id,
                    error = %error_chain(&error),
                    "video stream disconnected"
                );
                sleep_or_no_clients(&stream, delay).await;
                delay = (delay + delay / 2).min(RETRY_MAX_DELAY);
            }
        }
    }
}

async fn stream_once(inner: &VideoRuntimeInner, stream: &VideoStream) -> Result<()> {
    let session = resolve_session(inner, Some(&stream.device_id)).await?;
    let hosts = candidate_hosts(inner, &session.device_id).await;
    let mut last_error = None;

    for host in hosts {
        match stream_host_once(inner, stream, &session, &host).await {
            Ok(()) => return Ok(()),
            Err(_) if stream.clients.load(Ordering::SeqCst) == 0 => return Ok(()),
            Err(error) => {
                warn!(
                    device_id = %session.device_id,
                    host = %host,
                    error = %error_chain(&error),
                    "video host failed"
                );
                last_error = Some(error);
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("no video hosts configured")))
}

async fn stream_host_once(
    inner: &VideoRuntimeInner,
    video: &VideoStream,
    session: &VideoSession,
    host: &str,
) -> Result<()> {
    let address = format!("{host}:{}", inner.config.port);

    let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&address))
        .await
        .with_context(|| format!("timed out connecting to video server at {address}"))?
        .with_context(|| format!("failed to connect to video server at {address}"))?;

    let mut socket = inner
        .tls
        .connect(&session.device_id, tcp)
        .await
        .with_context(|| format!("failed TLS handshake with video server at {address}"))?;
    let certificate_device_id = device_tls::peer_device_id(&socket)
        .context("video server certificate did not include a usable common name")?;
    if certificate_device_id != session.device_id {
        remember_host(inner, &certificate_device_id, host).await;
        bail!(
            "video host certificate is for device `{certificate_device_id}`, not requested device `{}`",
            session.device_id
        );
    }

    socket
        .write_all(&auth_packet(&session.access_code)?)
        .await
        .context("failed to send video authentication packet")?;
    socket
        .flush()
        .await
        .context("failed to flush video authentication packet")?;

    info!(
        device_id = %session.device_id,
        address = %address,
        "connected to printer video stream"
    );

    let mut header = [0_u8; 16];
    while video.clients.load(Ordering::SeqCst) > 0 {
        if !read_exact_with_timeout(video, &mut socket, &mut header, "video frame header").await? {
            break;
        }
        let frame_size = u32::from_le_bytes(header[0..4].try_into().expect("u32 slice")) as usize;
        ensure!(
            (1..=MAX_FRAME_SIZE).contains(&frame_size),
            "invalid video frame size {frame_size}"
        );

        let mut frame = vec![0_u8; frame_size];
        if !read_exact_with_timeout(video, &mut socket, &mut frame, "video frame").await? {
            break;
        }
        if is_jpeg(&frame) {
            remember_host(inner, &session.device_id, host).await;
            let _ = video.parts.send(mjpeg_part(&frame));
        } else {
            warn!("discarding video frame without JPEG magic bytes");
        }
    }

    Ok(())
}

async fn sleep_or_no_clients(stream: &VideoStream, delay: Duration) {
    let no_clients = stream.no_clients.notified();
    tokio::pin!(no_clients);
    if stream.clients.load(Ordering::SeqCst) == 0 {
        return;
    }
    tokio::select! {
        _ = tokio::time::sleep(delay) => {}
        _ = &mut no_clients => {}
    }
}

async fn read_exact_with_timeout<S>(
    video: &VideoStream,
    stream: &mut S,
    buffer: &mut [u8],
    label: &str,
) -> Result<bool>
where
    S: AsyncRead + Unpin,
{
    let no_clients = video.no_clients.notified();
    tokio::pin!(no_clients);
    if video.clients.load(Ordering::SeqCst) == 0 {
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

async fn resolve_session(
    inner: &VideoRuntimeInner,
    requested_device_id: Option<&str>,
) -> Result<VideoSession> {
    let mut current_print = inner
        .client
        .current_print(&inner.access_token)
        .await
        .context("failed to fetch video access code from Bambu Cloud")?;
    current_print.devices = inner
        .devices
        .order_cloud_devices(current_print.devices)
        .await;
    select_session(current_print.devices, requested_device_id)
}

fn select_session(
    devices: Vec<CloudDevice>,
    requested_device_id: Option<&str>,
) -> Result<VideoSession> {
    let requested_device_id = requested_device_id
        .map(str::trim)
        .filter(|device_id| !device_id.is_empty());

    if let Some(requested_device_id) = requested_device_id {
        let Some(device) = devices.into_iter().find(|device| {
            device
                .id
                .as_deref()
                .map(str::trim)
                .is_some_and(|device_id| device_id == requested_device_id)
        }) else {
            bail!("device `{requested_device_id}` was not returned by Bambu Cloud");
        };
        return video_session(device).with_context(|| {
            format!("device `{requested_device_id}` did not include dev_access_code")
        });
    }

    let Some(device) = devices.into_iter().next() else {
        bail!("no devices were returned by Bambu Cloud");
    };
    video_session(device).context("first device did not include dev_access_code")
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

fn video_hosts(config: &VideoConfig) -> Vec<String> {
    config
        .hosts
        .iter()
        .map(|host| host.trim())
        .filter(|host| !host.is_empty())
        .map(str::to_owned)
        .collect()
}

async fn candidate_hosts(inner: &VideoRuntimeInner, device_id: &str) -> Vec<String> {
    let hosts = video_hosts(&inner.config);
    let remembered = inner.host_map.lock().await.get(device_id).cloned();

    order_hosts(hosts, remembered)
}

fn order_hosts(hosts: Vec<String>, remembered: Option<String>) -> Vec<String> {
    let Some(remembered) =
        remembered.filter(|host| hosts.iter().any(|candidate| candidate == host))
    else {
        return hosts;
    };

    let mut ordered = Vec::with_capacity(hosts.len());
    ordered.push(remembered.clone());
    ordered.extend(hosts.into_iter().filter(|host| host != &remembered));
    ordered
}

async fn remember_host(inner: &VideoRuntimeInner, device_id: &str, host: &str) {
    inner
        .host_map
        .lock()
        .await
        .insert(device_id.to_owned(), host.to_owned());
}

fn error_chain(error: &anyhow::Error) -> String {
    error
        .chain()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(": ")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        auth_packet, is_jpeg, mjpeg_part, order_hosts, select_session, video_hosts, VideoConfig,
    };
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
        let session = select_session(
            vec![device(json!({
                "dev_id": "printer-a",
                "dev_access_code": "12345678\n"
            }))],
            None,
        )
        .expect("single device should be selected");

        assert_eq!(session.device_id, "printer-a");
        assert_eq!(session.access_code, "12345678");
    }

    #[test]
    fn selected_session_uses_first_stable_device_by_default() {
        let session = select_session(
            vec![
                device(json!({"dev_id": "printer-a", "dev_access_code": "11111111"})),
                device(json!({"dev_id": "printer-b", "dev_access_code": "22222222"})),
            ],
            None,
        )
        .expect("first device should be selected");

        assert_eq!(session.device_id, "printer-a");
        assert_eq!(session.access_code, "11111111");
    }

    #[test]
    fn selected_session_can_match_requested_device_id() {
        let session = select_session(
            vec![
                device(json!({"dev_id": "printer-a", "dev_access_code": "11111111"})),
                device(json!({"dev_id": "printer-b", "dev_access_code": "22222222"})),
            ],
            Some("printer-b"),
        )
        .expect("requested device should be selected");

        assert_eq!(session.device_id, "printer-b");
        assert_eq!(session.access_code, "22222222");
    }

    #[test]
    fn selected_session_rejects_unknown_requested_device_id() {
        let error = select_session(
            vec![device(
                json!({"dev_id": "printer-a", "dev_access_code": "11111111"}),
            )],
            Some("printer-b"),
        )
        .unwrap_err();

        assert!(error.to_string().contains("printer-b"));
    }

    #[test]
    fn video_hosts_trim_empty_entries() {
        let hosts = video_hosts(&VideoConfig {
            hosts: vec![
                " 192.168.1.50 ".to_owned(),
                String::new(),
                "  ".to_owned(),
                "printer.local".to_owned(),
            ],
            port: 6000,
        });

        assert_eq!(hosts, ["192.168.1.50", "printer.local"]);
    }

    #[test]
    fn remembered_video_host_is_tried_first() {
        let hosts = order_hosts(
            vec![
                "192.168.1.50".to_owned(),
                "192.168.1.51".to_owned(),
                "192.168.1.52".to_owned(),
            ],
            Some("192.168.1.51".to_owned()),
        );

        assert_eq!(hosts, ["192.168.1.51", "192.168.1.50", "192.168.1.52"]);
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
}
