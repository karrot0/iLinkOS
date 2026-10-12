//! Cable-free transport: phone-initiated pairing and the Wi-Fi CoreDevice tunnel.

use std::{
    net::{IpAddr, SocketAddr, SocketAddrV6},
    path::PathBuf,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use idevice::{
    remote_pairing::{
        PAIRABLE_HOST_SERVICE_TYPE, PairableHost, PairableHostInfo, PeerDevice,
        RemotePairingClient, RpPairingFile, RpPairingSocket, connect_tls_psk_tunnel_native,
    },
    rsd::RsdHandshake,
    tcp::{adapter::Adapter, handle::AdapterHandle},
};
use mdns_sd::{ScopedIp, ServiceDaemon, ServiceEvent, ServiceInfo};
use tokio::net::{TcpListener, TcpStream};

const REMOTE_PAIRING_SERVICE_TYPE: &str = "_remotepairing._tcp.local.";
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(15);

/// Apple's pairing service names exceed the 15-byte DNS-SD limit that mdns-sd
/// enforces by default; without raising it, registration is silently dropped.
fn daemon() -> Result<ServiceDaemon> {
    let mdns = ServiceDaemon::new()?;
    mdns.set_service_name_len_max(30)?;
    Ok(mdns)
}

/// The address of the interface that reaches the local network, so we don't
/// advertise loopback or virtual adapters (WSL, Hyper-V) the phone can't reach.
fn lan_ip() -> Result<IpAddr> {
    // Connecting a UDP socket sends nothing; it only makes Windows pick the route.
    // The route out to the internet is the one the phone shares. (The multicast
    // route is no guide: Windows may hand it to the loopback adapter.)
    for target in ["192.0.2.1:9", "224.0.0.251:5353"] {
        let probe = std::net::UdpSocket::bind("0.0.0.0:0")?;
        if probe.connect(target).is_ok() {
            let ip = probe.local_addr()?.ip();
            if !ip.is_loopback() && !ip.is_unspecified() {
                return Ok(ip);
            }
        }
    }
    anyhow::bail!("this PC is not on a network the iPhone could reach")
}

pub fn host_name() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "ilinkos".into())
}

