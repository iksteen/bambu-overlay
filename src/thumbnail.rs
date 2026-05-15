use std::{
    collections::HashMap,
    io::{Cursor, Read},
    net::{SocketAddr, TcpStream, ToSocketAddrs},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, ensure, Context, Result};
use bytes::Bytes;
use flate2::read::DeflateDecoder;
use suppaftp::{types::FileType, Mode, NativeTlsConnector, NativeTlsFtpStream};
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, warn};

use crate::{
    bambu::{PrinterStatus, Task},
    cloud::CloudSession,
    device_tls,
    devices::{DeviceSource, KnownDevice},
    local::LocalDevice,
    mqtt::MqttRuntime,
};

const LOCAL_FTPS_PORT: u16 = 990;
const FTP_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_3MF_SIZE: usize = 512 * 1024 * 1024;
const MAX_THUMBNAIL_SIZE: usize = 32 * 1024 * 1024;
const CLOUD_TASK_LIMIT: usize = 10;
const MISSING_RETRY_DELAY: Duration = Duration::from_secs(30);
const ZIP_LOCAL_FILE_HEADER: u32 = 0x0403_4b50;
const ZIP_CENTRAL_DIRECTORY_HEADER: u32 = 0x0201_4b50;
const ZIP_END_OF_CENTRAL_DIRECTORY: u32 = 0x0605_4b50;

#[derive(Clone)]
pub(crate) struct ThumbnailRuntime {
    inner: Arc<ThumbnailInner>,
}

