use serde::Deserialize;

use bambu_overlay::bambu::{CurrentPrintResponse, PrinterStatus, TasksResponse, UserPreference};

#[test]
fn current_print_fixture_uses_observed_device_fields() {
    let response: CurrentPrintResponse =
        serde_json::from_str(include_str!("fixtures/current_print.json")).unwrap();

    let device = response.devices.first().unwrap();
    assert_eq!(device.id.as_deref(), Some("printer-a"));
    assert_eq!(device.name.as_deref(), Some("Office X1"));
    assert_eq!(device.online, Some(true));
}

#[test]
fn bind_fixture_uses_observed_device_fields() {
    let response: CurrentPrintResponse =
        serde_json::from_str(include_str!("fixtures/bind.json")).unwrap();

    let device = response.devices.first().unwrap();
    assert_eq!(device.id.as_deref(), Some("printer-a"));
    assert_eq!(device.name.as_deref(), Some("Office X1"));
    assert_eq!(device.online, Some(true));
    assert_eq!(device.access_code.as_deref(), Some("redacted"));
}

#[test]
fn tasks_fixture_uses_observed_task_fields() {
    let response: TasksResponse =
        serde_json::from_str(include_str!("fixtures/tasks.json")).unwrap();

    let task = response.hits.first().unwrap();
    assert_eq!(task.device_id.as_deref(), Some("printer-a"));
    assert_eq!(task.device_name.as_deref(), Some("Office X1"));
    assert_eq!(task.display_title().as_deref(), Some("Calibration cube"));
    assert_eq!(task.cost_time, Some(3600.0));
    assert_eq!(task.plate_index.as_deref(), Some("2"));
    assert_eq!(task.plate_name.as_deref(), Some("Plate 2"));
}

#[test]
fn preference_fixture_reads_numeric_uid() {
    let preference: UserPreference =
        serde_json::from_str(include_str!("fixtures/preference.json")).unwrap();

    assert_eq!(preference.mqtt_user_id().as_deref(), Some("1234567890"));
}

#[test]
fn mqtt_report_fixture_uses_known_report_fields() {
    #[derive(Deserialize)]
    struct WrappedReport {
        print: PrinterStatus,
    }

    let report: WrappedReport =
        serde_json::from_str(include_str!("fixtures/mqtt_report.json")).unwrap();

    assert_eq!(report.print.task_id.as_deref(), Some("task-1"));
    assert_eq!(report.print.task_name.as_deref(), Some("Calibration cube"));
    assert_eq!(report.print.status.as_deref(), Some("RUNNING"));
    assert_eq!(report.print.progress, Some(25.0));
    assert_eq!(report.print.toolhead_temperature, Some(220.0));
    assert_eq!(report.print.bed_temperature, Some(60.0));
    assert_eq!(report.print.fan_speed, Some(40.0));
    assert_eq!(report.print.speed_level, Some(2));
    assert_eq!(report.print.ams.as_ref().unwrap().ams.len(), 1);
}
