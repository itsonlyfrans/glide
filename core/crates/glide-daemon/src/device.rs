//! What kind of computer this is (MacBook Pro 16-inch, Mac Studio, a Windows laptop...), for its picture on the Desk.
//! Read once in the background; nothing here is sent anywhere except to paired computers.

use glide_proto::ipc::DeviceModel;

/// Picture kind from a Mac's marketing name ("MacBook Pro", "Mac Studio", ...).
pub fn mac_kind(machine_name: &str) -> &'static str {
    let name = machine_name.to_ascii_lowercase();
    if name.contains("macbook") {
        "laptop"
    } else if name.contains("studio") {
        "studio"
    } else if name.contains("mini") {
        "mini"
    } else if name.contains("imac") {
        "imac"
    } else if name.contains("mac pro") {
        "tower"
    } else {
        "desktop"
    }
}

/// A laptop screen's marketing size from its diagonal in millimetres: 14.2" is sold as 14-inch, 13.6" as 13-inch.
pub fn marketed_inches(width_mm: f64, height_mm: f64) -> Option<u32> {
    let inches = width_mm.hypot(height_mm) / 25.4;
    (inches.is_finite() && (10.0..30.0).contains(&inches)).then(|| inches.floor() as u32)
}

/// The machine name from `system_profiler SPHardwareDataType -json`.
pub fn machine_name_from_profile(json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let name = value
        .get("SPHardwareDataType")?
        .get(0)?
        .get("machine_name")?
        .as_str()?;
    let name = name.trim();
    (!name.is_empty() && name.len() <= 64 && !name.chars().any(char::is_control))
        .then(|| name.to_owned())
}

#[cfg(target_os = "macos")]
pub fn detect(monitors: &[glide_platform::Monitor]) -> Option<DeviceModel> {
    #[repr(C)]
    struct CGSize {
        width: f64,
        height: f64,
    }
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGDisplayIsBuiltin(display: u32) -> u32;
        fn CGDisplayScreenSize(display: u32) -> CGSize;
    }
    let output = std::process::Command::new("/usr/sbin/system_profiler")
        .args(["SPHardwareDataType", "-json"])
        .output()
        .ok()?;
    let machine = machine_name_from_profile(&String::from_utf8_lossy(&output.stdout))?;
    let kind = mac_kind(&machine);
    // SAFETY: plain Quartz queries with a display id from this Mac's own monitor list.
    let builtin = monitors.iter().find_map(|m| {
        let id = m.id.parse::<u32>().ok()?;
        (unsafe { CGDisplayIsBuiltin(id) } != 0)
            .then(|| (m.id.clone(), unsafe { CGDisplayScreenSize(id) }))
    });
    let name = match (&builtin, kind) {
        (Some((_, size)), "laptop") => match marketed_inches(size.width, size.height) {
            Some(inches) => format!("{machine} {inches}-inch"),
            None => machine.clone(),
        },
        _ => machine.clone(),
    };
    Some(DeviceModel {
        name,
        kind: kind.into(),
        builtin_monitor: builtin.map(|(id, _)| id),
    })
}

#[cfg(windows)]
pub fn detect(_monitors: &[glide_platform::Monitor]) -> Option<DeviceModel> {
    use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    let mut status: SYSTEM_POWER_STATUS = unsafe { std::mem::zeroed() };
    // SAFETY: fills a caller-owned struct; no pointers are retained.
    let known = unsafe { GetSystemPowerStatus(&mut status) } != 0;
    // 128 = no system battery, 255 = unknown: treat both as a desktop.
    let laptop = known && status.BatteryFlag != 128 && status.BatteryFlag != 255;
    Some(DeviceModel {
        name: if laptop {
            "Windows laptop"
        } else {
            "Windows PC"
        }
        .into(),
        kind: if laptop { "laptop" } else { "desktop" }.into(),
        builtin_monitor: None,
    })
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn detect(_monitors: &[glide_platform::Monitor]) -> Option<DeviceModel> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // Feature: the Desk draws a MacBook as a laptop with its size, and a Mac Studio or mini as what it is.
    #[test]
    fn macs_are_recognised_from_their_name_and_screen_size() {
        let profile = r#"{"SPHardwareDataType":[{"_name":"hardware_overview","machine_model":"Mac15,7","machine_name":"MacBook Pro"}]}"#;
        assert_eq!(
            machine_name_from_profile(profile).as_deref(),
            Some("MacBook Pro")
        );
        assert_eq!(mac_kind("MacBook Pro"), "laptop");
        assert_eq!(mac_kind("MacBook Air"), "laptop");
        assert_eq!(mac_kind("Mac Studio"), "studio");
        assert_eq!(mac_kind("Mac mini"), "mini");
        assert_eq!(mac_kind("iMac"), "imac");
        assert_eq!(mac_kind("Mac Pro"), "tower");
        // Real built-in panel sizes (millimetres) map to the size Apple sells them as.
        assert_eq!(marketed_inches(302.0, 196.0), Some(14)); // 14.2"
        assert_eq!(marketed_inches(345.0, 223.0), Some(16)); // 16.2"
        assert_eq!(marketed_inches(294.0, 191.0), Some(13)); // 13.6" Air
        assert_eq!(marketed_inches(331.0, 214.0), Some(15)); // 15.3" Air
        assert_eq!(marketed_inches(0.0, 0.0), None);
        assert_eq!(machine_name_from_profile("not json"), None);
    }
}
