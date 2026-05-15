use std::collections::HashMap;

use chrono::Utc;
use serde::Serialize;

use crate::{
    bambu::{AmsState, PrinterStatus, Tray},
    devices::KnownDevice,
};

use super::format::{
    format_percent, format_seconds, format_temperature, format_weight, parse_bambu_datetime,
    progress_number,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
enum TaskSource {
    #[default]
    #[serde(rename = "printer status")]
    PrinterStatus,
}

#[derive(Debug, Clone, Default)]
pub(super) struct DeviceSummary {
    id: Option<String>,
    name: String,
    online: bool,
    task_name: Option<String>,
    title: Option<String>,
    filename: Option<String>,
    task_status: Option<String>,
    start_time: Option<String>,
    prediction: Option<f64>,
    progress: Option<f64>,
    thumbnail: Option<String>,
    weight: Option<String>,
    layer_current: Option<i64>,
    layer_total: Option<i64>,
    time_remaining: Option<String>,
    toolhead_temperature: Option<f64>,
    bed_temperature: Option<f64>,
    fan_speed: Option<f64>,
    print_mode: Option<String>,
    ams_spools: Vec<Spool>,
    external_spool: Option<Spool>,
    is_printing: bool,
    task_source: TaskSource,
    plate_index: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct OverlayDevice {
    id: Option<String>,
    name: String,
    online: bool,
    is_printing: bool,
    title: Option<String>,
    filename: Option<String>,
    task_name: Option<String>,
    task_status: Option<String>,
    task_source: TaskSource,
    mode: Option<String>,
    progress: Option<f64>,
    progress_source: Option<String>,
    total_print_time: Option<String>,
    weight: Option<String>,
    layer_current: Option<i64>,
    layer_total: Option<i64>,
    time_remaining: Option<String>,
    toolhead_temp: Option<String>,
    bed_temp: Option<String>,
    fan_speed: Option<String>,
    started: Option<String>,
    plate: Option<String>,
    ams_spools: Vec<Spool>,
    external_spool: Option<Spool>,
    thumbnail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct Spool {
    label: String,
    material: String,
    color: String,
}

struct DeviceFields<'a> {
    device: &'a KnownDevice,
    report: Option<&'a PrinterStatus>,
}

impl<'a> DeviceFields<'a> {
    fn new(device: &'a KnownDevice, report: Option<&'a PrinterStatus>) -> Self {
        Self { device, report }
    }

    fn catalog_status(&self) -> &PrinterStatus {
        &self.device.status
    }

    fn print_string(&self, pick: impl Fn(&PrinterStatus) -> Option<&String>) -> Option<String> {
        self.report
            .and_then(&pick)
            .or_else(|| pick(self.catalog_status()))
            .cloned()
    }

    fn print_f64(&self, pick: impl Fn(&PrinterStatus) -> Option<f64>) -> Option<f64> {
        self.report
            .and_then(&pick)
            .or_else(|| pick(self.catalog_status()))
    }

    fn print_i64(&self, pick: impl Fn(&PrinterStatus) -> Option<i64>) -> Option<i64> {
        self.report
            .and_then(&pick)
            .or_else(|| pick(self.catalog_status()))
    }

    fn ams(&self) -> Option<&AmsState> {
        self.report
            .and_then(|print| print.ams.as_ref())
            .filter(|ams| ams.has_spool_data())
            .or(self.catalog_status().ams.as_ref())
    }

    fn external_tray(&self) -> Option<&Tray> {
        self.report
            .and_then(|print| print.external_tray.as_ref())
            .filter(|tray| tray.has_spool_data())
            .or(self.catalog_status().external_tray.as_ref())
    }

    fn display_mode(&self) -> Option<String> {
        self.report
            .and_then(print_mode)
            .or_else(|| print_mode(self.catalog_status()))
    }

    fn task_id(&self) -> Option<String> {
        self.print_string(|print| print.task_id.as_ref())
    }

    fn device_id(&self) -> Option<String> {
        self.device.id.clone()
    }

    fn task_name(&self) -> Option<String> {
        self.print_string(|print| print.task_name.as_ref())
    }

    fn task_status(&self) -> Option<String> {
        self.print_string(|print| print.status.as_ref())
    }

    fn progress(&self) -> Option<f64> {
        self.print_f64(|print| print.progress)
    }

    fn prediction(&self) -> Option<f64> {
        self.print_f64(|print| print.prediction_seconds)
    }

    fn start_time(&self) -> Option<String> {
        self.print_string(|print| print.start_time.as_ref())
    }

    fn has_print_status_task(
        &self,
        task_name: &Option<String>,
        task_id: &Option<String>,
        task_status: &Option<String>,
        start_time: &Option<String>,
        prediction: Option<f64>,
        progress: Option<f64>,
    ) -> bool {
        task_name.is_some()
            || task_id.is_some()
            || task_status.is_some()
            || start_time.is_some()
            || prediction.is_some()
            || progress.is_some()
            || self.print_string(|print| print.filename.as_ref()).is_some()
            || self.print_i64(|print| print.layer_current).is_some()
            || self.print_i64(|print| print.layer_total).is_some()
    }

    fn summary(&self) -> DeviceSummary {
        let device_id = self.device_id();
        let task_id = self.task_id();
        let task_name = self.task_name();
        let task_status = self.task_status();
        let progress = self.progress();
        let prediction = self.prediction();
        let start_time = self.start_time();
        let filename = self.print_string(|print| print.filename.as_ref());
        let has_print_status_task = self.has_print_status_task(
            &task_name,
            &task_id,
            &task_status,
            &start_time,
            prediction,
            progress,
        );
        let thumbnail = thumbnail_path(
            device_id.as_deref(),
            has_print_status_task,
            task_id.as_deref(),
            filename.as_deref(),
            task_name.as_deref(),
            start_time.as_deref(),
        );

        DeviceSummary {
            id: device_id,
            name: self
                .device
                .name
                .clone()
                .unwrap_or_else(|| "Bambu printer".to_owned()),
            online: self.device.online.unwrap_or(true),
            task_name: task_name.clone(),
            title: task_name,
            filename: filename.clone(),
            task_status,
            start_time,
            prediction,
            progress,
            thumbnail,
            weight: self.print_string(|print| print.weight.as_ref()),
            layer_current: self.print_i64(|print| print.layer_current),
            layer_total: self.print_i64(|print| print.layer_total),
            time_remaining: self
                .print_f64(|print| print.remaining_minutes)
                .map(|minutes| format_seconds(minutes * 60.0)),
            toolhead_temperature: self.print_f64(|print| print.toolhead_temperature),
            bed_temperature: self.print_f64(|print| print.bed_temperature),
            fan_speed: self.print_f64(|print| print.fan_speed),
            print_mode: self.display_mode(),
            ams_spools: ams_spools(self.ams()),
            external_spool: external_spool(self.external_tray()),
            is_printing: has_print_status_task,
            task_source: TaskSource::PrinterStatus,
            plate_index: None,
        }
    }
}

pub(super) fn summarize_devices(
    devices: &[KnownDevice],
    reports: &HashMap<String, PrinterStatus>,
) -> Vec<DeviceSummary> {
    devices
        .iter()
        .map(|device| summarize_device(device, reports))
        .collect()
}

fn summarize_device(
    device: &KnownDevice,
    reports: &HashMap<String, PrinterStatus>,
) -> DeviceSummary {
    let report = device.id.as_ref().and_then(|id| reports.get(id));
    let fields = DeviceFields::new(device, report);
    fields.summary()
}

fn thumbnail_path(
    device_id: Option<&str>,
    has_print_status_task: bool,
    task_id: Option<&str>,
    filename: Option<&str>,
    task_name: Option<&str>,
    start_time: Option<&str>,
) -> Option<String> {
    if !has_print_status_task {
        return None;
    }
    let device_id = device_id?;
    let mut path = format!("/api/thumbnail?device={}", encode_query_value(device_id));
    if let Some(task) = task_id
        .or(filename)
        .or(task_name)
        .or(start_time)
        .map(str::trim)
        .filter(|task| !task.is_empty())
    {
        path.push_str("&task=");
        path.push_str(&encode_query_value(task));
    }
    Some(path)
}

fn encode_query_value(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(byte as char);
            }
            _ => {
                encoded.push('%');
                encoded.push(hex(byte >> 4));
                encoded.push(hex(byte & 0x0f));
            }
        }
    }
    encoded
}

