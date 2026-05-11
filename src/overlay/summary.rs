use std::collections::HashMap;

use chrono::Utc;
use serde::Serialize;

use crate::bambu::{
    AmsState, CloudDevice, CurrentPrintResponse, PrinterStatus, Task, TasksResponse, Tray,
};

use super::format::{
    format_percent, format_seconds, format_temperature, format_weight, parse_bambu_datetime,
    progress_number,
};

type TaskMatches = HashMap<String, TaskMatch>;

#[derive(Debug, Clone)]
struct TaskMatch {
    task: Task,
    active: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
enum TaskSource {
    #[default]
    #[serde(rename = "printer status")]
    PrinterStatus,
    #[serde(rename = "printer status + task history")]
    PrinterStatusWithTaskHistory,
    #[serde(rename = "task history")]
    TaskHistory,
    #[serde(rename = "task history fallback")]
    TaskHistoryFallback,
}

#[derive(Debug, Clone, Default)]
pub(super) struct DeviceSummary {
    name: String,
    model_name: Option<String>,
    product_name: Option<String>,
    online: bool,
    task_id: Option<String>,
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
    name: String,
    model: String,
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
    device: &'a CloudDevice,
    report: Option<&'a PrinterStatus>,
}

impl<'a> DeviceFields<'a> {
    fn new(device: &'a CloudDevice, report: Option<&'a PrinterStatus>) -> Self {
        Self { device, report }
    }

    fn cloud_print(&self) -> &PrinterStatus {
        &self.device.status
    }

    fn print_string(&self, pick: impl Fn(&PrinterStatus) -> Option<&String>) -> Option<String> {
        self.report
            .and_then(&pick)
            .or_else(|| pick(self.cloud_print()))
            .cloned()
    }

    fn print_f64(&self, pick: impl Fn(&PrinterStatus) -> Option<f64>) -> Option<f64> {
        self.report
            .and_then(&pick)
            .or_else(|| pick(self.cloud_print()))
    }

    fn print_i64(&self, pick: impl Fn(&PrinterStatus) -> Option<i64>) -> Option<i64> {
        self.report
            .and_then(&pick)
            .or_else(|| pick(self.cloud_print()))
    }

    fn ams(&self) -> Option<&AmsState> {
        self.report
            .and_then(|print| print.ams.as_ref())
            .filter(|ams| ams.has_spool_data())
            .or(self.cloud_print().ams.as_ref())
    }

    fn external_tray(&self) -> Option<&Tray> {
        self.report
            .and_then(|print| print.external_tray.as_ref())
            .filter(|tray| tray.has_spool_data())
            .or(self.cloud_print().external_tray.as_ref())
    }

    fn display_mode(&self) -> Option<String> {
        self.report
            .and_then(print_mode)
            .or_else(|| print_mode(self.cloud_print()))
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

    fn summary(&self) -> (Option<String>, DeviceSummary) {
        let device_id = self.device_id();
        let task_id = self.task_id();
        let task_name = self.task_name();
        let task_status = self.task_status();
        let progress = self.progress();
        let prediction = self.prediction();
        let start_time = self.start_time();
        let has_print_status_task = self.has_print_status_task(
            &task_name,
            &task_id,
            &task_status,
            &start_time,
            prediction,
            progress,
        );

        let summary = DeviceSummary {
            name: self
                .device
                .name
                .clone()
                .unwrap_or_else(|| "Bambu printer".to_owned()),
            model_name: self.device.model_name.clone(),
            product_name: self.device.product_name.clone(),
            online: self.device.online.unwrap_or(true),
            task_id,
            task_name: task_name.clone(),
            title: task_name,
            filename: self.print_string(|print| print.filename.as_ref()),
            task_status,
            start_time,
            prediction,
            progress,
            thumbnail: None,
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
        };

        (device_id, summary)
    }
}

struct TaskRecord<'a> {
    task: &'a Task,
}

impl<'a> TaskRecord<'a> {
    fn new(task: &'a Task) -> Self {
        Self { task }
    }

    fn device_id(&self) -> Option<String> {
        self.task.device_id.clone()
    }

    fn active(&self) -> bool {
        let start = self
            .task
            .start_time
            .as_deref()
            .and_then(parse_bambu_datetime);
        let end = self.task.end_time.as_deref().and_then(parse_bambu_datetime);
        match (start, end) {
            (None, _) => false,
            (Some(_), None) => true,
            (Some(start), Some(end)) if (end - start).num_seconds().abs() <= 60 => true,
            _ => self
                .task
                .status
                .as_deref()
                .map(|status| {
                    matches!(
                        status.to_ascii_lowercase().as_str(),
                        "active" | "running" | "printing"
                    )
                })
                .unwrap_or(false),
        }
    }

