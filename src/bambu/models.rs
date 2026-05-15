use serde::{Deserialize, Serialize};

use super::de;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginResponse {
    #[serde(default, deserialize_with = "de::optional_string")]
    pub access_token: Option<String>,
    #[serde(default, deserialize_with = "de::optional_string")]
    pub refresh_token: Option<String>,
    #[serde(default, deserialize_with = "de::optional_i64")]
    pub expires_in: Option<i64>,
    #[serde(default, deserialize_with = "de::optional_string")]
    pub login_type: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CurrentPrintResponse {
    #[serde(default)]
    pub devices: Vec<CloudDevice>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CloudDevice {
    #[serde(default, rename = "dev_id", deserialize_with = "de::optional_string")]
    pub id: Option<String>,
    #[serde(
        default,
        rename = "dev_name",
        alias = "name",
        deserialize_with = "de::optional_string"
    )]
    pub name: Option<String>,
    #[serde(default, rename = "dev_online", alias = "online")]
    pub online: Option<bool>,
    #[serde(
        default,
        rename = "dev_access_code",
        deserialize_with = "de::optional_string"
    )]
    pub access_code: Option<String>,
    #[serde(default, rename = "print")]
    pub status: PrinterStatus,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct PrinterStatus {
    #[serde(
        default,
        rename = "subtask_id",
        deserialize_with = "de::optional_string"
    )]
    pub task_id: Option<String>,
    #[serde(
        default,
        rename = "subtask_name",
        deserialize_with = "de::optional_string"
    )]
    pub task_name: Option<String>,
    #[serde(
        default,
        rename = "gcode_state",
        deserialize_with = "de::optional_string"
    )]
    pub status: Option<String>,
    #[serde(default, rename = "mc_percent", deserialize_with = "de::optional_f64")]
    pub progress: Option<f64>,
    #[serde(default, rename = "cost_time", deserialize_with = "de::optional_f64")]
    pub prediction_seconds: Option<f64>,
    #[serde(
        default,
        rename = "gcode_start_time",
        deserialize_with = "de::optional_string"
    )]
    pub start_time: Option<String>,
    #[serde(
        default,
        rename = "gcode_file",
        deserialize_with = "de::optional_string"
    )]
    pub filename: Option<String>,
    #[serde(
        default,
        rename = "print_type",
        deserialize_with = "de::optional_string"
    )]
    pub print_type: Option<String>,
    #[serde(default, deserialize_with = "de::optional_string")]
    pub weight: Option<String>,
    #[serde(default, rename = "layer_num", deserialize_with = "de::optional_i64")]
    pub layer_current: Option<i64>,
    #[serde(
        default,
        rename = "total_layer_num",
        deserialize_with = "de::optional_i64"
    )]
    pub layer_total: Option<i64>,
    #[serde(
        default,
        rename = "mc_remaining_time",
        deserialize_with = "de::optional_f64"
    )]
    pub remaining_minutes: Option<f64>,
    #[serde(
        default,
        rename = "nozzle_temper",
        deserialize_with = "de::optional_f64"
    )]
    pub toolhead_temperature: Option<f64>,
    #[serde(default, rename = "bed_temper", deserialize_with = "de::optional_f64")]
    pub bed_temperature: Option<f64>,
    #[serde(
        default,
        rename = "cooling_fan_speed",
        deserialize_with = "de::optional_f64"
    )]
    pub fan_speed: Option<f64>,
    #[serde(default)]
    pub ams: Option<AmsState>,
    #[serde(default, rename = "vt_tray")]
    pub external_tray: Option<Tray>,
    #[serde(default, rename = "spd_lvl", deserialize_with = "de::optional_i64")]
    pub speed_level: Option<i64>,
}

