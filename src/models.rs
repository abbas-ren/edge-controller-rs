use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum VoltageLevel {
    #[default]
    Low,
    High,
}

fn default_optional_voltage_level() -> Option<VoltageLevel> {
    Some(VoltageLevel::Low)
}

impl VoltageLevel {
    pub fn is_high(self) -> bool {
        self == Self::High
    }

    pub fn inverse(self) -> Self {
        match self {
            Self::Low => Self::High,
            Self::High => Self::Low,
        }
    }
}

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
#[serde(rename_all = "camelCase")]
pub struct RelayIdentityUpdateRequest {
    pub serial_number: Option<String>,
    pub vid_pid: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayConfigRequest {
    pub mac: String,
    pub serial: String,
    pub channel: u8,
    pub gen: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UartConfigRequest {
    pub mac: String,
    pub gen: u8,
    pub vid_pid: String,
    pub serial: Option<String>,
    pub channel: Option<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UartConfigResponse {
    pub mac: String,
    pub generation: u8,
    pub vid_pid: String,
    pub tty: String,
    pub usb_serial: Option<String>,
    pub interface: u8,
    pub topology: String,
    pub connection: String,
    pub verified: bool,
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
    pub gpio: Option<u32>,
    #[serde(
        rename = "gpioDefaultLevel",
        default = "default_optional_voltage_level"
    )]
    pub gpio_default_level: Option<VoltageLevel>,
    #[serde(
        rename = "relayDefaultLevel",
        default = "default_optional_voltage_level"
    )]
    pub relay_default_level: Option<VoltageLevel>,
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
    pub gpio: u32,
    #[serde(rename = "gpioDefaultLevel", default)]
    pub gpio_default_level: VoltageLevel,
    #[serde(rename = "relayDefaultLevel", default)]
    pub relay_default_level: VoltageLevel,
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

#[cfg(test)]
mod tests {
    use super::{IplModeRequest, IplRequest, RelayIdentityUpdateRequest, VoltageLevel};

    #[test]
    fn ipl_voltage_levels_default_low() {
        let request: IplRequest = serde_json::from_value(serde_json::json!({
            "gen": 4
        }))
        .unwrap();

        assert_eq!(request.gpio_default_level, Some(VoltageLevel::Low));
        assert_eq!(request.relay_default_level, Some(VoltageLevel::Low));
    }

    #[test]
    fn ipl_mode_voltage_levels_default_low() {
        let request: IplModeRequest = serde_json::from_value(serde_json::json!({
            "gpio": 17,
            "mac": "aabbccddeeff",
            "serial": "RELAY-A",
            "channel": 0
        }))
        .unwrap();

        assert_eq!(request.gpio_default_level, VoltageLevel::Low);
        assert_eq!(request.relay_default_level, VoltageLevel::Low);
    }

    #[test]
    fn relay_identity_requires_vid_pid_in_the_wire_contract() {
        let result = serde_json::from_value::<RelayIdentityUpdateRequest>(serde_json::json!({
            "serialNumber": "RELAY-A"
        }));

        assert!(result.is_err());
    }
}