pub fn state_dir() -> Result<PathBuf> {
    let roaming = PathBuf::from(std::env::var("APPDATA").context("APPDATA is not set")?);
    let dir = roaming.join("ilinkos");
    let old = roaming.join("tether");
    if !dir.exists() && old.exists() {
        std::fs::rename(&old, &dir)
            .context("could not move the data folder to iLinkOS — quit and try again")?;
    }
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn pairing_path() -> Result<PathBuf> {
    Ok(state_dir()?.join("pairing.plist"))
}

/// Our mDNS identity key, persisted so an already-paired phone keeps recognising this PC.
fn host_info() -> Result<PairableHostInfo> {
    let path = state_dir()?.join("host_irk.bin");
    let mut info = PairableHostInfo::generate(host_name(), "Mac17,7");
    match std::fs::read(&path) {
        Ok(bytes) => {
            info.alt_irk = bytes
                .try_into()
                .map_err(|_| anyhow!("{} is corrupt", path.display()))?
        }
        Err(_) => std::fs::write(&path, info.alt_irk)?,
    }
    Ok(info)
}

pub async fn pair() -> Result<()> {
    println!("Advertising this PC as \"{}\".", host_name());
    println!("On the iPhone: Settings > Privacy & Security > Developer Mode, then choose it.");
    let phone = pair_with(|pin| println!("\n    Enter this code on the iPhone:  {pin}\n")).await?;
    println!("Paired with {phone}.");
    Ok(())
}

/// Advertises this PC, waits for the phone to start pairing and hands the PIN the
/// user must type on the phone to `on_pin`. Returns the phone's name and model.
pub async fn pair_with(on_pin: impl FnOnce(String)) -> Result<String> {
    let path = pairing_path()?;
    let mut pairing_file = match RpPairingFile::read_from_file(&path).await {
        Ok(f) => f,
        Err(_) => RpPairingFile::generate(&host_name()),
    };
    let info = host_info()?;

    // IPv4: a `[::]` listener is IPv6-only on Windows.
    let listener = TcpListener::bind("0.0.0.0:0").await?;
    let port = listener.local_addr()?.port();

    let mdns = daemon()?;
    let txt = info.mdns_txt_records(pairing_file.identifier());
    let txt: Vec<(&str, &str)> = txt.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let service = ServiceInfo::new(
        PAIRABLE_HOST_SERVICE_TYPE,
        pairing_file.identifier(),
        &format!("{}.local.", host_name()),
        lan_ip()?,
        port,
        &txt[..],
    )?;
    mdns.register(service)?;

    let (stream, _) = listener.accept().await?;
    let mut host = PairableHost::new(RpPairingSocket::new_device(stream), info);
    let peer = host
        .accept(&mut pairing_file, |pin| async move { on_pin(pin) })
        .await
        .context("pairing failed")?;

    pairing_file.write_to_file(&path).await?;
    let _ = mdns.shutdown();
    Ok(format!("{} ({})", peer.name, peer.model))
}

/// PC-initiated pairing: connects out to the phone, which shows a consent prompt.
/// Needs no inbound connection, so it works where a firewall blocks [`pair`].
pub async fn pair_direct(addr: SocketAddr) -> Result<()> {
    let path = pairing_path()?;
    let mut pairing_file = match RpPairingFile::read_from_file(&path).await {
        Ok(f) => f,
        Err(_) => RpPairingFile::generate(&host_name()),
    };
    let stream = connect_any(&[addr]).await?;
    println!("Connected to {addr}. Unlock the device and accept the pairing prompt if one appears...");
    let mut client = RemotePairingClient::new(RpPairingSocket::new(stream), &host_name());
    client
        .connect(&mut pairing_file, || async {
            println!("Enter the code shown on the device:");
            let mut line = String::new();
            let _ = std::io::stdin().read_line(&mut line);
            line.trim().to_string()
        })
        .await
        .context("pairing failed")?;
    pairing_file.write_to_file(&path).await?;
    match client.paired_peer_device() {
        Ok(peer) => println!("Paired with {} ({}).", peer.name, peer.model),
        Err(_) => println!("Paired."),
    }
    println!("Pairing saved to {}", path.display());
    Ok(())
}

/// Lists Apple pairing-related services visible on the local network.
pub async fn scan() -> Result<()> {
    let mdns = daemon()?;
    let types = [
        REMOTE_PAIRING_SERVICE_TYPE,
        PAIRABLE_HOST_SERVICE_TYPE,
        "_remotepairing-manual-pairing._tcp.local.",
        "_apple-mobdev2._tcp.local.",
    ];
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    for ty in types {
        let events = mdns.browse(ty)?;
        let tx = tx.clone();
        tokio::spawn(async move {
            while let Ok(event) = events.recv_async().await {
                if let ServiceEvent::ServiceResolved(svc) = event {
                    let _ = tx.send(svc);
                }
            }
        });
    }
    let mut count = 0;
    let _ = tokio::time::timeout(Duration::from_secs(6), async {
        while let Some(svc) = rx.recv().await {
            count += 1;
            let addrs: Vec<_> = svc.addresses.iter().map(|a| a.to_ip_addr()).collect();
            println!("{}  port {}  {:?}", svc.fullname, svc.port, addrs);
        }
    })
    .await;
    let _ = mdns.shutdown();
    println!("{count} service(s) found");
    Ok(())
}

/// Browses for the paired phone's `_remotepairing._tcp` service.
async fn discover(alt_irk: &[u8]) -> Result<Vec<SocketAddr>> {
    let mdns = ServiceDaemon::new()?;
    let events = mdns.browse(REMOTE_PAIRING_SERVICE_TYPE)?;
    let found = tokio::time::timeout(DISCOVERY_TIMEOUT, async {
        loop {
            let ServiceEvent::ServiceResolved(svc) = events.recv_async().await.ok()? else {
                continue;
            };
            let identifier = svc.txt_properties.get_property_val_str("identifier");
            let auth_tag = svc.txt_properties.get_property_val_str("authTag");
            let (Some(identifier), Some(auth_tag)) = (identifier, auth_tag) else {
                continue;
            };
            if !PeerDevice::validate_auth_tag(alt_irk, identifier, auth_tag) {
                continue;
            }
            // IPv4 first: link-local IPv6 needs a scope and is fussier on Windows.
            let mut addrs: Vec<SocketAddr> = svc
                .addresses
                .iter()
                .map(|ip| match ip {
                    ScopedIp::V6(v6) => SocketAddr::V6(SocketAddrV6::new(
                        *v6.addr(),
                        svc.port,
                        0,
                        v6.scope_id().index,
                    )),
                    other => SocketAddr::new(other.to_ip_addr(), svc.port),
                })
                .collect();
            addrs.sort_by_key(|a| a.is_ipv6());
            return Some(addrs);
        }
    })
    .await;
    let _ = mdns.shutdown();
    match found {
        Ok(Some(addrs)) => Ok(addrs),
        _ => bail!(
            "the paired iPhone was not found on this network within {}s — make sure it is \
             unlocked and on the same Wi-Fi as this PC",
            DISCOVERY_TIMEOUT.as_secs()
        ),
    }
}

async fn connect_any(addrs: &[SocketAddr]) -> Result<TcpStream> {
    let mut last = None;
    for addr in addrs {
        match tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(addr)).await {
            Ok(Ok(stream)) => return Ok(stream),
            Ok(Err(e)) => last = Some(anyhow!("{addr}: {e}")),
            Err(_) => last = Some(anyhow!("{addr}: timed out")),
        }
    }
    Err(last.unwrap_or_else(|| anyhow!("the phone advertised no addresses")))
}