impl PrinterStatus {
    pub fn merge(&mut self, patch: PrinterStatus) {
        macro_rules! merge_fields {
            ($($field:ident),+ $(,)?) => {
                $(merge_option(&mut self.$field, patch.$field);)+
            };
        }

        merge_fields!(
            task_id,
            task_name,
            status,
            progress,
            prediction_seconds,
            start_time,
            filename,
            print_type,
            weight,
            layer_current,
            layer_total,
            remaining_minutes,
            toolhead_temperature,
            bed_temperature,
            fan_speed,
            speed_level,
        );
        merge_ams(&mut self.ams, patch.ams);
        merge_tray(&mut self.external_tray, patch.external_tray);
    }
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct AmsState {
    #[serde(default, rename = "tray_now", deserialize_with = "de::optional_i64")]
    pub tray_now: Option<i64>,
    #[serde(default)]
    pub ams: Vec<AmsUnit>,
}

impl AmsState {
    pub fn has_spool_data(&self) -> bool {
        self.ams
            .iter()
            .flat_map(|unit| &unit.tray)
            .any(Tray::has_spool_data)
    }
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct AmsUnit {
    #[serde(default, deserialize_with = "de::optional_i64")]
    pub id: Option<i64>,
    #[serde(default)]
    pub tray: Vec<Tray>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct Tray {
    #[serde(default, deserialize_with = "de::optional_i64")]
    pub id: Option<i64>,
    #[serde(
        default,
        rename = "tray_type",
        deserialize_with = "de::optional_string"
    )]
    pub material: Option<String>,
    #[serde(
        default,
        rename = "tray_id_name",
        deserialize_with = "de::optional_string"
    )]
    pub display_name: Option<String>,
    #[serde(
        default,
        rename = "tray_sub_brands",
        deserialize_with = "de::optional_string"
    )]
    pub sub_brand: Option<String>,
    #[serde(
        default,
        rename = "tray_info_idx",
        deserialize_with = "de::optional_string"
    )]
    pub info_index: Option<String>,
    #[serde(
        default,
        rename = "tray_color",
        deserialize_with = "de::optional_string"
    )]
    pub color: Option<String>,
    #[serde(default)]
    pub cols: Vec<String>,
}

impl Tray {
    pub fn has_spool_data(&self) -> bool {
        self.material.as_deref().is_some_and(has_text)
            || self.display_name.as_deref().is_some_and(has_text)
            || self.sub_brand.as_deref().is_some_and(has_text)
            || self.info_index.as_deref().is_some_and(has_text)
            || self.color.as_deref().is_some_and(has_visible_color)
            || self.cols.iter().any(|color| has_visible_color(color))
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct TasksResponse {
    #[serde(default)]
    pub hits: Vec<Task>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Task {
    #[serde(default, rename = "deviceId", deserialize_with = "de::optional_string")]
    pub device_id: Option<String>,
    #[serde(default, deserialize_with = "de::optional_string")]
    pub title: Option<String>,
    #[serde(
        default,
        rename = "designTitle",
        deserialize_with = "de::optional_string"
    )]
    pub design_title: Option<String>,
    #[serde(
        default,
        rename = "designTitleTranslated",
        deserialize_with = "de::optional_string"
    )]
    pub translated_title: Option<String>,
    #[serde(
        default,
        rename = "deviceName",
        deserialize_with = "de::optional_string"
    )]
    pub device_name: Option<String>,
    #[serde(default, deserialize_with = "de::optional_string")]
    pub id: Option<String>,
    #[serde(default, deserialize_with = "de::optional_string")]
    pub status: Option<String>,
    #[serde(
        default,
        rename = "startTime",
        deserialize_with = "de::optional_string"
    )]
    pub start_time: Option<String>,
    #[serde(default, rename = "endTime", deserialize_with = "de::optional_string")]
    pub end_time: Option<String>,
    #[serde(default, rename = "costTime", deserialize_with = "de::optional_f64")]
    pub cost_time: Option<f64>,
    #[serde(default, deserialize_with = "de::optional_string")]
    pub cover: Option<String>,
    #[serde(default, deserialize_with = "de::optional_string")]
    pub weight: Option<String>,
    #[serde(
        default,
        rename = "plateIndex",
        deserialize_with = "de::optional_string"
    )]
    pub plate_index: Option<String>,
    #[serde(
        default,
        rename = "plateName",
        deserialize_with = "de::optional_string"
    )]
    pub plate_name: Option<String>,
}

