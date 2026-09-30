use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Generation {
    Gen3,
    Gen4,
    Gen5,
}

impl Generation {
    pub fn from_int(v: u8) -> Option<Self> {
        match v {
            3 => Some(Self::Gen3),
            4 => Some(Self::Gen4),
            5 => Some(Self::Gen5),
            _ => None,
        }
    }

    pub fn as_int(&self) -> u8 {
        match self {
            Self::Gen3 => 3,
            Self::Gen4 => 4,
            Self::Gen5 => 5,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayRequest {
    pub serial: String,
    pub state: String,
    pub channel: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayStatusRequest {
    pub serial: String,
    pub channel: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayConfigRequest {
    pub mac: String,
    pub serial: String,
    pub channel: u8,
    pub gen: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteRequest {
    #[serde(default = "default_relay_generation")]
    pub gen: u8,

    pub uid: Option<String>,
    pub mac: Option<String>,
    pub serial: Option<String>,
    pub channel: Option<u8>,
}

fn default_relay_generation() -> u8 {
    4
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfirmationRequest {
    #[serde(rename = "controllerId")]
    pub controller_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IplRequest {
    pub gen: u8,
    pub gpio1: Option<u32>,
    pub gpio2: Option<u32>,
    pub mac: Option<String>,
    pub serial: Option<String>,
    pub channel: Option<u8>,
    pub path: Option<String>,
    pub uart: Option<String>,
    pub power: Option<String>,
    #[serde(rename = "sdk_ver")]
    pub sdk_ver: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IplModeRequest {
    pub gpio1: u32,
    pub gpio2: u32,
    pub mac: String,
    pub serial: String,
    pub channel: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Gen5TtyRequest {
    pub mac: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Gen5PowerRequest {
    pub state: String,
    pub power: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MappingEntryRequest {
    pub mac: String,
    pub gen: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RebootDeviceRequest {
    pub power: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoveIplRequest {
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RtosStartRequest {
    pub gen: u8,
    pub mac: Option<String>,
    pub serial: Option<String>,
    pub channel: Option<u8>,
    pub rtos: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RtosEndRequest {
    pub gen: u8,
    pub mac: Option<String>,
    pub serial: Option<String>,
    pub channel: Option<u8>,
    pub rtos: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistrationPayload {
    #[serde(rename = "macAddress")]
    pub mac_address: String,
    #[serde(rename = "ipAddress")]
    pub ip_address: String,
    #[serde(rename = "deviceFamily")]
    pub device_family: String,
    pub relays: Vec<RelayInventory>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uid: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayInventory {
    #[serde(rename = "serialNumber")]
    pub serial_number: String,
    pub state: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatPayload {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub uid: String,
    pub ip: String,
    pub timestamp: i64,
    #[serde(rename = "cpuCurrent")]
    pub cpu_current: String,
    #[serde(rename = "cpuTotal")]
    pub cpu_total: String,
    #[serde(rename = "cpuUsagePercent")]
    pub cpu_usage_percent: f64,
    #[serde(rename = "memoryUsed")]
    pub memory_used: String,
    #[serde(rename = "memoryTotal")]
    pub memory_total: String,
    #[serde(rename = "memoryUsagePercent")]
    pub memory_usage_percent: f64,
    #[serde(rename = "networkUpload")]
    pub network_upload: String,
    #[serde(rename = "networkDownload")]
    pub network_download: String,
    #[serde(rename = "diskUsed")]
    pub disk_used: String,
    #[serde(rename = "diskTotal")]
    pub disk_total: String,
    #[serde(rename = "diskUsagePercent")]
    pub disk_usage_percent: f64,
}