    fn start_sort_key(&self) -> i64 {
        self.task
            .start_time
            .as_deref()
            .and_then(parse_bambu_datetime)
            .map(|datetime| datetime.timestamp())
            .unwrap_or(i64::MIN)
    }

    fn summary(&self, active: bool) -> DeviceSummary {
        let task_name = self
            .task
            .display_title()
            .unwrap_or_else(|| "unknown task".to_owned());
        DeviceSummary {
            name: self
                .task
                .device_name
                .clone()
                .unwrap_or_else(|| "Bambu printer".to_owned()),
            model_name: self.task.device_model.clone(),
            online: true,
            task_id: self.task.id.clone(),
            task_name: Some(task_name.clone()),
            title: Some(task_name),
            filename: self.task.plate_name.clone(),
            task_status: self.task.status.clone(),
            start_time: self.task.start_time.clone(),
            prediction: self.task.cost_time,
            progress: None,
            thumbnail: self.task.cover.clone(),
            weight: self.task.weight.clone(),
            is_printing: active,
            task_source: if active {
                TaskSource::TaskHistoryFallback
            } else {
                TaskSource::TaskHistory
            },
            plate_index: self.task.plate_index.clone(),
            ..Default::default()
        }
    }
}

pub(super) fn summarize_devices(
    print_payload: &CurrentPrintResponse,
    tasks_payload: &TasksResponse,
    reports: &HashMap<String, PrinterStatus>,
) -> Vec<DeviceSummary> {
    let tasks_by_device = tasks_by_device(tasks_payload);
    print_payload
        .devices
        .iter()
        .map(|device| summarize_device(device, &tasks_by_device, reports))
        .collect()
}

fn summarize_device(
    device: &CloudDevice,
    tasks_by_device: &TaskMatches,
    reports: &HashMap<String, PrinterStatus>,
) -> DeviceSummary {
    let report = device.id.as_ref().and_then(|id| reports.get(id));
    let fields = DeviceFields::new(device, report);
    let (device_id, mut summary) = fields.summary();
    let task_match = device_id.as_ref().and_then(|id| tasks_by_device.get(id));

    if let Some(history_task) = task_match {
        if !summary.is_printing {
            summary = TaskRecord::new(&history_task.task).summary(history_task.active);
        } else {
            fill_missing_from_task(&mut summary, &history_task.task);
        }
    }
    summary
}

pub(super) fn overlay_device(device: DeviceSummary) -> OverlayDevice {
    let mut progress_source = "reported";
    let mut progress = device.progress.and_then(progress_number);
    if progress.is_none() {
        progress = estimated_progress(&device);
        progress_source = "estimated";
    }
    OverlayDevice {
        name: device.name,
        model: device
            .product_name
            .or(device.model_name)
            .unwrap_or_else(|| "unknown model".to_owned()),
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

fn tasks_by_device(tasks_payload: &TasksResponse) -> TaskMatches {
    let mut matches = tasks_payload
        .hits
        .iter()
        .map(|task| {
            let record = TaskRecord::new(task);
            (task, record.active(), record.start_sort_key())
        })
        .collect::<Vec<_>>();
    matches.sort_by_key(|(_, active, sort_key)| (*active, *sort_key));
    matches.reverse();

    let mut by_device = HashMap::new();
    for (task, active, _) in matches {
        let record = TaskRecord::new(task);
        if let Some(device_id) = record.device_id() {
            by_device.entry(device_id).or_insert(TaskMatch {
                task: task.clone(),
                active,
            });
        }
    }
    by_device
}

fn fill_missing_from_task(summary: &mut DeviceSummary, task: &Task) {
    let task_summary = TaskRecord::new(task).summary(true);
    if summary.task_id.is_none() {
        summary.task_id = task_summary.task_id;
    }
    if summary.task_name.is_none() {
        summary.task_name = task_summary.task_name;
    }
    if summary.title.is_none() {
        summary.title = task_summary.title;
    }
    if summary.filename.is_none() {
        summary.filename = task_summary.filename;
    }
    if summary.task_status.is_none() {
        summary.task_status = task_summary.task_status;
    }
    if summary.start_time.is_none() {
        summary.start_time = task_summary.start_time;
    }
    if summary.prediction.is_none() {
        summary.prediction = task_summary.prediction;
    }
    if summary.thumbnail.is_none() {
        summary.thumbnail = task_summary.thumbnail;
    }
    if summary.weight.is_none() {
        summary.weight = task_summary.weight;
    }
    if summary.plate_index.is_none() {
        summary.plate_index = task_summary.plate_index;
    }
    if summary.task_source == TaskSource::PrinterStatus {
        summary.task_source = TaskSource::PrinterStatusWithTaskHistory;
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

    use crate::bambu::{CurrentPrintResponse, TasksResponse, Tray};

    use super::{overlay_device, spool_color, summarize_devices, TaskSource};

    fn decode<T: DeserializeOwned>(value: Value) -> T {
        serde_json::from_value(value).expect("fixture should match typed API shape")
    }

    #[test]
    fn summarize_devices_uses_matching_mqtt_report_fields_only() {
        let print: CurrentPrintResponse = decode(json!({
            "devices": [
                {
                    "dev_id": "printer-a",
                    "print": {
                        "mc_percent": 12,
                        "nozzle_temper": 210
                    }
                },
                {
                    "dev_id": "printer-b",
                    "print": {
                        "mc_percent": 1
                    }
                }
            ]
        }));
        let tasks = TasksResponse::default();
        let reports = HashMap::from([(
            "printer-a".to_owned(),
            decode(json!({
                "mc_percent": 42,
                "bed_temper": 60
            })),
        )]);

        let summaries = summarize_devices(&print, &tasks, &reports);
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
        let print: CurrentPrintResponse = decode(json!({
            "devices": [
                {
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
                }
            ]
        }));
        let tasks = TasksResponse::default();
        let reports = HashMap::from([(
            "printer-a".to_owned(),
            decode(json!({
                "mc_percent": 42,
                "ams": {"ams": [{"id": 0, "tray": [{"id": 0, "tray_color": "00000000"}]}]},
                "vt_tray": {"id": 255, "tray_color": "00000000"}
            })),
        )]);

        let summaries = summarize_devices(&print, &tasks, &reports);
        let device = overlay_device(summaries.into_iter().next().unwrap());

        assert_eq!(device.progress, Some(42.0));
        assert_eq!(device.ams_spools.len(), 1);
        assert_eq!(device.ams_spools[0].material, "PLA");
        assert_eq!(device.ams_spools[0].color, "#FF0000");
        assert_eq!(device.external_spool.as_ref().unwrap().material, "PETG");
        assert_eq!(device.external_spool.as_ref().unwrap().color, "#336699");
    }

    #[test]
    fn summarize_devices_merges_printer_status_task_history_and_spools() {
        let print: CurrentPrintResponse = decode(json!({
            "devices": [
                {
                    "dev_id": "printer-a",
                    "dev_name": "Office X1",
                    "dev_product_name": "X1 Carbon",
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
                }
            ]
        }));
        let tasks: TasksResponse = decode(json!({
            "hits": [
                {
                    "deviceId": "printer-a",
                    "title": "Task title",
                    "cover": "https://example.invalid/thumb.png",
                    "plateIndex": 2,
                    "plateName": "Plate 2",
                    "weight": 12.5,
                    "startTime": "2026-05-11T00:00:00Z",
                    "endTime": "2026-05-11T01:00:00Z",
                    "status": "finished"
                }
            ]
        }));

        let summaries = summarize_devices(&print, &tasks, &HashMap::new());
        let device = overlay_device(summaries.into_iter().next().unwrap());

        assert_eq!(device.name, "Office X1");
        assert_eq!(device.model, "X1 Carbon");
        assert_eq!(device.title.as_deref(), Some("Calibration cube"));
        assert_eq!(device.task_source, TaskSource::PrinterStatusWithTaskHistory);
        assert_eq!(device.progress, Some(25.0));
        assert_eq!(device.total_print_time.as_deref(), Some("1h"));
        assert_eq!(device.weight.as_deref(), Some("12.5g"));
        assert_eq!(device.plate.as_deref(), Some("2"));
        assert_eq!(
            device.thumbnail.as_deref(),
            Some("https://example.invalid/thumb.png")
        );
        assert_eq!(device.ams_spools.len(), 1);
        assert_eq!(device.ams_spools[0].material, "PLA");
        assert_eq!(device.ams_spools[0].color, "#FF0000");
    }

    #[test]
    fn summarize_devices_uses_active_task_history_when_printer_status_is_empty() {
        let print: CurrentPrintResponse = decode(json!({
            "devices": [
                {
                    "dev_id": "printer-a",
                    "dev_name": "Office X1",
                    "dev_online": true
                }
            ]
        }));
        let tasks: TasksResponse = decode(json!({
            "hits": [
                {
                    "deviceId": "printer-a",
                    "title": "Active print",
                    "id": "task-1",
                    "startTime": "2026-05-11T00:00:00Z",
                    "status": "running"
                }
            ]
        }));

        let summaries = summarize_devices(&print, &tasks, &HashMap::new());
        let device = overlay_device(summaries.into_iter().next().unwrap());

        assert!(device.is_printing);
        assert_eq!(device.task_name.as_deref(), Some("Active print"));
        assert_eq!(device.task_source, TaskSource::TaskHistoryFallback);
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