impl Task {
    pub fn display_title(&self) -> Option<String> {
        self.title
            .clone()
            .or_else(|| self.design_title.clone())
            .or_else(|| self.translated_title.clone())
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct UserPreference {
    #[serde(default, deserialize_with = "de::optional_string")]
    pub uid: Option<String>,
}

impl UserPreference {
    pub fn mqtt_user_id(&self) -> Option<String> {
        let value = self.uid.as_deref()?;
        let normalized = value.trim().strip_prefix("u_").unwrap_or(value.trim());
        (!normalized.is_empty()).then(|| normalized.to_owned())
    }
}

#[derive(Serialize)]
pub(super) struct LoginRequest<'a> {
    pub account: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<&'a str>,
}

fn merge_option<T>(target: &mut Option<T>, patch: Option<T>) {
    if patch.is_some() {
        *target = patch;
    }
}

fn merge_ams(target: &mut Option<AmsState>, patch: Option<AmsState>) {
    let Some(patch) = patch else {
        return;
    };
    if target.is_none() || patch.has_spool_data() {
        *target = Some(patch);
    } else if let Some(tray_now) = patch.tray_now {
        if let Some(target) = target {
            target.tray_now = Some(tray_now);
        }
    }
}

fn merge_tray(target: &mut Option<Tray>, patch: Option<Tray>) {
    if patch.as_ref().is_some_and(Tray::has_spool_data) || target.is_none() {
        merge_option(target, patch);
    }
}

fn has_text(value: &str) -> bool {
    !value.trim().is_empty()
}

fn has_visible_color(value: &str) -> bool {
    let normalized = value.trim().trim_start_matches('#');
    if normalized.len() < 6 {
        return false;
    }
    !(normalized[..6].eq_ignore_ascii_case("000000") && normalized.get(6..8) == Some("00"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{PrinterStatus, UserPreference};

    #[test]
    fn user_preference_reads_only_uid_for_mqtt_user_id() {
        let preference: UserPreference =
            serde_json::from_value(json!({"uid": "u_123456"})).unwrap();

        assert_eq!(preference.mqtt_user_id().as_deref(), Some("123456"));

        let preference: UserPreference =
            serde_json::from_value(json!({"userId": "wrong", "id": "wrong"})).unwrap();

        assert_eq!(preference.mqtt_user_id(), None);
    }

    #[test]
    fn printer_status_merge_updates_every_field() {
        let mut base: PrinterStatus = serde_json::from_value(json!({
            "subtask_id": "old",
            "subtask_name": "old name",
            "gcode_state": "RUNNING",
            "mc_percent": 10,
            "cost_time": 100,
            "gcode_start_time": "2026-05-11T00:00:00Z",
            "gcode_file": "old.3mf",
            "print_type": "cloud",
            "weight": "10",
            "layer_num": 1,
            "total_layer_num": 2,
            "mc_remaining_time": 90,
            "nozzle_temper": 200,
            "bed_temper": 55,
            "cooling_fan_speed": 30,
            "spd_lvl": 2
        }))
        .unwrap();
        let patch: PrinterStatus = serde_json::from_value(json!({
            "subtask_id": "new",
            "subtask_name": "new name",
            "gcode_state": "FINISH",
            "mc_percent": 100,
            "cost_time": 200,
            "gcode_start_time": "2026-05-11T01:00:00Z",
            "gcode_file": "new.3mf",
            "print_type": "local",
            "weight": "20",
            "layer_num": 3,
            "total_layer_num": 4,
            "mc_remaining_time": 0,
            "nozzle_temper": 205,
            "bed_temper": 60,
            "cooling_fan_speed": 40,
            "vt_tray": {"id": 255, "tray_type": "PLA"},
            "ams": {"ams": [{"id": 0, "tray": [{"id": 0, "tray_type": "PETG"}]}]},
            "spd_lvl": 3
        }))
        .unwrap();

        base.merge(patch.clone());

        assert_eq!(base, patch);
    }

    #[test]
    fn printer_status_merge_keeps_spools_when_patch_has_no_spool_data() {
        let mut status: PrinterStatus = serde_json::from_value(json!({
            "mc_percent": 10,
            "ams": {"ams": [{"id": 0, "tray": [{"id": 0, "tray_type": "PLA", "tray_color": "ff0000ff"}]}]},
            "vt_tray": {"id": 255, "tray_type": "PETG", "tray_color": "336699ff"}
        }))
        .unwrap();
        let patch: PrinterStatus = serde_json::from_value(json!({
            "mc_percent": 20,
            "ams": {"tray_now": "0", "ams": [{"id": 0, "tray": [{"id": 0, "tray_color": "00000000"}]}]},
            "vt_tray": {"id": 255, "tray_color": "00000000"}
        }))
        .unwrap();

        status.merge(patch);

        assert_eq!(status.progress, Some(20.0));
        assert_eq!(status.ams.as_ref().unwrap().tray_now, Some(0));
        let tray = &status.ams.as_ref().unwrap().ams[0].tray[0];
        assert_eq!(tray.material.as_deref(), Some("PLA"));
        assert_eq!(tray.color.as_deref(), Some("ff0000ff"));
        let external = status.external_tray.as_ref().unwrap();
        assert_eq!(external.material.as_deref(), Some("PETG"));
        assert_eq!(external.color.as_deref(), Some("336699ff"));
    }
}
