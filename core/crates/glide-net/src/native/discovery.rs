//! Optional mDNS discovery for nearby Glide daemons.
//!
//! Advertisements are hints only. The caller must authenticate peers over the
//! pairing or pinned QUIC path before trusting their identity.

use std::{
    collections::HashMap,
    net::{SocketAddr, SocketAddrV4, SocketAddrV6},
};

use glide_platform::Os;
use glide_proto::ipc::DiscoveredPeer;
use mdns_sd::{IfKind, ScopedIp, ServiceDaemon, ServiceEvent, ServiceInfo};
use tokio::{sync::mpsc, task::JoinHandle};

use super::NativeError;

const SERVICE_TYPE: &str = "_glide._udp.local.";
const MAX_NAME_BYTES: usize = 128;

/// An unauthenticated peer advertisement entering the native peer manager.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DiscoveryUpdate {
    /// A nearby endpoint was resolved. Its identity is still untrusted.
    Found(DiscoveredPeer),
    /// The corresponding mDNS service is no longer visible.
    Removed(String),
}

/// RAII owner for one mDNS advertisement and browse task.
pub struct Discovery {
    daemon: ServiceDaemon,
    service_fullname: String,
    browse_task: JoinHandle<()>,
}

impl Discovery {
    pub fn update(
        &self,
        device_id: &str,
        name: &str,
        os: Os,
        port: u16,
    ) -> Result<(), NativeError> {
        validate_metadata(device_id, name, port)
            .map_err(|_| NativeError::Internal("invalid mDNS metadata".into()))?;
        let service = advertisement(device_id, name, os, port)
            .map_err(|_| NativeError::Internal("mDNS update preparation failed".into()))?;
        self.daemon
            .register(service)
            .map_err(|_| NativeError::Internal("mDNS update failed".into()))
    }
    /// Advertise this device and browse nearby `_glide._udp.local.` services.
    ///
    /// The output channel should be bounded. Discovery can block asynchronously
    /// when its consumer is behind; dropping this owner cancels the task and
    /// unregisters the local service.
    pub fn start(
        device_id: &str,
        name: &str,
        os: Os,
        port: u16,
        tx: mpsc::Sender<DiscoveryUpdate>,
    ) -> Result<Self, NativeError> {
        validate_metadata(device_id, name, port)
            .map_err(|_| NativeError::Internal("invalid mDNS advertisement metadata".into()))?;
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| {
            NativeError::Internal("mDNS discovery requires an active Tokio runtime".into())
        })?;

        let daemon = ServiceDaemon::new()
            .map_err(|_| NativeError::Internal("failed to initialize mDNS discovery".into()))?;
        // mdns-sd enables loopback interfaces by default since 0.17. Keep the
        // earlier default that released Glide versions use: advertise and
        // browse on real network interfaces only.
        if daemon
            .disable_interface(vec![IfKind::LoopbackV4, IfKind::LoopbackV6])
            .is_err()
        {
            let _ = daemon.shutdown();
            return Err(NativeError::Internal(
                "failed to configure mDNS interfaces".into(),
            ));
        }
        let service = match advertisement(device_id, name, os, port) {
            Ok(service) => service,
            Err(_) => {
                let _ = daemon.shutdown();
                return Err(NativeError::Internal(
                    "failed to create mDNS advertisement".into(),
                ));
            }
        };
        let service_fullname = service.get_fullname().to_owned();
        let receiver = match daemon.browse(SERVICE_TYPE) {
            Ok(receiver) => receiver,
            Err(_) => {
                let _ = daemon.shutdown();
                return Err(NativeError::Internal(
                    "failed to browse mDNS services".into(),
                ));
            }
        };
        if daemon.register(service).is_err() {
            let _ = daemon.shutdown();
            return Err(NativeError::Internal(
                "failed to register mDNS advertisement".into(),
            ));
        }