fn hex(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        10..=15 => (b'A' + nibble - 10) as char,
        _ => unreachable!("nibble must be four bits"),
    }
}

pub(super) fn overlay_device(device: DeviceSummary) -> OverlayDevice {
    let mut progress_source = "reported";
    let mut progress = device.progress.and_then(progress_number);
    if progress.is_none() {
        progress = estimated_progress(&device);
        progress_source = "estimated";
    }
    OverlayDevice {
        id: device.id,
        name: device.name,
        online: device.online,
        is_printing: device.is_printing,
        title: device.title.or(device.task_name.clone()),
        filename: device.filename,
        task_name: device.task_name,
        task_status: device.task_status,
        task_source: device.task_source,
        mode: device.print_mode,
        progress: progress.map(|value| (value * 10.0).round() / 10.0),
        progress_source: progress.map(|_| progress_source.to_owned()),
        total_print_time: device.prediction.map(format_seconds),
        weight: device.weight.as_deref().and_then(format_weight),
        layer_current: device.layer_current,
        layer_total: device.layer_total,
        time_remaining: device.time_remaining,
        toolhead_temp: device.toolhead_temperature.map(format_temperature),
        bed_temp: device.bed_temperature.map(format_temperature),
        fan_speed: device.fan_speed.map(format_percent),
        started: device.start_time,
        plate: device.plate_index,
        ams_spools: device.ams_spools,
        external_spool: device.external_spool,
        thumbnail: device.thumbnail,
    }
}