/// Opens the userspace CoreDevice tunnel over Wi-Fi and performs the RSD handshake.
pub async fn open_tunnel() -> Result<(AdapterHandle, RsdHandshake)> {
    let path = pairing_path()?;
    let mut pairing_file = RpPairingFile::read_from_file(&path)
        .await
        .map_err(|_| anyhow!("not paired yet — run `ilinkos pair` first"))?;
    let alt_irk = pairing_file
        .alt_irk()
        .context("pairing file has no device key — run `ilinkos pair` again")?
        .to_vec();

    let addrs = discover(&alt_irk).await?;
    let stream = connect_any(&addrs).await?;
    let mut tunnel_addr = stream.peer_addr()?;

    let mut client = RemotePairingClient::new(RpPairingSocket::new(stream), &host_name());
    client
        .connect(&mut pairing_file, || async {
            // Only reached if the phone no longer accepts our stored pairing.
            String::from("000000")
        })
        .await
        .context("the phone rejected the stored pairing — run `ilinkos pair` again")?;

    tunnel_addr.set_port(client.create_tcp_listener().await?);
    let stream = TcpStream::connect(tunnel_addr).await?;
    let tunnel = connect_tls_psk_tunnel_native(stream, client.encryption_key()).await?;

    let info = tunnel.info.clone();
    let mut adapter = Adapter::new(
        Box::new(tunnel.into_inner()),
        info.client_address.parse::<IpAddr>()?,
        info.server_address.parse::<IpAddr>()?,
    );
    adapter.set_mss((info.mtu as usize).saturating_sub(60));
    let mut handle = adapter.to_async_handle();
    let rsd = RsdHandshake::new(handle.connect(info.server_rsd_port).await?).await?;
    Ok((handle, rsd))
}