        let self_id = device_id.to_owned();
        let browse_task = runtime.spawn(async move {
            let mut services = HashMap::<String, String>::new();
            while let Ok(event) = receiver.recv_async().await {
                match event {
                    ServiceEvent::ServiceResolved(service) => {
                        if let Some(peer) = parse_service(&service, &self_id) {
                            let fullname = service.fullname.to_ascii_lowercase();
                            if !services.contains_key(&fullname)
                                && services.len() >= super::MAX_PEERS
                            {
                                continue;
                            }
                            services.insert(fullname, peer.device_id.clone());
                            if tx.send(DiscoveryUpdate::Found(peer)).await.is_err() {
                                break;
                            }
                        }
                    }
                    ServiceEvent::ServiceRemoved(_, fullname) => {
                        match services.remove(&fullname.to_ascii_lowercase()) {
                            Some(device_id)
                                if !services.values().any(|seen_id| seen_id == &device_id) =>
                            {
                                if tx.send(DiscoveryUpdate::Removed(device_id)).await.is_err() {
                                    break;
                                }
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
        });

        Ok(Self {
            daemon,
            service_fullname,
            browse_task,
        })
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        self.browse_task.abort();
        let _ = self.daemon.unregister(&self.service_fullname);
        let _ = self.daemon.shutdown();
    }
}

/// Build the advertised service. The instance name, host name, service type
/// and TXT keys are part of the cross-version discovery contract: older and
/// newer Glide releases must keep finding each other, so never change them.
/// The caller must have validated the metadata (`device_id` is 64 hex digits).
fn advertisement(
    device_id: &str,
    name: &str,
    os: Os,
    port: u16,
) -> Result<ServiceInfo, mdns_sd::Error> {
    let port_text = port.to_string();
    let properties = [
        ("device_id", device_id),
        ("name", name),
        ("os", os_name(os)),
        ("port", port_text.as_str()),
    ];
    Ok(ServiceInfo::new(
        SERVICE_TYPE,
        &format!("glide-{}", &device_id[..32]),
        &format!("{}.local.", &device_id[..32]),
        "",
        port,
        &properties[..],
    )?
    .enable_addr_auto())
}

fn validate_metadata(device_id: &str, name: &str, port: u16) -> Result<(), ()> {
    if device_id.len() != 64
        || !device_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || name.is_empty()
        || name.len() > MAX_NAME_BYTES
        || name.trim().is_empty()
        || name.chars().any(char::is_control)
        || port == 0
    {
        return Err(());
    }
    Ok(())
}

fn os_name(os: Os) -> &'static str {
    match os {
        Os::Windows => "windows",
        Os::Macos => "macos",
    }
}

fn parse_service(service: &mdns_sd::ResolvedService, self_id: &str) -> Option<DiscoveredPeer> {
    if !service.is_valid() || !service.ty_domain.eq_ignore_ascii_case(SERVICE_TYPE) {
        return None;
    }

    let properties = &service.txt_properties;
    let device_id = properties.get_property_val_str("device_id")?;
    let name = properties.get_property_val_str("name")?;
    let os = match properties.get_property_val_str("os")? {
        "windows" => Os::Windows,
        "macos" => Os::Macos,
        _ => return None,
    };
    let advertised_port = properties
        .get_property_val_str("port")?
        .parse::<u16>()
        .ok()?;
    if validate_metadata(device_id, name, advertised_port).is_err()
        || advertised_port != service.port
        || device_id == self_id
    {
        return None;
    }

    let address = service
        .addresses
        .iter()
        .filter_map(|ip| format_address(ip, service.port))
        .min()?;
    Some(DiscoveredPeer {
        device_id: device_id.to_owned(),
        name: name.to_owned(),
        os,
        address,
    })
}

fn format_address(ip: &ScopedIp, port: u16) -> Option<String> {
    if port == 0 {
        return None;
    }
    let ip_addr = ip.to_ip_addr();
    if ip_addr.is_unspecified() || ip_addr.is_multicast() {
        return None;
    }
    let socket = match ip {
        ScopedIp::V4(address) => {
            if *address.addr() == std::net::Ipv4Addr::BROADCAST {
                return None;
            }
            SocketAddr::V4(SocketAddrV4::new(*address.addr(), port))
        }
        ScopedIp::V6(address) => {
            let scope_id = address.scope_id().index;
            let is_link_local = (address.addr().segments()[0] & 0xffc0) == 0xfe80;
            if is_link_local && scope_id == 0 {
                return None;
            }
            SocketAddr::V6(SocketAddrV6::new(*address.addr(), port, 0, scope_id))
        }
        _ => return None,
    };
    Some(socket.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    #[test]
    fn advertisement_metadata_is_strictly_bounded() {
        let valid = "a".repeat(64);
        assert!(validate_metadata(&valid, "Glide device", 24_800).is_ok());
        assert!(validate_metadata(&valid.to_uppercase(), "Glide", 24_800).is_err());
        assert!(validate_metadata(&valid[..63], "Glide", 24_800).is_err());
        assert!(validate_metadata(&valid, &"n".repeat(129), 24_800).is_err());
        assert!(validate_metadata(&valid, "bad\nname", 24_800).is_err());
        assert!(validate_metadata(&valid, "Glide", 0).is_err());
    }

    /// Pins the advertised service format (recorded with mdns-sd 0.15.2) so
    /// that peers on older releases keep discovering this device.
    #[test]
    fn advertised_service_format_is_stable() {
        let device_id = "0123456789abcdef0123456789abcdef00112233445566778899aabbccddeeff";
        let service = advertisement(device_id, "Office PC", Os::Windows, 24_801).unwrap();
        assert_eq!(service.get_type(), "_glide._udp.local.");
        assert_eq!(
            service.get_fullname(),
            "glide-0123456789abcdef0123456789abcdef._glide._udp.local."
        );
        assert_eq!(
            service.get_hostname(),
            "0123456789abcdef0123456789abcdef.local."
        );
        assert_eq!(service.get_port(), 24_801);
        assert!(service.get_subtype().is_none());
        assert!(service.is_addr_auto());
        let properties = service
            .get_properties()
            .iter()
            .map(|property| (property.key().to_owned(), property.val_str().to_owned()))
            .collect::<Vec<_>>();
        assert_eq!(
            properties,
            [
                ("device_id", device_id),
                ("name", "Office PC"),
                ("os", "windows"),
                ("port", "24801"),
            ]
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
        );
        let mac = advertisement(device_id, "Mac", Os::Macos, 24_801).unwrap();
        assert_eq!(mac.get_property_val_str("os"), Some("macos"));
    }

    #[test]
    fn resolved_ipv4_and_ipv6_addresses_are_socket_addresses() {
        let v4 = ScopedIp::from(IpAddr::V4("192.0.2.1".parse().unwrap()));
        let v6 = ScopedIp::from(IpAddr::V6("2001:db8::1".parse().unwrap()));
        assert_eq!(
            format_address(&v4, 24_800).as_deref(),
            Some("192.0.2.1:24800")
        );
        assert_eq!(
            format_address(&v6, 24_800).as_deref(),
            Some("[2001:db8::1]:24800")
        );
        assert!(format_address(&v4, 0).is_none());
        let unspecified = ScopedIp::from(IpAddr::V4("0.0.0.0".parse().unwrap()));
        assert!(format_address(&unspecified, 24_800).is_none());
    }
}