fn ams_spools(ams: Option<&AmsState>) -> Vec<Spool> {
    let Some(ams) = ams else {
        return Vec::new();
    };
    let mut spools = Vec::new();
    for (ams_index, ams_unit) in ams.ams.iter().enumerate() {
        for (tray_index, tray) in ams_unit.tray.iter().enumerate() {
            let mut label = ((tray.id.unwrap_or(tray_index as i64)) + 1).to_string();
            if ams.ams.len() > 1 {
                label = format!("{}-{label}", ams_unit.id.unwrap_or(ams_index as i64) + 1);
            }
            if let Some(spool) = spool_summary(tray, label) {
                spools.push(spool);
            }
        }
    }
    spools
}

fn external_spool(tray: Option<&Tray>) -> Option<Spool> {
    tray.and_then(|tray| spool_summary(tray, "ext".to_owned()))
}

fn spool_summary(tray: &Tray, label: String) -> Option<Spool> {
    let material = tray
        .material
        .clone()
        .or_else(|| tray.display_name.clone())
        .or_else(|| tray.sub_brand.clone())
        .or_else(|| tray.info_index.clone());
    let color = spool_color(tray);
    if material.is_none() && color.is_none() {
        return None;
    }
    Some(Spool {
        label,
        material: material.unwrap_or_else(|| "Filament".to_owned()),
        color: color.unwrap_or_else(|| "#9CA3AF".to_owned()),
    })
}

fn spool_color(tray: &Tray) -> Option<String> {
    let color = tray.color.as_ref().or_else(|| tray.cols.first())?;
    let normalized = color.trim().trim_start_matches('#');
    if normalized.len() < 6 {
        return None;
    }
    let rgb = &normalized[..6];
    if rgb.eq_ignore_ascii_case("000000") && normalized.get(6..8) == Some("00") {
        return None;
    }
    u32::from_str_radix(rgb, 16).ok()?;
    Some(format!("#{rgb}", rgb = rgb.to_ascii_uppercase()))
}

fn print_mode(print_status: &PrinterStatus) -> Option<String> {
    if let Some(speed_level) = print_status.speed_level {
        return Some(match speed_level {
            1 => "Silent".to_owned(),
            2 => "Standard".to_owned(),
            3 => "Sport".to_owned(),
            4 => "Ludicrous".to_owned(),
            other => format!("Level {other}"),
        });
    }
    None
}

