//! Reaching the phone: the CoreDevice tunnel over Wi-Fi or USB, and mounting the
//! developer disk image that provides the screen and input services.

use anyhow::{Context, Result, bail};
use idevice::{
    IdeviceService,
    core_device_proxy::CoreDeviceProxy,
    lockdown::LockdownClient,
    mobile_image_mounter::ImageMounter,
    rsd::RsdHandshake,
    tcp::handle::AdapterHandle,
    usbmuxd::{UsbmuxdAddr, UsbmuxdConnection, UsbmuxdDevice},
};

pub type Tunnel = (AdapterHandle, RsdHandshake);

pub async fn open_tunnel(usb: bool, udid: Option<&str>) -> Result<Tunnel> {
    if usb {
        open_usb_tunnel(udid).await
    } else {
        crate::wifi::open_tunnel().await
    }
}

async fn mux() -> Result<UsbmuxdConnection> {
    UsbmuxdConnection::default().await.context(
        "no usbmuxd listening on 127.0.0.1:27015 — install Apple's \"Apple Devices\" app \
         (or iTunes) so Windows has the iPhone USB driver, then plug the phone in",
    )
}

/// Lists iPhones attached over USB.
pub async fn usb_devices() -> Result<Vec<UsbmuxdDevice>> {
    Ok(mux().await?.get_devices().await?)
}

/// Opens the userspace CoreDevice tunnel over USB and performs the RSD handshake.
async fn open_usb_tunnel(udid: Option<&str>) -> Result<Tunnel> {
    let mut mux = mux().await?;
    let device = match udid {
        Some(udid) => mux.get_device(udid).await?,
        None => mux
            .get_devices()
            .await?
            .into_iter()
            .next()
            .context("no iPhone found — plug it in, unlock it and tap Trust")?,
    };
    let provider = device.to_provider(UsbmuxdAddr::default(), "ilinkos");
    let proxy = CoreDeviceProxy::connect(&provider)
        .await
        .context("could not start CoreDeviceProxy (needs iOS 17.4+, unlocked and trusted)")?;
    let rsd_port = proxy.tunnel_info().server_rsd_port;
    let mut handle = proxy.create_software_tunnel()?.to_async_handle();
    let stream = handle.connect(rsd_port).await?;
    let handshake = RsdHandshake::new(stream).await?;
    Ok((handle, handshake))
}

/// Mounts the developer disk image from `<state dir>/ddi`. Returns what happened.
pub async fn mount((mut handle, mut rsd): Tunnel) -> Result<&'static str> {
    let dir = crate::wifi::state_dir()?.join("ddi");
    let read = |name: &str| {
        std::fs::read(dir.join(name)).with_context(|| format!("missing {}", dir.join(name).display()))
    };
    let image = read("Image.dmg")?;
    let trust_cache = read("Image.dmg.trustcache")?;
    let manifest = read("BuildManifest.plist")?;

    let mut lockdown: LockdownClient = rsd.connect(&mut handle).await?;
    let chip_id = lockdown
        .get_value(Some("UniqueChipID"), None)
        .await?
        .as_unsigned_integer()
        .context("device did not report a UniqueChipID")?;

    let mut mounter: ImageMounter = rsd.connect(&mut handle).await?;
    if !mounter.query_developer_mode_status().await? {
        bail!("Developer Mode is off — enable it in Settings > Privacy & Security > Developer Mode");
    }
    if !mounter.copy_devices().await?.is_empty() {
        return Ok("a developer disk image is already mounted");
    }
    mounter
        .mount_personalized_rsd(&mut handle, &mut rsd, image, trust_cache, &manifest, None, chip_id)
        .await
        .context("mount failed")?;
    Ok("developer disk image mounted")
}
