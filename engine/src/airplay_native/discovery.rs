use std::{collections::BTreeMap, net::SocketAddr, time::Duration};

use anyhow::{Context, Result};
use mdns_sd::{ServiceDaemon, ServiceEvent};

const SERVICE: &str = "_airplay._tcp.local.";

#[derive(Clone, Debug)]
pub struct Device {
    pub name: String,
    pub addresses: Vec<SocketAddr>,
    pub device_id: Option<String>,
    pub model: Option<String>,
    pub public_key: Option<String>,
    pub features: u64,
    pub txt: BTreeMap<String, String>,
}

/// Browse until the deadline. The daemon is stopped even if the caller receives no results.
pub async fn discover(window: Duration) -> Result<Vec<Device>> {
    let daemon = ServiceDaemon::new().context("starting mDNS")?;
    let events = daemon.browse(SERVICE).context("browsing AirPlay")?;
    let deadline = tokio::time::Instant::now() + window;
    let mut found = BTreeMap::new();
    while let Ok(Ok(event)) = tokio::time::timeout_at(deadline, events.recv_async()).await {
        match event {
            ServiceEvent::ServiceResolved(service) => {
                let mut addresses: Vec<SocketAddr> = service
                    .addresses
                    .iter()
                    .filter_map(|ip| {
                        let s = ip.to_string();
                        format!("[{s}]:{}", service.port)
                            .parse()
                            .ok()
                            .or_else(|| format!("{s}:{}", service.port).parse().ok())
                    })
                    .collect();
                addresses.sort_by_key(|a| (a.is_ipv6(), *a));
                if addresses.is_empty() {
                    continue;
                }
                let txt: BTreeMap<String, String> = service
                    .txt_properties
                    .iter()
                    .map(|p| (p.key().to_ascii_lowercase(), p.val_str().to_owned()))
                    .collect();
                let get = |key: &str| txt.get(key).cloned();
                let features = get("features").as_deref().map(parse_features).unwrap_or(0);
                let name = service
                    .fullname
                    .strip_suffix(&format!(".{SERVICE}"))
                    .unwrap_or(&service.fullname)
                    .to_owned();
                found.insert(
                    service.fullname,
                    Device {
                        name,
                        addresses,
                        device_id: get("deviceid"),
                        model: get("model"),
                        public_key: get("pk"),
                        features,
                        txt,
                    },
                );
            }
            ServiceEvent::ServiceRemoved(_, fullname) => {
                found.remove(&fullname);
            }
            _ => {}
        }
    }
    let _ = daemon.stop_browse(SERVICE);
    let _ = daemon.shutdown();
    Ok(found.into_values().collect())
}

fn parse_features(value: &str) -> u64 {
    fn parse(v: &str) -> Option<u64> {
        let v = v.trim();
        if let Some(v) = v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")) {
            u64::from_str_radix(v, 16).ok()
        } else {
            v.parse().ok()
        }
    }
    let mut parts = value.split(',');
    let low = parts.next().and_then(parse).unwrap_or(0) & 0xffff_ffff;
    let high = parts.next().and_then(parse).unwrap_or(0) & 0xffff_ffff;
    if value.contains(',') {
        low | high << 32
    } else {
        parse(value).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn feature_words() {
        assert_eq!(parse_features("0x1234,0x2"), 0x2_0000_1234);
        assert_eq!(parse_features("0x100000000"), 0x1_0000_0000);
    }
}
