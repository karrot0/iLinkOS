//! Command-line front end for iLinkOS.

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use ilinkos::{control, daemon, device, mcp, serve, wifi};

const HID_SERVICE: &str = "com.apple.coredevice.hid.universalhidservice";
const INDIGO_HID_SERVICE: &str = "com.apple.coredevice.hid.indigo";
const DISPLAY_SERVICE: &str = "com.apple.coredevice.displayservice";

#[derive(Parser)]
#[command(about = "Control an iPhone from this PC")]
struct Cli {
    /// UDID of the device to use over USB (defaults to the first one found)
    #[arg(long, global = true)]
    udid: Option<String>,
    /// Connect over a USB cable instead of Wi-Fi
    #[arg(long, global = true)]
    usb: bool,
    /// Drive a virtual iPhone (an Inferno emulator) through its VNC display at
    /// `host:port`, instead of a real phone
    #[arg(long, global = true)]
    vm: Option<String>,
    /// Print protocol-level debug logs
    #[arg(short, long, global = true)]
    verbose: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Pair with an iPhone over Wi-Fi (iOS 27+): the phone initiates, you type the PIN shown here
    Pair {
        /// Connect out to this `ip:port` (from `scan`) instead of waiting for the phone
        #[arg(long)]
        to: Option<std::net::SocketAddr>,
    },
    /// List pairing-related services visible on the local network
    Scan,
    /// Mount the developer disk image that provides screen and input services
    Mount,
    /// Run the MCP server on stdio so an AI agent can see and operate the phone
    Mcp,
    /// Run the background daemon in the foreground (normally started automatically)
    Serve {
        #[arg(long, default_value_t = serve::DEFAULT_PORT)]
        port: u16,
    },
    /// Stop the background daemon and disconnect from the phone
    Stop,
    /// Tap at a fractional screen position, e.g. `tap 0.5 0.5` for the centre
    Tap { x: f64, y: f64 },
    /// Drag between two fractional positions, e.g. `swipe 0.8 0.5 0.2 0.5`
    Swipe { x1: f64, y1: f64, x2: f64, y2: f64 },
    /// Press a hardware button: home, lock, volume-up, volume-down, siri
    Button { name: String },
    /// Start the screen stream and report how much video arrives in 3 seconds
    Stream,
    /// Save what is on the phone's screen as a PNG
    Shot { path: std::path::PathBuf },
    /// Print the phone's clipboard, or replace it with the given text
    Clipboard { text: Option<String> },
    /// Developer aid: describe the phone's audio stream and dump it to a file
    #[command(hide = true)]
    AudioProbe { out: std::path::PathBuf },
    /// List what is on the phone's screen: labelled elements, with positions when
    /// the app or daemon is running
    Elements,
    /// List installed apps
    Apps,
    /// Open an app by name or bundle identifier
    Launch { app: String },
    /// List running processes
    Ps,
    /// Show the phone's name, model, iOS version, battery and appearance
    Info,
    /// Read the phone's appearance, or set it to light or dark
    Appearance { style: Option<String> },
    /// Stream the phone's system log until interrupted
    Logs {
        /// Only show lines containing this text
        #[arg(long)]
        filter: Option<String>,
        /// Stop after this many lines
        #[arg(long)]
        lines: Option<usize>,
    },
    /// List crash reports on the phone, or save one into a folder
    Crashes {
        /// The report to save
        name: Option<String>,
        /// Where to save it
        #[arg(default_value = ".")]
        dir: std::path::PathBuf,
    },
    /// Record the screen and sound to an MP4 for a number of seconds
    Record {
        seconds: u64,
        /// Where to save it (defaults to a dated file under Videos\iLinkOS)
        path: Option<std::path::PathBuf>,
    },
    /// Simulate the phone's location: `locate 48.8584 2.2945`, or `locate off`
    Locate {
        /// Latitude in degrees, or `off`
        #[arg(allow_hyphen_values = true)]
        lat: String,
        #[arg(allow_hyphen_values = true)]
        lon: Option<f64>,
    },
    /// Show what the app in front is using: CPU, memory and frame rate
    Perf {
        /// How many one-second samples to print
        #[arg(default_value_t = 5)]
        samples: usize,
    },
    /// List iPhones attached over USB
    Devices,
    /// Open the CoreDevice tunnel and report which control paths this phone exposes
    Doctor {
        /// Print every service the phone advertises
        #[arg(long)]
        all: bool,
    },
}

