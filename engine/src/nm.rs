use std::{collections::HashMap, net::Ipv4Addr, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use tokio::time::{sleep, timeout};
use zbus::{
    Connection, Proxy,
    proxy::CacheProperties,
    zvariant::{OwnedObjectPath, OwnedValue, Value},
};

use crate::wfd;

const DEST: &str = "org.freedesktop.NetworkManager";
const ROOT: &str = "/org/freedesktop/NetworkManager";
const DEVICE: &str = "org.freedesktop.NetworkManager.Device";
const P2P: &str = "org.freedesktop.NetworkManager.Device.WifiP2P";
const ACTIVE: &str = "org.freedesktop.NetworkManager.Connection.Active";

async fn proxy<'a>(conn: &'a Connection, path: &'a str, interface: &'a str) -> Result<Proxy<'a>> {
    Ok(zbus::proxy::Builder::new(conn)
        .destination(DEST)?
        .path(path)?
        .interface(interface)?
        .cache_properties(CacheProperties::No)
        .build()
        .await?)
}

#[derive(Clone, Debug, Serialize)]
pub struct Peer {
    pub mac: String,
    pub name: String,
    pub model: String,
    pub strength: u8,
    pub wfd: bool,
    pub adapter: String,
    #[serde(skip)]
    pub device_path: String,
}

#[derive(Clone, Copy, Debug)]
pub struct Network {
    pub address: Ipv4Addr,
    pub prefix: u32,
}

impl Network {
    pub fn contains_peer(&self, ip: Ipv4Addr) -> bool {
        if self.prefix == 0 || self.prefix > 30 {
            return false;
        }
        let mask = u32::MAX << (32 - self.prefix);
        let net = u32::from(self.address) & mask;
        let other = u32::from(ip);
        other != u32::from(self.address)
            && other & mask == net
            && other != net
            && other != (net | !mask)
    }
}

pub struct Manager {
    pub connection: Connection,
    active: Option<OwnedObjectPath>,
    device: Option<OwnedObjectPath>,
    wifi: Vec<(String, OwnedObjectPath)>,
}

impl Manager {
    pub async fn new() -> Result<Self> {
        let connection = Connection::system()
            .await
            .context("No se puede acceder al bus de sistema")?;
        Ok(Self {
            connection,
            active: None,
            device: None,
            wifi: Vec::new(),
        })
    }

    async fn devices(&self) -> Result<Vec<OwnedObjectPath>> {
        Ok(proxy(&self.connection, ROOT, DEST)
            .await?
            .call("GetDevices", &())
            .await?)
    }

    pub async fn snapshot_wifi(&mut self) -> Result<()> {
        for path in self.devices().await? {
            let dev = proxy(&self.connection, path.as_str(), DEVICE).await?;
            if dev.get_property::<u32>("DeviceType").await? == 2
                && dev.get_property::<u32>("State").await? == 100
            {
                self.wifi.push((
                    path.to_string(),
                    dev.get_property("ActiveConnection").await?,
                ));
            }
        }
        Ok(())
    }

