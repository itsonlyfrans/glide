//! Types and bounded JSONL codecs for the UI-to-daemon protocol.

use glide_platform::{Monitor, Os, PermissionKind, Permissions};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Zeroizing owner for the JSON pairing code, without adding a dependency.
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PairingCode(String);

impl From<String> for PairingCode {
    fn from(code: String) -> Self {
        Self(code)
    }
}
impl From<&str> for PairingCode {
    fn from(code: &str) -> Self {
        Self(code.to_owned())
    }
}
impl std::ops::Deref for PairingCode {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}
impl std::fmt::Debug for PairingCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PairingCode([redacted])")
    }
}
impl Drop for PairingCode {
    fn drop(&mut self) {
        wipe_string(&mut self.0);
    }
}

/// Volatile clearing prevents dead-store elimination; copies owned by serde,
/// the OS pipe or Electron are outside this owner's lifetime guarantees.
pub fn wipe_bytes(bytes: &mut [u8]) {
    for byte in bytes {
        // SAFETY: this pointer references an exclusively borrowed live byte.
        unsafe {
            std::ptr::write_volatile(byte, 0);
        }
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}
fn wipe_string(text: &mut str) {
    // SAFETY: exclusive access, length unchanged, and zero is valid UTF-8.
    wipe_bytes(unsafe { text.as_bytes_mut() });
}
fn wipe_json_code(value: &mut Value) {
    if let Some(Value::String(code)) = value.get_mut("code") {
        wipe_string(code);
    }
}

#[cfg(test)]
mod secret_tests {
    use super::*;

    #[test]
    fn code_owners_preserve_json_redact_debug_and_wipe_valid_utf8() {
        let code = PairingCode::from("123456");
        assert_eq!(serde_json::to_string(&code).expect("JSON"), "\"123456\"");
        assert_eq!(&*code.clone(), "123456");
        assert_eq!(format!("{code:?}"), "PairingCode([redacted])");
        let mut text = String::from("123456世界");
        let length = text.len();
        wipe_string(&mut text);
        assert_eq!(text.len(), length);
        assert!(text.as_bytes().iter().all(|byte| *byte == 0));
        let mut value = serde_json::json!({"code":"123456", "address":"peer"});
        wipe_json_code(&mut value);
        assert_eq!(value["code"], "\0\0\0\0\0\0");
        assert_eq!(value["address"], "peer");
    }
}

pub const METHOD_GET_STATE: &str = "get_state";
/// Total files/directories per transfer, across up to 4096-entry manifest pages.
pub const MAX_TRANSFER_ITEMS: u32 = crate::wire::MAX_TRANSFER_ITEMS;
pub const METHOD_SET_SETTINGS: &str = "set_settings";
pub const METHOD_SET_SHARING: &str = "set_sharing";
pub const METHOD_SET_LAYOUT: &str = "set_layout";
pub const METHOD_PAIRING_START_HOST: &str = "pairing.start_host";
pub const METHOD_PAIRING_CANCEL_HOST: &str = "pairing.cancel_host";
pub const METHOD_PAIRING_JOIN: &str = "pairing.join";
pub const METHOD_PAIRING_CONFIRM: &str = "pairing.confirm";
pub const METHOD_PEER_ADD_MANUAL: &str = "peer.add_manual";
pub const METHOD_PEER_UNPAIR: &str = "peer.unpair";
pub const METHOD_PEER_CONFIGURE: &str = "peer.configure";
pub const METHOD_RETURN_HOME: &str = "return_home";
pub const METHOD_TRANSFER_CANCEL: &str = "transfer.cancel";
pub const METHOD_TRANSFER_CONFIRM: &str = "transfer.confirm";
pub const METHOD_PERMISSIONS_OPEN_SETTINGS: &str = "permissions.open_settings";
pub const METHOD_PERMISSIONS_REQUEST: &str = "permissions.request";
pub const METHOD_APP_SHUTDOWN: &str = "app.shutdown";

/// Complete UI snapshot returned by `get_state` and state events.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct State {
    #[serde(rename = "self")]
    pub self_info: SelfInfo,
    pub sharing_enabled: bool,
    pub active_device_id: String,
    pub permissions: PermissionState,
    pub peers: Vec<Peer>,
    pub discovered: Vec<DiscoveredPeer>,
    pub layout: Layout,
    pub settings: Settings,
    pub transfers: Vec<Transfer>,
}

