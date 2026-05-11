mod client;
mod de;
mod error;
mod models;

pub use client::BambuClient;
pub use models::{
    AmsState, AmsUnit, CloudDevice, CurrentPrintResponse, LoginResponse, PrinterStatus, Task,
    TasksResponse, Tray, UserPreference,
};

pub const API_BASE: &str = "https://api.bambulab.com";
pub const MQTT_HOST: &str = "us.mqtt.bambulab.com";
pub const MQTT_PORT: u16 = 8883;

const USER_AGENT: &str = "bambu-overlay/0.1";