struct ThumbnailInner {
    mqtt: MqttRuntime,
    cloud: Option<CloudSession>,
    devices: Vec<KnownDevice>,
    local_devices: HashMap<String, LocalDevice>,
    cache: RwLock<HashMap<String, ThumbnailEntry>>,
    fetch_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

#[derive(Debug, Clone)]
pub(crate) struct ThumbnailImage {
    pub(crate) content_type: String,
    pub(crate) bytes: Bytes,
}

#[derive(Debug, Clone)]
struct ThumbnailEntry {
    task: TaskKey,
    request_task: Option<String>,
    result: ThumbnailResult,
    retry_after: Option<Instant>,
}

#[derive(Debug, Clone)]
enum ThumbnailResult {
    Ready(ThumbnailImage),
    Missing(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TaskKey(String);

impl ThumbnailRuntime {
    pub(crate) fn new(
        mqtt: MqttRuntime,
        cloud: Option<CloudSession>,
        devices: Vec<KnownDevice>,
        local_devices: Vec<LocalDevice>,
    ) -> Self {
        let local_devices = local_devices
            .into_iter()
            .map(|device| (device.id.clone(), device))
            .collect();
        Self {
            inner: Arc::new(ThumbnailInner {
                mqtt,
                cloud,
                devices,
                local_devices,
                cache: RwLock::new(HashMap::new()),
                fetch_locks: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub(crate) fn start(&self) {
        let runtime = self.clone();
        tokio::spawn(async move { runtime.watch_task_changes().await });
    }

    pub(crate) async fn thumbnail(
        &self,
        requested_device_id: Option<&str>,
        requested_task: Option<&str>,
    ) -> Result<Option<ThumbnailImage>> {
        let Some(device_id) = self.select_device_id(requested_device_id).await? else {
            return Ok(None);
        };

        self.refresh_device(&device_id, normalized_request_task(requested_task))
            .await?;
        Ok(self.cached_image(&device_id).await)
    }

    async fn watch_task_changes(&self) {
        let mut changes = self.inner.mqtt.subscribe();
        self.refresh_changed_devices().await;
        loop {
            if changes.recv().await.is_err() {
                changes = self.inner.mqtt.subscribe();
            }
            self.refresh_changed_devices().await;
        }
    }

    async fn refresh_changed_devices(&self) {
        let reports = self.inner.mqtt.reports().await;
        for device in &self.inner.devices {
            let Some(device_id) = device.id.as_deref() else {
                continue;
            };
            let Some(report) = reports.get(device_id) else {
                self.clear_device(device_id).await;
                continue;
            };
            let Some(task) = TaskKey::from_report(report) else {
                self.clear_device(device_id).await;
                continue;
            };
            if self.cache_matches(device_id, &task, None).await {
                continue;
            }
            if let Err(error) = self.fetch_and_cache(device_id, report, task, None).await {
                warn!(
                    device_id,
                    error = %error_chain(&error),
                    "failed to refresh print thumbnail"
                );
            }
        }
    }

    async fn refresh_device(&self, device_id: &str, request_task: Option<String>) -> Result<()> {
        let reports = self.inner.mqtt.reports().await;
        let Some(report) = reports.get(device_id) else {
            self.clear_device(device_id).await;
            return Ok(());
        };
        let Some(task) = TaskKey::from_report(report) else {
            self.clear_device(device_id).await;
            return Ok(());
        };
        if self
            .cache_matches(device_id, &task, request_task.as_deref())
            .await
        {
            return Ok(());
        }
        self.fetch_and_cache(device_id, report, task, request_task)
            .await
    }

    async fn fetch_and_cache(
        &self,
        device_id: &str,
        report: &PrinterStatus,
        task: TaskKey,
        request_task: Option<String>,
    ) -> Result<()> {
        let fetch_lock = self.fetch_lock(device_id).await;
        let _guard = fetch_lock.lock().await;

        if self
            .cache_matches(device_id, &task, request_task.as_deref())
            .await
        {
            return Ok(());
        }

        let (result, retry_after) = match self.fetch_thumbnail(device_id, report).await {
            Ok(image) => {
                debug!(device_id, "cached print thumbnail");
                (ThumbnailResult::Ready(image), None)
            }
            Err(error) => {
                let message = error_chain(&error);
                warn!(
                    device_id,
                    error = %message,
                    "print thumbnail is unavailable"
                );
                (
                    ThumbnailResult::Missing(message),
                    Some(Instant::now() + MISSING_RETRY_DELAY),
                )
            }
        };

        self.inner.cache.write().await.insert(
            device_id.to_owned(),
            ThumbnailEntry {
                task,
                request_task,
                result,
                retry_after,
            },
        );
        Ok(())
    }

    async fn fetch_thumbnail(
        &self,
        device_id: &str,
        report: &PrinterStatus,
    ) -> Result<ThumbnailImage> {
        let device = self
            .inner
            .devices
            .iter()
            .find(|device| device.id.as_deref() == Some(device_id))
            .with_context(|| format!("device `{device_id}` is not known"))?;

        match device.source {
            DeviceSource::Cloud => self.fetch_cloud_thumbnail(device_id, report).await,
            DeviceSource::Local => self.fetch_local_thumbnail(device_id, report).await,
        }
    }

    async fn fetch_cloud_thumbnail(
        &self,
        device_id: &str,
        report: &PrinterStatus,
    ) -> Result<ThumbnailImage> {
        let cloud = self
            .inner
            .cloud
            .as_ref()
            .context("cloud thumbnail lookup requires a Bambu Cloud token")?;
        let tasks = cloud
            .client
            .tasks(&cloud.access_token, CLOUD_TASK_LIMIT, Some(device_id))
            .await
            .with_context(|| {
                format!("failed to load Bambu Cloud tasks for device `{device_id}`")
            })?;
        let task = select_cloud_task(&tasks.hits, report).with_context(|| {
            format!("no matching Bambu Cloud task found for device `{device_id}`")
        })?;
        let cover = task
            .cover
            .as_deref()
            .map(str::trim)
            .filter(|cover| !cover.is_empty())
            .context("matching Bambu Cloud task does not include a thumbnail URL")?;
        let downloaded = cloud
            .client
            .download_bytes(cover, MAX_THUMBNAIL_SIZE)
            .await
            .with_context(|| format!("failed to download Bambu Cloud thumbnail `{cover}`"))?;

        Ok(ThumbnailImage {
            content_type: image_content_type(
                downloaded.content_type.as_deref(),
                downloaded.bytes.as_ref(),
            ),
            bytes: downloaded.bytes,
        })
    }

    async fn fetch_local_thumbnail(
        &self,
        device_id: &str,
        report: &PrinterStatus,
    ) -> Result<ThumbnailImage> {
        let local = self
            .inner
            .local_devices
            .get(device_id)
            .with_context(|| format!("device `{device_id}` does not have a local endpoint"))?;
        let filename = report
            .filename
            .as_deref()
            .map(str::trim)
            .filter(|filename| !filename.is_empty())
            .context("MQTT report does not include gcode_file for local thumbnail lookup")?;
        fetch_local_3mf_thumbnail(local, filename, report.print_type.as_deref())
            .await
            .with_context(|| {
                format!("failed to fetch thumbnail from `{filename}` on local device `{device_id}`")
            })
    }

    async fn select_device_id(&self, requested_device_id: Option<&str>) -> Result<Option<String>> {
        let requested_device_id = requested_device_id
            .map(str::trim)
            .filter(|device_id| !device_id.is_empty());
        if let Some(device_id) = requested_device_id {
            ensure!(
                self.inner
                    .devices
                    .iter()
                    .any(|device| device.id.as_deref() == Some(device_id)),
                "device `{device_id}` is not known"
            );
            return Ok(Some(device_id.to_owned()));
        }

        Ok(self
            .inner
            .devices
            .iter()
            .find_map(|device| device.id.clone()))
    }

    async fn cached_image(&self, device_id: &str) -> Option<ThumbnailImage> {
        let cache = self.inner.cache.read().await;
        match cache.get(device_id).map(|entry| &entry.result) {
            Some(ThumbnailResult::Ready(image)) => Some(image.clone()),
            Some(ThumbnailResult::Missing(error)) => {
                debug!(device_id, error, "thumbnail is unavailable");
                None
            }
            None => None,
        }
    }

    async fn cache_matches(
        &self,
        device_id: &str,
        task: &TaskKey,
        request_task: Option<&str>,
    ) -> bool {
        let cache = self.inner.cache.read().await;
        let Some(entry) = cache.get(device_id) else {
            return false;
        };
        if entry.task != *task {
            return false;
        }
        match entry.result {
            ThumbnailResult::Ready(_) => true,
            ThumbnailResult::Missing(_) => {
                if let Some(request_task) = request_task {
                    if entry.request_task.as_deref() != Some(request_task) {
                        return false;
                    }
                }
                entry
                    .retry_after
                    .is_some_and(|retry_after| retry_after > Instant::now())
            }
        }
    }

    async fn clear_device(&self, device_id: &str) {
        self.inner.cache.write().await.remove(device_id);
    }

    async fn fetch_lock(&self, device_id: &str) -> Arc<Mutex<()>> {
        let mut locks = self.inner.fetch_locks.lock().await;
        locks
            .entry(device_id.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }
}

impl TaskKey {
    fn from_report(report: &PrinterStatus) -> Option<Self> {
        let task_id = trimmed(report.task_id.as_deref());
        let filename = trimmed(report.filename.as_deref());
        let task_name = trimmed(report.task_name.as_deref());
        if task_id.is_none() && filename.is_none() && task_name.is_none() {
            return None;
        }

        Some(Self(format!(
            "{}\u{0}{}\u{0}{}\u{0}{}\u{0}{}",
            task_id.unwrap_or_default(),
            filename.unwrap_or_default(),
            task_name.unwrap_or_default(),
            trimmed(report.start_time.as_deref()).unwrap_or_default(),
            trimmed(report.print_type.as_deref()).unwrap_or_default()
        )))
    }
}

fn select_cloud_task<'a>(tasks: &'a [Task], report: &PrinterStatus) -> Option<&'a Task> {
    let task_id = trimmed(report.task_id.as_deref());
    if let Some(task_id) = task_id {
        if let Some(task) = tasks
            .iter()
            .find(|task| trimmed(task.id.as_deref()) == Some(task_id))
        {
            return Some(task);
        }
    }

    let task_name = trimmed(report.task_name.as_deref());
    if let Some(task_name) = task_name {
        if let Some(task) = tasks.iter().find(|task| {
            task.display_title()
                .as_deref()
                .map(str::trim)
                .filter(|title| !title.is_empty())
                .is_some_and(|title| title == task_name)
        }) {
            return Some(task);
        }
    }

    let start_time = trimmed(report.start_time.as_deref());
    if let Some(start_time) = start_time {
        if let Some(task) = tasks
            .iter()
            .find(|task| trimmed(task.start_time.as_deref()) == Some(start_time))
        {
            return Some(task);
        }
    }

    None
}

async fn fetch_local_3mf_thumbnail(
    device: &LocalDevice,
    filename: &str,
    print_type: Option<&str>,
) -> Result<ThumbnailImage> {
    let device = device.clone();
    let filename = filename.to_owned();
    let print_type = print_type.map(str::to_owned);
    tokio::task::spawn_blocking(move || {
        fetch_local_3mf_thumbnail_blocking(&device, &filename, print_type.as_deref())
    })
    .await
    .context("local FTPS thumbnail task failed")?
}

fn fetch_local_3mf_thumbnail_blocking(
    device: &LocalDevice,
    filename: &str,
    print_type: Option<&str>,
) -> Result<ThumbnailImage> {
    let candidates = local_file_candidates(filename, print_type);
    if candidates.is_empty() {
        bail!("no local file candidates were generated");
    }

    // suppaftp exposes the post-handshake TCP stream, but not the peer certificate.
    // Probe before login so the access code is only sent to the expected device.
    verify_local_ftps_device_id(device)?;
    fetch_local_3mf_thumbnail_with_mode(device, &candidates, Mode::Passive)
}

fn fetch_local_3mf_thumbnail_with_mode(
    device: &LocalDevice,
    candidates: &[String],
    mode: Mode,
) -> Result<ThumbnailImage> {
    let mut last_error = None;
    for path in candidates {
        match retrieve_thumbnail_from_candidate(device, mode, path) {
            Ok(image) => return Ok(image),
            Err(error) => {
                last_error = Some(error);
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("no local file candidates were generated")))
}

fn retrieve_thumbnail_from_candidate(
    device: &LocalDevice,
    mode: Mode,
    path: &str,
) -> Result<ThumbnailImage> {
    let mut client = connect_local_ftps(device, mode)?;
    retrieve_thumbnail(&mut client, path)
}

fn connect_local_ftps(device: &LocalDevice, mode: Mode) -> Result<NativeTlsFtpStream> {
    let address = local_ftps_address(device);
    let connector = NativeTlsConnector::from(device_tls::native_connector()?);
    let mut client = NativeTlsFtpStream::connect_secure_implicit(
        address.as_str(),
        connector,
        device.endpoint.host(),
    )
    .with_context(|| format!("failed to connect to local FTPS at {address}"))?;
    client
        .get_ref()
        .set_read_timeout(Some(FTP_TIMEOUT))
        .context("failed to set local FTPS read timeout")?;
    client
        .get_ref()
        .set_write_timeout(Some(FTP_TIMEOUT))
        .context("failed to set local FTPS write timeout")?;
    client.set_passive_nat_workaround(true);
    client.set_mode(mode);
    client
        .login("bblp", device.endpoint.access_code())
        .context("local FTPS login failed")?;
    client
        .transfer_type(FileType::Binary)
        .context("failed to set local FTPS binary transfer mode")?;
    Ok(client)
}

fn verify_local_ftps_device_id(device: &LocalDevice) -> Result<()> {
    let address = local_ftps_address(device);
    let address = resolve_socket_addr(&address)?;
    let tcp = TcpStream::connect_timeout(&address, FTP_TIMEOUT)
        .with_context(|| format!("failed to connect to local FTPS at {address}"))?;
    tcp.set_read_timeout(Some(FTP_TIMEOUT))
        .context("failed to set local FTPS preflight read timeout")?;
    tcp.set_write_timeout(Some(FTP_TIMEOUT))
        .context("failed to set local FTPS preflight write timeout")?;
    let socket = device_tls::native_connector()?
        .connect(device.endpoint.host(), tcp)
        .with_context(|| format!("failed local FTPS TLS handshake at {address}"))?;
    let certificate = socket
        .peer_certificate()
        .context("failed to read local FTPS certificate")?
        .context("local FTPS did not send a certificate")?;
    let certificate_device_id = device_tls::certificate_device_id(&certificate)
        .context("local FTPS certificate did not include a device ID")?;
    ensure!(
        certificate_device_id == device.id,
        "local FTPS certificate is for device `{certificate_device_id}`, not `{}`",
        device.id
    );
    Ok(())
}

fn retrieve_thumbnail(client: &mut NativeTlsFtpStream, path: &str) -> Result<ThumbnailImage> {
    let mut stream = client
        .retr_as_stream(path)
        .with_context(|| format!("local FTPS RETR `{path}` failed"))?;
    // Intentionally stop reading once the thumbnail entry has been parsed. Finalizing
    // the RETR stream waits for the rest of the 3MF payload and defeats incremental use.
    let image = extract_bambu_3mf_thumbnail_stream(&mut stream)
        .with_context(|| format!("failed to stream thumbnail from local 3MF `{path}`"))?;
    drop(stream);
    Ok(image)
}

fn local_ftps_address(device: &LocalDevice) -> String {
    if device.endpoint.host().contains(':') {
        format!("[{}]:{LOCAL_FTPS_PORT}", device.endpoint.host())
    } else {
        format!("{}:{LOCAL_FTPS_PORT}", device.endpoint.host())
    }
}

fn resolve_socket_addr(address: &str) -> Result<SocketAddr> {
    address
        .to_socket_addrs()
        .with_context(|| format!("failed to resolve local FTPS address `{address}`"))?
        .next()
        .with_context(|| format!("local FTPS address `{address}` did not resolve"))
}

// Streaming extraction intentionally supports the ZIP subset emitted by Bambu 3MF files.
// It reads local file headers in order and stops as soon as a supported thumbnail is found.
fn extract_bambu_3mf_thumbnail_stream(reader: &mut dyn Read) -> Result<ThumbnailImage> {
    let mut scanned = 0_usize;
    loop {
        let signature = read_u32_le(reader).context("failed to read zip entry signature")?;
        scanned += 4;
        match signature {
            ZIP_LOCAL_FILE_HEADER => {
                let entry = read_zip_entry_header(reader)?;
                scanned = checked_add(scanned, 26 + entry.name.len() + entry.extra_len)?;
                if is_supported_thumbnail_entry(&entry.name) {
                    let bytes = read_zip_entry_data(reader, &entry).with_context(|| {
                        format!("failed to read thumbnail entry `{}`", entry.name)
                    })?;
                    debug!(
                        entry = %entry.name,
                        scanned,
                        size = bytes.len(),
                        "streamed thumbnail from local 3MF"
                    );
                    return Ok(ThumbnailImage {
                        content_type: image_content_type(path_content_type(&entry.name), &bytes),
                        bytes: Bytes::from(bytes),
                    });
                }
                skip_exact(reader, entry.compressed_size)
                    .with_context(|| format!("failed to skip zip entry `{}`", entry.name))?;
                scanned = checked_add(scanned, entry.compressed_size)?;
            }
            ZIP_CENTRAL_DIRECTORY_HEADER | ZIP_END_OF_CENTRAL_DIRECTORY => {
                bail!(
                    "3MF did not include a supported thumbnail image before the central directory"
                )
            }
            other => bail!("unexpected zip entry signature 0x{other:08x}"),
        }
        ensure!(
            scanned <= MAX_3MF_SIZE,
            "local 3MF exceeds maximum supported scan size of {MAX_3MF_SIZE} bytes"
        );
    }
}

#[derive(Debug)]
struct ZipEntryHeader {
    name: String,
    compression: u16,
    flags: u16,
    compressed_size: usize,
    extra_len: usize,
}

fn read_zip_entry_header(reader: &mut dyn Read) -> Result<ZipEntryHeader> {
    let mut fixed = [0_u8; 26];
    reader
        .read_exact(&mut fixed)
        .context("failed to read zip local file header")?;
    let flags = u16::from_le_bytes([fixed[2], fixed[3]]);
    let compression = u16::from_le_bytes([fixed[4], fixed[5]]);
    let compressed_size = u32::from_le_bytes([fixed[14], fixed[15], fixed[16], fixed[17]]);
    let uncompressed_size = u32::from_le_bytes([fixed[18], fixed[19], fixed[20], fixed[21]]);
    let name_len = u16::from_le_bytes([fixed[22], fixed[23]]) as usize;
    let extra_len = u16::from_le_bytes([fixed[24], fixed[25]]) as usize;
    let name = read_exact_vec(reader, name_len, "zip entry name")?;
    let extra = read_exact_vec(reader, extra_len, "zip entry extra fields")?;
    let mut compressed_size = compressed_size as u64;
    if compressed_size == u32::MAX as u64 || uncompressed_size == u32::MAX {
        if let Some(zip64_compressed_size) = zip64_compressed_size(
            &extra,
            uncompressed_size == u32::MAX,
            compressed_size == u32::MAX as u64,
        )? {
            compressed_size = zip64_compressed_size;
        }
    }
    ensure!(
        flags & 0x0008 == 0,
        "zip entry uses a data descriptor before a thumbnail was found"
    );
    ensure!(
        compressed_size <= MAX_3MF_SIZE as u64,
        "zip entry exceeds maximum supported size of {MAX_3MF_SIZE} bytes"
    );
    let name = String::from_utf8_lossy(&name).replace('\\', "/");
    Ok(ZipEntryHeader {
        name,
        compression,
        flags,
        compressed_size: compressed_size as usize,
        extra_len,
    })
}

fn read_zip_entry_data(reader: &mut dyn Read, entry: &ZipEntryHeader) -> Result<Vec<u8>> {
    ensure!(entry.flags & 0x0001 == 0, "zip entry is encrypted");
    ensure!(
        entry.compressed_size <= MAX_THUMBNAIL_SIZE,
        "thumbnail entry exceeds maximum supported size of {MAX_THUMBNAIL_SIZE} bytes"
    );
    let data = read_exact_vec(reader, entry.compressed_size, "thumbnail entry data")?;
    let bytes = match entry.compression {
        0 => data,
        8 => {
            let mut decoder = DeflateDecoder::new(Cursor::new(data));
            read_limited(&mut decoder, MAX_THUMBNAIL_SIZE, "deflated thumbnail entry")?
        }
        other => bail!("thumbnail entry uses unsupported zip compression method {other}"),
    };
    ensure!(
        !bytes.is_empty(),
        "thumbnail entry `{}` is empty",
        entry.name
    );
    Ok(bytes)
}

fn read_exact_vec(reader: &mut dyn Read, len: usize, label: &str) -> Result<Vec<u8>> {
    let mut bytes = vec![0_u8; len];
    reader
        .read_exact(&mut bytes)
        .with_context(|| format!("failed to read {label}"))?;
    Ok(bytes)
}

fn read_limited(reader: &mut dyn Read, limit: usize, label: &str) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .with_context(|| format!("failed to read {label}"))?;
        if read == 0 {
            break;
        }
        ensure!(
            bytes.len() + read <= limit,
            "{label} exceeds maximum supported size of {limit} bytes"
        );
        bytes.extend_from_slice(&buffer[..read]);
    }
    Ok(bytes)
}

fn skip_exact(reader: &mut dyn Read, len: usize) -> Result<()> {
    let mut remaining = len;
    let mut buffer = [0_u8; 64 * 1024];
    while remaining > 0 {
        let read_len = remaining.min(buffer.len());
        reader
            .read_exact(&mut buffer[..read_len])
            .context("failed to skip zip entry data")?;
        remaining -= read_len;
    }
    Ok(())
}

fn read_u32_le(reader: &mut dyn Read) -> Result<u32> {
    let mut bytes = [0_u8; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn zip64_compressed_size(
    extra: &[u8],
    has_uncompressed_size: bool,
    has_compressed_size: bool,
) -> Result<Option<u64>> {
    let mut cursor = 0;
    while cursor + 4 <= extra.len() {
        let tag = u16::from_le_bytes([extra[cursor], extra[cursor + 1]]);
        let len = u16::from_le_bytes([extra[cursor + 2], extra[cursor + 3]]) as usize;
        cursor += 4;
        ensure!(cursor + len <= extra.len(), "zip extra field is truncated");
        if tag == 0x0001 {
            let field = &extra[cursor..cursor + len];
            let mut offset = 0;
            if has_uncompressed_size {
                ensure!(offset + 8 <= field.len(), "zip64 extra field is truncated");
                offset += 8;
            }
            if has_compressed_size {
                ensure!(offset + 8 <= field.len(), "zip64 extra field is truncated");
                return Ok(Some(u64::from_le_bytes(
                    field[offset..offset + 8]
                        .try_into()
                        .expect("zip64 size slice"),
                )));
            }
            return Ok(None);
        }
        cursor += len;
    }
    Ok(None)
}

fn checked_add(left: usize, right: usize) -> Result<usize> {
    left.checked_add(right)
        .context("local 3MF scan size overflowed")
}

fn is_supported_thumbnail_entry(name: &str) -> bool {
    let normalized = name.replace('\\', "/").to_ascii_lowercase();
    match normalized.as_str() {
        "metadata/thumbnail.png"
        | "metadata/thumbnail.jpg"
        | "metadata/thumbnail.jpeg"
        | "metadata/thumbnail_small.png"
        | "metadata/plate_1.png"
        | "metadata/top_1.png" => true,
        _ if normalized.starts_with("metadata/")
            && (normalized.ends_with(".png")
                || normalized.ends_with(".jpg")
                || normalized.ends_with(".jpeg")) =>
        {
            true
        }
        _ if normalized.ends_with(".png")
            || normalized.ends_with(".jpg")
            || normalized.ends_with(".jpeg") =>
        {
            true
        }
        _ => false,
    }
}

fn local_file_candidates(filename: &str, print_type: Option<&str>) -> Vec<String> {
    let filename = filename.trim().replace('\\', "/");
    if filename.is_empty()
        || filename.contains('\0')
        || filename.contains('\r')
        || filename.contains('\n')
    {
        return Vec::new();
    }

    let relative = filename.trim_start_matches('/');
    let mut candidates = Vec::new();
    if relative.starts_with("cache/") || relative.starts_with("sdcard/") {
        push_unique(&mut candidates, format!("/{relative}"));
        push_unique(&mut candidates, relative.to_owned());
    } else {
        match print_type_root(print_type) {
            Some(root) => push_unique(&mut candidates, format!("{root}/{relative}")),
            None => {
                push_unique(&mut candidates, format!("/cache/{relative}"));
                push_unique(&mut candidates, format!("/sdcard/{relative}"));
            }
        }
        push_unique(&mut candidates, relative.to_owned());
        if filename.starts_with('/') {
            push_unique(&mut candidates, format!("/{relative}"));
        }
    }
    candidates
}

fn print_type_root(print_type: Option<&str>) -> Option<&'static str> {
    match print_type.map(str::trim) {
        Some(value) if value.eq_ignore_ascii_case("cloud") => Some("/cache"),
        Some(value) if value.eq_ignore_ascii_case("local") => Some("/sdcard"),
        _ => None,
    }
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.iter().any(|existing| existing == &value) {
        values.push(value);
    }
}

fn image_content_type(content_type: Option<&str>, bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xff, 0xd8]) {
        return "image/jpeg".to_owned();
    }
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return "image/png".to_owned();
    }
    if let Some(content_type) = content_type
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .filter(|value| {
            value.eq_ignore_ascii_case("image/png") || value.eq_ignore_ascii_case("image/jpeg")
        })
    {
        return content_type.to_ascii_lowercase();
    }
    "application/octet-stream".to_owned()
}

fn path_content_type(path: &str) -> Option<&'static str> {
    let path = path.to_ascii_lowercase();
    if path.ends_with(".png") {
        Some("image/png")
    } else if path.ends_with(".jpg") || path.ends_with(".jpeg") {
        Some("image/jpeg")
    } else {
        None
    }
}

fn trimmed(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn normalized_request_task(value: Option<&str>) -> Option<String> {
    trimmed(value).map(str::to_owned)
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
    use super::{
        extract_bambu_3mf_thumbnail_stream, is_supported_thumbnail_entry, local_file_candidates,
        select_cloud_task, TaskKey, ZIP_LOCAL_FILE_HEADER,
    };
    use crate::bambu::{PrinterStatus, Task};

    #[test]
    fn task_key_tracks_the_active_print_identity() {
        let report = PrinterStatus {
            task_id: Some("task-1".to_owned()),
            filename: Some("cube.3mf".to_owned()),
            task_name: Some("Cube".to_owned()),
            start_time: Some("2026-01-01".to_owned()),
            ..PrinterStatus::default()
        };

        assert!(TaskKey::from_report(&report).is_some());
        assert_eq!(TaskKey::from_report(&PrinterStatus::default()), None);
    }

    #[test]
    fn cloud_task_selection_prefers_task_id_then_title() {
        let tasks = vec![
            Task {
                id: Some("old".to_owned()),
                title: Some("Cube".to_owned()),
                ..Task::default()
            },
            Task {
                id: Some("task-1".to_owned()),
                title: Some("Other".to_owned()),
                ..Task::default()
            },
        ];
        let report = PrinterStatus {
            task_id: Some("task-1".to_owned()),
            task_name: Some("Cube".to_owned()),
            ..PrinterStatus::default()
        };

        assert_eq!(
            select_cloud_task(&tasks, &report).unwrap().id.as_deref(),
            Some("task-1")
        );

        let report = PrinterStatus {
            task_name: Some("Cube".to_owned()),
            ..PrinterStatus::default()
        };
        assert_eq!(
            select_cloud_task(&tasks, &report).unwrap().id.as_deref(),
            Some("old")
        );

        let report = PrinterStatus {
            start_time: Some("2026-01-01T10:00:00Z".to_owned()),
            ..PrinterStatus::default()
        };
        let tasks = vec![Task {
            id: Some("start-time-match".to_owned()),
            start_time: Some("2026-01-01T10:00:00Z".to_owned()),
            ..Task::default()
        }];
        assert_eq!(
            select_cloud_task(&tasks, &report).unwrap().id.as_deref(),
            Some("start-time-match")
        );

        let report = PrinterStatus {
            task_id: Some("missing".to_owned()),
            task_name: Some("No match".to_owned()),
            start_time: Some("no-match".to_owned()),
            ..PrinterStatus::default()
        };
        assert!(select_cloud_task(&tasks, &report).is_none());
    }

    #[test]
    fn local_file_candidates_try_print_cache_first() {
        assert_eq!(
            local_file_candidates("cube.3mf", None),
            vec!["/cache/cube.3mf", "/sdcard/cube.3mf", "cube.3mf"]
        );
        assert_eq!(
            local_file_candidates("cube.3mf", Some("cloud")),
            vec!["/cache/cube.3mf", "cube.3mf"]
        );
        assert_eq!(
            local_file_candidates("cube.3mf", Some("local")),
            vec!["/sdcard/cube.3mf", "cube.3mf"]
        );
        assert_eq!(
            local_file_candidates("/sdcard/cube.3mf", Some("cloud")),
            vec!["/sdcard/cube.3mf", "sdcard/cube.3mf"]
        );
        assert_eq!(
            local_file_candidates("/cache/cube.3mf", Some("local")),
            vec!["/cache/cube.3mf", "cache/cube.3mf"]
        );
    }

    #[test]
    fn streamed_3mf_thumbnail_reads_first_thumbnail_entry() {
        let thumbnail = b"\x89PNG\r\n\x1a\nthumbnail";
        let mut archive = Vec::new();
        archive.extend(stored_zip_entry(
            "Metadata/model_settings.config",
            b"settings",
        ));
        archive.extend(stored_zip_entry("Metadata/thumbnail.png", thumbnail));

        let image = extract_bambu_3mf_thumbnail_stream(&mut archive.as_slice()).unwrap();

        assert_eq!(image.content_type, "image/png");
        assert_eq!(image.bytes.as_ref(), thumbnail);
    }

    #[test]
    fn supported_thumbnail_entry_recognizes_bambu_thumbnail_names() {
        assert!(is_supported_thumbnail_entry("Metadata/thumbnail.png"));
        assert!(is_supported_thumbnail_entry("Metadata/plate_1.png"));
        assert!(is_supported_thumbnail_entry("foo/model.png"));
        assert!(!is_supported_thumbnail_entry("Metadata/model.xml"));
    }

    #[test]
    fn bambu_3mf_streaming_subset_rejects_data_descriptors() {
        let mut archive = Vec::new();
        archive.extend(stored_zip_entry_with_flags(
            "Metadata/model_settings.config",
            b"settings",
            0x0008,
        ));
        archive.extend(stored_zip_entry("Metadata/thumbnail.png", b"thumbnail"));

        let error = extract_bambu_3mf_thumbnail_stream(&mut archive.as_slice()).unwrap_err();

        assert!(error.to_string().contains("data descriptor"));
    }

    fn stored_zip_entry(name: &str, data: &[u8]) -> Vec<u8> {
        stored_zip_entry_with_flags(name, data, 0)
    }

    fn stored_zip_entry_with_flags(name: &str, data: &[u8], flags: u16) -> Vec<u8> {
        let mut entry = Vec::new();
        entry.extend(ZIP_LOCAL_FILE_HEADER.to_le_bytes());
        entry.extend(20_u16.to_le_bytes());
        entry.extend(flags.to_le_bytes());
        entry.extend(0_u16.to_le_bytes());
        entry.extend(0_u16.to_le_bytes());
        entry.extend(0_u16.to_le_bytes());
        entry.extend(0_u32.to_le_bytes());
        entry.extend((data.len() as u32).to_le_bytes());
        entry.extend((data.len() as u32).to_le_bytes());
        entry.extend((name.len() as u16).to_le_bytes());
        entry.extend(0_u16.to_le_bytes());
        entry.extend(name.as_bytes());
        entry.extend(data);
        entry
    }
}