/// Effective OS grants plus the daemon's capture recovery status.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PermissionState {
    pub accessibility: glide_platform::PermissionStatus,
    pub input_monitoring: glide_platform::PermissionStatus,
    pub injection: glide_platform::PermissionStatus,
    /// Grants are available, but capture still failed; relaunch may be necessary.
    #[serde(default)]
    pub restart_required: bool,
}

impl From<Permissions> for PermissionState {
    fn from(grants: Permissions) -> Self {
        Self {
            accessibility: grants.accessibility,
            input_monitoring: grants.input_monitoring,
            injection: grants.injection,
            restart_required: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SelfInfo {
    pub device_id: String,
    pub name: String,
    pub os: Os,
    pub fingerprint: String,
    pub listen_port: u16,
    pub version: String,
    pub monitors: Vec<Monitor>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Peer {
    pub device_id: String,
    pub name: String,
    pub os: Os,
    pub fingerprint: String,
    pub online: bool,
    pub connection: Connection,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<f64>,
    pub monitors: Vec<Monitor>,
    pub clipboard_enabled: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Connection {
    Offline,
    Connecting,
    Connected,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DiscoveredPeer {
    pub device_id: String,
    pub name: String,
    pub os: Os,
    pub address: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Layout {
    #[serde(default)]
    pub devices: Vec<LayoutDevice>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LayoutDevice {
    pub device_id: String,
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub device_name: String,
    pub hotkeys: Hotkeys,
    pub clipboard: ClipboardSettings,
    pub switching: SwitchingSettings,
    pub keyboard: KeyboardSettings,
    pub startup: StartupSettings,
    pub network: NetworkSettings,
    pub display: DisplaySettings,
}

/// A custom arrangement of this computer's own screens (empty: use the operating system's arrangement).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DisplaySettings {
    pub arrangement: Vec<MonitorPlacement>,
}

/// Where one of this computer's screens sits, in device-local logical pixels.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MonitorPlacement {
    pub monitor_id: String,
    pub x: f64,
    pub y: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            device_name: "Glide".to_owned(),
            hotkeys: Hotkeys::default(),
            clipboard: ClipboardSettings::default(),
            switching: SwitchingSettings::default(),
            keyboard: KeyboardSettings::default(),
            startup: StartupSettings::default(),
            network: NetworkSettings::default(),
            display: DisplaySettings::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Hotkeys {
    pub return_home: String,
    pub toggle_sharing: String,
}

impl Default for Hotkeys {
    fn default() -> Self {
        Self {
            return_home: "Ctrl+Alt+Shift+Home".to_owned(),
            toggle_sharing: "Ctrl+Alt+Shift+S".to_owned(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClipboardSettings {
    pub enabled: bool,
    pub sync_text: bool,
    pub sync_images: bool,
    pub sync_files: bool,
    pub max_auto_mb: u64,
    pub exclude_sensitive: bool,
}

impl Default for ClipboardSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            sync_text: true,
            sync_images: true,
            sync_files: true,
            max_auto_mb: 2048,
            exclude_sensitive: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SwitchingSettings {
    pub edge_delay_ms: u32,
    pub corner_dead_zone_px: f64,
    pub double_tap: bool,
    /// Multiplies mouse movement while this computer is controlling another one (1.0 = unchanged).
    pub pointer_speed: f64,
    /// Makes quick mouse movement travel further than slow movement while controlling another computer, like a Mac
    /// trackpad does (0.0 = off). Only used where the mouse reports raw, unaccelerated movement (Windows).
    pub pointer_acceleration: f64,
}

impl Default for SwitchingSettings {
    fn default() -> Self {
        Self {
            edge_delay_ms: 350,
            corner_dead_zone_px: 5.0,
            double_tap: false,
            pointer_speed: 1.0,
            pointer_acceleration: 0.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwapCtrlCmd {
    #[default]
    Auto,
    Always,
    Never,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct KeyboardSettings {
    pub swap_ctrl_cmd: SwapCtrlCmd,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StartupSettings {
    pub launch_at_login: bool,
    pub start_minimized: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NetworkSettings {
    pub port: u16,
    pub discovery: bool,
}

impl Default for NetworkSettings {
    fn default() -> Self {
        Self {
            port: 24800,
            discovery: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Transfer {
    pub id: String,
    pub direction: TransferDirection,
    pub peer_id: String,
    pub name: String,
    pub items: u32,
    pub bytes_total: u64,
    pub bytes_done: u64,
    pub rate_bps: u64,
    pub state: TransferState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferDirection {
    Send,
    Receive,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferState {
    Queued,
    AwaitingConfirm,
    Active,
    Done,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub method: String,
    #[serde(default = "empty_object")]
    pub params: Value,
}

impl Drop for Request {
    fn drop(&mut self) {
        wipe_json_code(&mut self.params);
    }
}

fn empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<IpcError>,
}

impl Drop for Response {
    fn drop(&mut self) {
        if let Some(result) = &mut self.result {
            wipe_json_code(result);
        }
    }
}

impl Response {
    pub fn success(id: u64, result: Value) -> Self {
        Self {
            id,
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    pub fn failure(id: u64, error: IpcError) -> Self {
        Self {
            id,
            ok: false,
            result: None,
            error: Some(error),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    BadCode,
    CodeExpired,
    LockedOut,
    Unreachable,
    NotPaired,
    InvalidParams,
    PermissionDenied,
    Internal,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct IpcError {
    pub code: ErrorCode,
    pub message: String,
}

impl IpcError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub enum Event {
    Ready {
        version: String,
    },
    State(Box<State>),
    #[serde(rename = "pairing.incoming")]
    PairingIncoming(PairingIncoming),
    #[serde(rename = "pairing.result")]
    PairingResult(PairingResult),
    #[serde(rename = "pairing.verify")]
    PairingVerify(PairingVerify),
    #[serde(rename = "peer.stats")]
    PeerStats(PeerStats),
    #[serde(rename = "transfer.progress")]
    TransferProgress(TransferProgress),
    Notification(Notification),
    ActiveChanged(ActiveChanged),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PairingIncoming {
    pub name: String,
    pub os: Os,
    pub address: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PairingResult {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<IpcError>,
}

/// Human-verification prompt shown after PAKE succeeds and before either peer is pinned.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PairingVerify {
    /// Three words derived from the authenticated pairing session.
    pub phrase: [String; 3],
    pub peer: VerificationPeer,
    /// Unix timestamp in milliseconds when the pending pairing expires.
    pub expires_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct VerificationPeer {
    pub name: String,
    pub os: Os,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PeerStats {
    pub device_id: String,
    pub latency_ms: f64,
    pub rx_bps: u64,
    pub tx_bps: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TransferProgress {
    pub id: String,
    pub bytes_done: u64,
    pub rate_bps: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Notification {
    pub level: String,
    pub title: String,
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActiveChanged {
    pub device_id: String,
    pub reason: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EmptyParams {}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SetSettingsParams {
    pub patch: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SetSharingParams {
    pub enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SetLayoutParams {
    pub devices: Vec<LayoutDevice>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PairingJoinParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    pub code: PairingCode,
}

impl PairingJoinParams {
    /// A join request must identify exactly one peer by address or device ID.
    pub fn has_single_target(&self) -> bool {
        self.address.is_some() ^ self.device_id.is_some()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PeerAddManualParams {
    pub address: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PeerUnpairParams {
    pub device_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PairingConfirmParams {
    pub accepted: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PeerConfigureParams {
    pub device_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clipboard_enabled: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TransferIdParams {
    pub id: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TransferConfirmParams {
    pub id: String,
    pub accept: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OpenPermissionSettingsParams {
    pub kind: PermissionKind,
}

pub type GetStateParams = EmptyParams;
pub type PairingStartHostParams = EmptyParams;
pub type PairingCancelHostParams = EmptyParams;
pub type ReturnHomeParams = EmptyParams;
pub type AppShutdownParams = EmptyParams;
pub type TransferCancelParams = TransferIdParams;
pub type PermissionsOpenSettingsParams = OpenPermissionSettingsParams;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PairingStartHostResult {
    pub code: PairingCode,
    pub expires_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PairingJoinResult {
    pub device_id: String,
}