    pub async fn wifi_unchanged(&self) -> Result<bool> {
        for (path, active) in &self.wifi {
            let dev = proxy(&self.connection, path, DEVICE).await?;
            if dev.get_property::<u32>("State").await? != 100
                || dev
                    .get_property::<OwnedObjectPath>("ActiveConnection")
                    .await?
                    != *active
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub async fn peers(&self, scan: bool) -> Result<Vec<Peer>> {
        let mut devices = Vec::new();
        for path in self.devices().await? {
            let dev = proxy(&self.connection, path.as_str(), DEVICE).await?;
            if dev.get_property::<u32>("DeviceType").await? == 30 {
                devices.push((
                    path.to_string(),
                    dev.get_property::<String>("Interface").await?,
                ));
            }
        }
        ensure!(
            !devices.is_empty(),
            "NetworkManager no ofrece un dispositivo Wi-Fi Direct"
        );
        if scan {
            for (path, _) in &devices {
                let options = HashMap::from([("timeout", Value::from(8i32))]);
                let _: () = proxy(&self.connection, path, P2P)
                    .await?
                    .call("StartFind", &(options,))
                    .await
                    .context("No se pudo buscar receptores Wi-Fi Direct")?;
            }
            // NetworkManager ends this bounded scan itself. Do not StopFind a
            // discovery that another application might still need.
            sleep(Duration::from_secs(8)).await;
        }
        let mut result = Vec::new();
        for (path, interface) in &devices {
            let peers: Vec<OwnedObjectPath> = proxy(&self.connection, path, P2P)
                .await?
                .get_property("Peers")
                .await?;
            for peer_path in peers.into_iter().take(64) {
                let peer = proxy(
                    &self.connection,
                    peer_path.as_str(),
                    "org.freedesktop.NetworkManager.WifiP2PPeer",
                )
                .await?;
                let mac: String = match peer.get_property("HwAddress").await {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if !valid_mac(&mac) {
                    continue;
                }
                let wfd_ies: Vec<u8> = peer.get_property("WfdIEs").await.unwrap_or_default();
                let name: String = peer.get_property("Name").await.unwrap_or_default();
                let model: String = peer.get_property("Model").await.unwrap_or_default();
                result.push(Peer {
                    mac: mac.to_ascii_uppercase(),
                    name: clean_name(&name),
                    model: clean_name(&model),
                    strength: peer
                        .get_property::<u8>("Strength")
                        .await
                        .unwrap_or(0)
                        .min(100),
                    wfd: wfd::available_sink(&wfd_ies),
                    adapter: interface.clone(),
                    device_path: path.clone(),
                });
            }
        }
        result.sort_by(|a, b| b.wfd.cmp(&a.wfd).then_with(|| b.strength.cmp(&a.strength)));
        result.dedup_by(|a, b| a.mac == b.mac && a.device_path == b.device_path);
        Ok(result)
    }

    pub async fn begin(&mut self, peer: &Peer) -> Result<()> {
        ensure!(
            valid_mac(&peer.mac) && peer.wfd,
            "El dispositivo no anuncia Wi-Fi Display"
        );
        let settings: HashMap<&str, HashMap<&str, Value<'_>>> = HashMap::from([
            (
                "connection",
                HashMap::from([
                    ("id", Value::from("Omarchy Miracast (temporal)")),
                    ("type", Value::from("wifi-p2p")),
                    ("autoconnect", Value::from(false)),
                ]),
            ),
            (
                "wifi-p2p",
                HashMap::from([
                    ("peer", Value::from(peer.mac.as_str())),
                    ("wfd-ies", Value::from(wfd::source_ies())),
                ]),
            ),
            (
                "ipv4",
                HashMap::from([
                    ("method", Value::from("auto")),
                    ("never-default", Value::from(true)),
                    ("ignore-auto-dns", Value::from(true)),
                    ("may-fail", Value::from(false)),
                ]),
            ),
            ("ipv6", HashMap::from([("method", Value::from("disabled"))])),
        ]);
        let options = HashMap::from([
            ("persist", Value::from("volatile")),
            ("bind-activation", Value::from("dbus-client")),
        ]);
        let device = OwnedObjectPath::try_from(peer.device_path.clone())?;
        self.device = Some(device.clone());
        let specific = OwnedObjectPath::try_from("/")?;
        let (_, active, _): (
            OwnedObjectPath,
            OwnedObjectPath,
            HashMap<String, OwnedValue>,
        ) = proxy(&self.connection, ROOT, DEST)
            .await?
            .call(
                "AddAndActivateConnection2",
                &(settings, device, specific, options),
            )
            .await
            .context(
                "NetworkManager rechazó la conexión P2P; puede requerir autorización de Polkit",
            )?;
        self.active = Some(active);
        Ok(())
    }

    pub async fn network_if_ready(&self) -> Result<Option<Network>> {
        let path = self
            .active
            .as_ref()
            .context("No hay una activación propia")?;
        let active = proxy(&self.connection, path.as_str(), ACTIVE).await?;
        match active.get_property::<u32>("State").await.context("NetworkManager dejó de exponer la activación Wi-Fi Direct; el enlace puede haberse cerrado")? {
            1 => return Ok(None),
            2 => {}
            _ => {
                // StateReason belongs to Device, not Connection.Active.
                let detail = if let Some(path) = &self.device {
                    match proxy(&self.connection,path.as_str(),DEVICE).await {
                        Ok(device) => device.get_property::<(u32,u32)>("StateReason").await.ok(),
                        Err(_) => None,
                    }
                } else { None };
                if let Some((state,reason)) = detail {
                    bail!("El enlace Wi-Fi Direct falló o se cerró (estado del dispositivo {state}, motivo NM {reason})");
                }
                bail!("El enlace Wi-Fi Direct falló o se cerró; NetworkManager ya no expone el motivo");
            }
        }
        let ip_path: OwnedObjectPath = active.get_property("Ip4Config").await?;
        if ip_path.as_str() == "/" {
            return Ok(None);
        }
        let ip = proxy(
            &self.connection,
            ip_path.as_str(),
            "org.freedesktop.NetworkManager.IP4Config",
        )
        .await?;
        let addresses: Vec<HashMap<String, OwnedValue>> = ip.get_property("AddressData").await?;
        for entry in addresses {
            let Some(address) = entry
                .get("address")
                .and_then(|v| <&str>::try_from(v).ok())
                .and_then(|s| s.parse::<Ipv4Addr>().ok())
            else {
                continue;
            };
            let prefix = entry
                .get("prefix")
                .and_then(|v| u32::try_from(v).ok())
                .unwrap_or(0);
            if !address.is_link_local() && !address.is_loopback() && prefix > 0 && prefix <= 30 {
                return Ok(Some(Network { address, prefix }));
            }
        }
        Ok(None)
    }

    pub async fn close(mut self) -> Result<()> {
        // Only deactivate the volatile activation created by THIS D-Bus client.
        if let Some(active) = self.active.take() {
            let operation = async {
                let _: () = proxy(&self.connection, ROOT, DEST)
                    .await?
                    .call("DeactivateConnection", &(active,))
                    .await?;
                Ok::<(), anyhow::Error>(())
            };
            let _ = timeout(Duration::from_secs(3), operation).await;
        }
        // bind-activation also handles process crashes and calls cancelled
        // before their reply: the dedicated D-Bus connection goes away.
        self.connection.close().await?;
        Ok(())
    }
}

pub fn valid_mac(value: &str) -> bool {
    let parts: Vec<_> = value.split(':').collect();
    parts.len() == 6
        && parts
            .iter()
            .all(|p| p.len() == 2 && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn clean_name(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_control())
        .take(120)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn peer_must_be_on_our_group_not_broadcast_or_self() {
        let net = Network {
            address: Ipv4Addr::new(192, 168, 49, 1),
            prefix: 24,
        };
        for ip in ["192.168.49.2", "192.168.49.200"] {
            assert!(net.contains_peer(ip.parse().unwrap()));
        }
        for ip in [
            "192.168.49.1",
            "192.168.49.0",
            "192.168.49.255",
            "192.168.1.2",
        ] {
            assert!(!net.contains_peer(ip.parse().unwrap()));
        }
    }
    #[test]
    fn input_mac_and_names_are_bounded() {
        assert!(valid_mac("BE:7E:8B:BE:29:2E"));
        assert!(!valid_mac("BE:7E:8B:BE:29:2E;reboot"));
        assert_eq!(clean_name("TV\n\u{1b}"), "TV");
        assert_eq!(clean_name(&"x".repeat(1000)).len(), 120);
    }
}