fn main() -> Result<()> {
    // Read here, before any other thread exists, so the whole process agrees.
    if let Some(address) = Cli::parse().vm {
        // SAFETY: no other thread is running yet.
        unsafe { std::env::set_var(ilinkos::vm::TARGET_VAR, address) };
    }
    ilinkos::block_on_deep(run)
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    if cli.verbose {
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(std::io::stderr)
            .init();
    }
    let tunnel = || device::open_tunnel(cli.usb, cli.udid.as_deref());
    match cli.command {
        Command::Pair { to: Some(addr) } => wifi::pair_direct(addr).await,
        Command::Pair { to: None } => wifi::pair().await,
        Command::Scan => wifi::scan().await,
        Command::Mcp => mcp::run(cli.usb).await,
        Command::Serve { port } => serve::run(port, cli.usb, cli.udid.clone()).await,
        Command::Stop => {
            match daemon::running().await {
                Some(daemon) => {
                    let _ = daemon.request("POST", "/quit", b"").await;
                    println!("stopped");
                }
                None => println!("the daemon is not running"),
            }
            Ok(())
        }
        Command::Devices => {
            let devices = device::usb_devices().await?;
            if devices.is_empty() {
                bail!("no iPhone found — plug it in, unlock it and tap Trust");
            }
            for d in devices {
                println!("{}  {:?}", d.udid, d.connection_type);
            }
            Ok(())
        }
        Command::Record { seconds, ref path } => {
            let daemon = daemon::ensure(cli.usb).await?;
            let path = match path {
                Some(path) => Some(std::path::absolute(path)?),
                None => None,
            };
            let started = api(&daemon, serde_json::json!({"t": "record", "on": true, "path": path})).await?;
            println!("recording to {}", started["path"].as_str().unwrap_or_default());
            tokio::time::sleep(std::time::Duration::from_secs(seconds)).await;
            let saved = api(&daemon, serde_json::json!({"t": "record", "on": false})).await?;
            println!("saved {}", saved["path"].as_str().unwrap_or_default());
            Ok(())
        }
        Command::Locate { ref lat, lon } => {
            let daemon = daemon::ensure(cli.usb).await?;
            if lat == "off" {
                api(&daemon, serde_json::json!({"t": "location_off"})).await?;
                println!("the phone is back at its real location");
            } else {
                let (lat, lon): (f64, f64) = (lat.parse()?, lon.context("give a longitude too")?);
                api(&daemon, serde_json::json!({"t": "location", "lat": lat, "lon": lon})).await?;
                println!("the phone now reports {lat}, {lon} — `ilinkos locate off` or stopping iLinkOS ends it");
            }
            Ok(())
        }
        Command::Perf { samples } => {
            let daemon = daemon::ensure(cli.usb).await?;
            for _ in 0..samples {
                let sample = api(&daemon, serde_json::json!({"t": "perf"})).await?;
                println!(
                    "{:<20} CPU {:>5.1}%   {:>6.0} MB   {:>3.0} fps",
                    sample["app"].as_str().unwrap_or_default(),
                    sample["cpu"].as_f64().unwrap_or(0.0),
                    sample["memory_mb"].as_f64().unwrap_or(0.0),
                    sample["fps"].as_f64().unwrap_or(0.0),
                );
            }
            Ok(())
        }
        Command::Doctor { all } => doctor(tunnel().await?, all),
        Command::Clipboard { ref text } => {
            use idevice::{
                RsdService,
                core_device::{GENERAL_PASTEBOARD, PasteboardServiceClient},
            };
            let (mut handle, mut rsd) = tunnel().await?;
            let mut pasteboard = PasteboardServiceClient::connect_rsd(&mut handle, &mut rsd).await?;
            match text {
                Some(text) => pasteboard.set_text(text, GENERAL_PASTEBOARD).await?,
                None => {
                    let snapshot = pasteboard.get(GENERAL_PASTEBOARD).await?;
                    match (snapshot.text(), snapshot.image()) {
                        (Some(text), _) => println!("{text}"),
                        (None, Some((uti, bytes))) => println!("<{uti}, {} bytes>", bytes.len()),
                        _ => println!("<empty>"),
                    }
                }
            }
            Ok(())
        }
        Command::AudioProbe { ref out } => {
            use idevice::core_device::RtpPacket;
            use std::io::Write;
            let mut session = control::Session::start(tunnel().await?).await?;
            println!("answer: {:#?}", session.audio_answer);
            let audio = session.take_audio().context("no audio socket")?;
            let mut file = std::fs::File::create(out)?;
            let mut count = 0;
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while let Ok(datagram) = audio.recv().await {
                    let Some(packet) = RtpPacket::parse(&datagram.data) else {
                        println!("non-RTP datagram, {} bytes", datagram.data.len());
                        continue;
                    };
                    if count < 12 {
                        println!(
                            "pt={} seq={} ts={} marker={} ext={} len={} head={:02x?}",
                            packet.payload_type,
                            packet.sequence_number,
                            packet.timestamp,
                            packet.marker,
                            packet.extension,
                            packet.payload.len(),
                            &packet.payload[..packet.payload.len().min(16)]
                        );
                    }
                    count += 1;
                    let _ = file.write_all(&(packet.payload.len() as u16).to_le_bytes());
                    let _ = file.write_all(packet.payload);
                }
            })
            .await;
            println!("{count} audio packets in 5s");
            let _ = session.stop().await;
            Ok(())
        }
        Command::Elements => {
            // Through the running app or daemon the elements come with positions,
            // read off its picture; on our own there are only labels.
            let elements: Vec<serde_json::Value> = match daemon::running().await {
                Some(daemon) => {
                    let (status, body) = daemon.request("POST", "/api", br#"{"t":"elements"}"#).await?;
                    if status != 200 {
                        bail!("{}", String::from_utf8_lossy(&body));
                    }
                    serde_json::from_slice(&body)?
                }
                None => {
                    let (mut handle, rsd) = tunnel().await?;
                    let mut inspector = ilinkos::ax::Inspector::connect(&mut handle, &rsd).await?;
                    serde_json::from_value(serde_json::to_value(inspector.elements(None).await?)?)?
                }
            };
            for element in &elements {
                let place = match element.get("rect") {
                    Some(r) => format!(
                        "{:>4},{:<4} {:>3}x{:<4}",
                        r["x"], r["y"], r["width"], r["height"]
                    ),
                    None => " ".repeat(18),
                };
                println!("{place}  {}", element["label"].as_str().unwrap_or_default());
            }
            eprintln!("{} elements", elements.len());
            Ok(())
        }
        Command::Apps => {
            let (mut handle, mut rsd) = tunnel().await?;
            for app in ilinkos::tools::apps(&mut handle, &mut rsd).await? {
                println!("{:<28} {}", app.name, app.bundle_id);
            }
            Ok(())
        }
        Command::Launch { ref app } => {
            let (mut handle, mut rsd) = tunnel().await?;
            let opened = ilinkos::tools::launch(&mut handle, &mut rsd, app).await?;
            println!("opened {} ({})", opened.name, opened.bundle_id);
            Ok(())
        }
        Command::Ps => {
            let (mut handle, mut rsd) = tunnel().await?;
            for process in ilinkos::tools::processes(&mut handle, &mut rsd).await? {
                println!("{:>6}  {}{}", process.pid, process.name, if process.app { "  (app)" } else { "" });
            }
            Ok(())
        }
        Command::Info => {
            let (mut handle, mut rsd) = tunnel().await?;
            let info = ilinkos::tools::info(&mut handle, &mut rsd).await?;
            println!("{}", serde_json::to_string_pretty(&info)?);
            Ok(())
        }
        Command::Appearance { ref style } => {
            let (mut handle, mut rsd) = tunnel().await?;
            println!("{}", ilinkos::tools::appearance(&mut handle, &mut rsd, style.as_deref()).await?);
            Ok(())
        }
        Command::Logs { ref filter, lines } => {
            let (mut handle, mut rsd) = tunnel().await?;
            let mut shown = 0;
            ilinkos::tools::logs(&mut handle, &mut rsd, |line| {
                if filter.as_ref().is_none_or(|f| line.contains(f.as_str())) {
                    println!("{line}");
                    shown += 1;
                }
                lines.is_none_or(|limit| shown < limit)
            })
            .await
        }
        Command::Crashes { ref name, ref dir } => {
            let (mut handle, mut rsd) = tunnel().await?;
            match name {
                None => {
                    for report in ilinkos::tools::crash_reports(&mut handle, &mut rsd).await? {
                        println!("{report}");
                    }
                }
                Some(name) => {
                    let bytes = ilinkos::tools::crash_report(&mut handle, &mut rsd, name).await?;
                    let file = dir.join(name.rsplit('/').next().unwrap_or(name));
                    std::fs::write(&file, bytes)?;
                    println!("saved {}", file.display());
                }
            }
            Ok(())
        }
        Command::Mount => {
            println!("{}", device::mount(tunnel().await?).await?);
            Ok(())
        }
        Command::Shot { ref path } => {
            let mut session = control::Session::start(tunnel().await?).await?;
            let video = session.record(std::time::Duration::from_secs(2)).await;
            let _ = session.stop().await;
            let hevc = path.with_extension("hevc");
            std::fs::write(&hevc, video?)?;
            let status = std::process::Command::new(ilinkos::ffmpeg())
                .args(["-y", "-loglevel", "error", "-i"])
                .arg(&hevc)
                .args(["-frames:v", "1", "-update", "1"])
                .arg(path)
                .status()
                .context("could not run ffmpeg")?;
            let _ = std::fs::remove_file(&hevc);
            if !status.success() {
                bail!("ffmpeg could not decode the captured video");
            }
            println!("saved {}", path.display());
            Ok(())
        }
        Command::Tap { .. } | Command::Swipe { .. } | Command::Button { .. } | Command::Stream => {
            let mut session = control::Session::start(tunnel().await?).await?;
            let result = match &cli.command {
                Command::Tap { x, y } => session.tap(*x, *y).await,
                Command::Swipe { x1, y1, x2, y2 } => {
                    session.drag((*x1, *y1), (*x2, *y2), 600).await
                }
                Command::Button { name } => session.button(name).await,
                _ => {
                    let (packets, bytes) =
                        session.count_video_packets(std::time::Duration::from_secs(3)).await;
                    println!("received {packets} video packets ({} KB) in 3s", bytes / 1000);
                    Ok(())
                }
            };
            let _ = session.stop().await;
            result?;
            println!("ok");
            Ok(())
        }
    }
}