fn estimated_progress(device: &DeviceSummary) -> Option<f64> {
    if device.progress.is_some() {
        return None;
    }
    let start = device
        .start_time
        .as_deref()
        .and_then(parse_bambu_datetime)?;
    let prediction = device.prediction?;
    if prediction <= 0.0 {
        return None;
    }
    let elapsed = (Utc::now() - start).num_seconds() as f64;
    Some((elapsed / prediction * 100.0).clamp(0.0, 100.0))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde::de::DeserializeOwned;
    use serde_json::{json, Value};

    use crate::{
        bambu::{CloudDevice, Tray},
        devices::KnownDevice,
    };

    use super::{overlay_device, spool_color, summarize_devices, TaskSource};

    fn decode<T: DeserializeOwned>(value: Value) -> T {
        serde_json::from_value(value).expect("fixture should match typed API shape")
    }

    fn device(value: Value) -> KnownDevice {
        KnownDevice::from_cloud(decode::<CloudDevice>(value))
    }

    #[test]
    fn summarize_devices_uses_matching_mqtt_report_fields_only() {
        let devices = vec![
            device(json!({
                    "dev_id": "printer-a",
                    "print": {
                        "mc_percent": 12,
                        "nozzle_temper": 210
                    }
            })),
            device(json!({
                    "dev_id": "printer-b",
                    "print": {
                        "mc_percent": 1
                    }
            })),
        ];
        let reports = HashMap::from([(
            "printer-a".to_owned(),
            decode(json!({
                "mc_percent": 42,
                "bed_temper": 60
            })),
        )]);

        let summaries = summarize_devices(&devices, &reports);
        let devices = summaries
            .into_iter()
            .map(overlay_device)
            .collect::<Vec<_>>();

        assert_eq!(devices[0].progress, Some(42.0));
        assert_eq!(devices[0].toolhead_temp.as_deref(), Some("210C"));
        assert_eq!(devices[0].bed_temp.as_deref(), Some("60C"));
        assert_eq!(devices[1].progress, Some(1.0));
    }

    #[test]
    fn summarize_devices_keeps_cloud_spools_when_mqtt_report_is_empty() {
        let devices = vec![device(json!({
                    "dev_id": "printer-a",
                    "print": {
                        "mc_percent": 12,
                        "ams": {
                            "ams": [
                                {
                                    "id": 0,
                                    "tray": [
                                        {
                                            "id": 0,
                                            "tray_type": "PLA",
                                            "tray_color": "ff0000ff"
                                        }
                                    ]
                                }
                            ]
                        },
                        "vt_tray": {
                            "id": 255,
                            "tray_type": "PETG",
                            "tray_color": "336699ff"
                        }
                    }
        }))];
        let reports = HashMap::from([(
            "printer-a".to_owned(),
            decode(json!({
                "mc_percent": 42,
                "ams": {"ams": [{"id": 0, "tray": [{"id": 0, "tray_color": "00000000"}]}]},
                "vt_tray": {"id": 255, "tray_color": "00000000"}
            })),
        )]);

        let summaries = summarize_devices(&devices, &reports);
        let device = overlay_device(summaries.into_iter().next().unwrap());

        assert_eq!(device.progress, Some(42.0));
        assert_eq!(device.ams_spools.len(), 1);
        assert_eq!(device.ams_spools[0].material, "PLA");
        assert_eq!(device.ams_spools[0].color, "#FF0000");
        assert_eq!(device.external_spool.as_ref().unwrap().material, "PETG");
        assert_eq!(device.external_spool.as_ref().unwrap().color, "#336699");
    }

    #[test]
    fn summarize_devices_uses_catalog_status_and_spools() {
        let devices = vec![device(json!({
                    "dev_id": "printer-a",
                    "dev_name": "Office X1",
                    "dev_online": true,
                    "print": {
                        "subtask_name": "Calibration cube",
                        "mc_percent": 25,
                        "cost_time": 3600,
                        "gcode_start_time": "2026-05-11T00:00:00Z",
                        "layer_num": 4,
                        "total_layer_num": 20,
                        "nozzle_temper": 220,
                        "bed_temper": 60,
                        "ams": {
                            "ams": [
                                {
                                    "id": 0,
                                    "tray": [
                                        {
                                            "id": 0,
                                            "tray_type": "PLA",
                                            "tray_color": "ff0000ff"
                                        }
                                    ]
                                }
                            ]
                        }
                    }
        }))];

        let summaries = summarize_devices(&devices, &HashMap::new());
        let device = overlay_device(summaries.into_iter().next().unwrap());

        assert_eq!(device.name, "Office X1");
        assert_eq!(device.title.as_deref(), Some("Calibration cube"));
        assert_eq!(device.task_source, TaskSource::PrinterStatus);
        assert_eq!(device.progress, Some(25.0));
        assert_eq!(device.total_print_time.as_deref(), Some("1h"));
        assert_eq!(device.weight, None);
        assert_eq!(device.plate, None);
        assert_eq!(
            device.thumbnail.as_deref(),
            Some("/api/thumbnail?device=printer-a&task=Calibration%20cube")
        );
        assert_eq!(device.ams_spools.len(), 1);
        assert_eq!(device.ams_spools[0].material, "PLA");
        assert_eq!(device.ams_spools[0].color, "#FF0000");
    }

    #[test]
    fn spool_color_normalizes_color_sources() {
        assert_eq!(
            spool_color(&decode::<Tray>(json!({"tray_color": "00ff00ff"}))).as_deref(),
            Some("#00FF00")
        );
        assert_eq!(
            spool_color(&decode::<Tray>(json!({"cols": ["336699ff"]}))).as_deref(),
            Some("#336699")
        );
        assert_eq!(
            spool_color(&decode::<Tray>(json!({"tray_color": "00000000"}))),
            None
        );
        assert_eq!(
            spool_color(&decode::<Tray>(json!({"tray_color": "xyz"}))),
            None
        );
    }
}