/// Runs one action on the daemon and returns its JSON reply, if it has one.
async fn api(daemon: &daemon::Daemon, action: serde_json::Value) -> Result<serde_json::Value> {
    let (status, body) = daemon.request("POST", "/api", action.to_string().as_bytes()).await?;
    if status != 200 {
        bail!("{}", String::from_utf8_lossy(&body));
    }
    Ok(serde_json::from_slice(&body).unwrap_or_default())
}

fn doctor((_handle, rsd): device::Tunnel, all: bool) -> Result<()> {
    let prop = |k: &str| {
        rsd.properties.get(k).and_then(|v| v.as_string()).unwrap_or("?").to_string()
    };
    println!("{} — iOS {} ({})", prop("ProductType"), prop("OSVersion"), prop("BuildVersion"));
    println!("{} services advertised over the tunnel", rsd.services.len());

    let has = |name: &str| rsd.services.contains_key(name);
    let mark = |ok: bool| if ok { "yes" } else { "NO " };
    println!("  [{}] screen stream   {DISPLAY_SERVICE}", mark(has(DISPLAY_SERVICE)));
    println!("  [{}] touch/keyboard  {HID_SERVICE}", mark(has(HID_SERVICE)));
    println!("  [{}] buttons         {INDIGO_HID_SERVICE}", mark(has(INDIGO_HID_SERVICE)));
    if !(has(DISPLAY_SERVICE) && has(HID_SERVICE)) {
        println!(
            "\nNative control services are missing. They are served from the developer disk \
             image: enable Developer Mode on the phone and run `ilinkos mount`."
        );
    }
    if all {
        let mut names: Vec<_> = rsd.services.keys().collect();
        names.sort();
        for name in names {
            println!("  {name}");
        }
    }
    Ok(())
}
