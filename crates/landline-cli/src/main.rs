//! `landline`: drives the core headlessly, so protocol work can be tested
//! against the real phone without touching Swift.

use std::io::{BufRead, Write};
use std::net::IpAddr;
use std::path::PathBuf;
use std::process::{Command as Process, ExitCode};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use tracing_subscriber::filter::Targets;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use clap::{Parser, Subcommand};
use landline_core::{
    Config, Core, CoreDelegate, Device, NetworkStatus, Packet, PairState, PairingFailure,
};

#[derive(Parser)]
#[command(
    name = "landline",
    version,
    about = "Headless driver for the Landline core"
)]
struct Cli {
    /// Where this device's certificate and pairings live. Defaults to the
    /// directory the app uses, so both share one identity. Do not run the
    /// CLI and the app at the same time against the same directory.
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    /// Name shown on the other device. Defaults to this computer's name.
    #[arg(long, global = true)]
    name: Option<String>,
    /// Also announce directly to this address, for networks that drop
    /// broadcasts.
    #[arg(long, global = true, value_name = "IP")]
    connect: Option<IpAddr>,
    /// Log what the core is doing to stderr. Repeat for more detail.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print the core version.
    Version,
    /// List devices as they appear on the network.
    Discover {
        /// Stop after this many seconds.
        #[arg(long, default_value_t = 10)]
        seconds: u64,
    },
    /// Pair with a device, or answer its pairing request.
    Pair {
        /// Device ID, or the start of its name.
        device: String,
    },
    /// Forget a paired device.
    Unpair {
        /// Device ID, or the start of its name.
        device: String,
    },
    /// Ping a paired device.
    Ping {
        /// Device ID, or the start of its name.
        device: String,
    },
    /// Print every event and every packet from paired devices as JSON lines,
    /// until interrupted. Packets can contain private content.
    Listen,
}

enum Event {
    Devices(Vec<Device>),
    PairingFailed(String, PairingFailure),
    Ping(String),
    Network(NetworkStatus),
    Packet(String, String),
}

struct Forward(Sender<Event>);

impl CoreDelegate for Forward {
    fn devices_changed(&self, devices: Vec<Device>) {
        let _ = self.0.send(Event::Devices(devices));
    }
    fn pairing_failed(&self, device_id: &str, reason: PairingFailure) {
        let _ = self
            .0
            .send(Event::PairingFailed(device_id.to_owned(), reason));
    }
    fn ping_received(&self, device_id: &str) {
        let _ = self.0.send(Event::Ping(device_id.to_owned()));
    }
    fn network_status_changed(&self, status: NetworkStatus) {
        let _ = self.0.send(Event::Network(status));
    }
    fn packet_received(&self, device_id: &str, packet: &Packet) {
        let _ = self
            .0
            .send(Event::Packet(device_id.to_owned(), packet.to_line()));
    }
}

type Failure = Box<dyn std::error::Error>;

/// How long to wait for a named device to show up on the network.
const FIND_TIMEOUT: Duration = Duration::from_secs(20);

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging(cli.verbose);
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("landline: {error}");
            ExitCode::FAILURE
        }
    }
}

fn init_logging(verbose: u8) {
    let level = match verbose {
        0 => return,
        1 => tracing::Level::INFO,
        2 => tracing::Level::DEBUG,
        _ => tracing::Level::TRACE,
    };
    // Only the core's own events: its dependencies are far too chatty.
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .with(Targets::new().with_target("landline_core", level))
        .init();
}

fn run(cli: Cli) -> Result<(), Failure> {
    if matches!(cli.command, Command::Version) {
        println!("{}", landline_core::version());
        return Ok(());
    }

    let data_dir = match cli.data_dir {
        Some(dir) => dir,
        None => default_data_dir()?,
    };
    let name = cli.name.unwrap_or_else(computer_name);
    let (tx, events) = mpsc::channel();
    let core = Core::new(Config::new(data_dir, name), Arc::new(Forward(tx)))?;
    core.start();
    if let Some(address) = cli.connect {
        core.announce_to(address);
    }
    let session = Session { core, events };

    match cli.command {
        Command::Version => unreachable!("handled above"),
        Command::Discover { seconds } => session.discover(Duration::from_secs(seconds)),
        Command::Pair { device } => session.pair(&device),
        Command::Unpair { device } => session.unpair(&device),
        Command::Ping { device } => session.ping(&device),
        Command::Listen => session.listen(),
    }
}

/// The directory the app uses: Application Support, named by bundle ID.
fn default_data_dir() -> Result<PathBuf, Failure> {
    let dirs = directories::ProjectDirs::from("io.github", "tuhin-cmd", "landline")
        .ok_or("cannot find a home directory; pass --data-dir")?;
    Ok(dirs.data_dir().to_owned())
}

fn computer_name() -> String {
    Process::new("scutil")
        .args(["--get", "ComputerName"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "Mac".to_owned())
}

struct Session {
    core: Core,
    events: Receiver<Event>,
}

impl Session {
    fn discover(&self, duration: Duration) -> Result<(), Failure> {
        let deadline = Instant::now() + duration;
        print_devices(&self.core.devices());
        while let Some(event) = self.next_event(deadline) {
            match event {
                Event::Devices(devices) => print_devices(&devices),
                Event::Network(status) => explain_network(status),
                _ => {}
            }
        }
        Ok(())
    }

    fn pair(&self, wanted: &str) -> Result<(), Failure> {
        let device = self.find(wanted, |device| device.reachable)?;
        if device.pair_state == PairState::Paired {
            println!("{} is already paired.", device.name);
            return Ok(());
        }
        if device.pair_state == PairState::NotPaired {
            self.core.request_pair(&device.id)?;
        }

        let deadline = Instant::now() + Duration::from_secs(40);
        let mut shown = false;
        let mut current = Some(device.clone());
        loop {
            if let Some(now) = current.take() {
                match now.pair_state {
                    PairState::Paired => {
                        println!("Paired with {}.", now.name);
                        return Ok(());
                    }
                    PairState::Requested if !shown => {
                        shown = true;
                        let key = now.verification_key.unwrap_or_default();
                        println!("Asked {} to pair. Verification key: {key}", now.name);
                        println!("Accept on the device if it shows the same key.");
                    }
                    PairState::RequestedByPeer if !shown => {
                        shown = true;
                        let key = now.verification_key.unwrap_or_default();
                        println!("{} asks to pair. Verification key: {key}", now.name);
                        if confirm("Does the device show the same key? [y/N] ") {
                            self.core.accept_pair(&now.id)?;
                        } else {
                            self.core.reject_pair(&now.id)?;
                            return Err("pairing rejected".into());
                        }
                    }
                    _ => {}
                }
            }
            match self.next_event(deadline) {
                Some(Event::Devices(devices)) => {
                    current = devices.into_iter().find(|d| d.id == device.id);
                    if current.is_none() {
                        return Err("the device went away".into());
                    }
                }
                Some(Event::PairingFailed(id, reason)) if id == device.id => {
                    return Err(format!("pairing failed: {reason}").into());
                }
                Some(_) => {}
                None => return Err("pairing did not finish".into()),
            }
        }
    }

    fn unpair(&self, wanted: &str) -> Result<(), Failure> {
        let device = self.find(wanted, |device| device.pair_state == PairState::Paired)?;
        self.core.unpair(&device.id)?;
        // Give the core a moment to tell the device before the process ends.
        let deadline = Instant::now() + Duration::from_secs(5);
        while let Some(event) = self.next_event(deadline) {
            if let Event::Devices(devices) = event
                && !devices
                    .iter()
                    .any(|d| d.id == device.id && d.pair_state == PairState::Paired)
            {
                break;
            }
        }
        println!("Unpaired {}.", device.name);
        Ok(())
    }

    fn ping(&self, wanted: &str) -> Result<(), Failure> {
        let ready = |d: &Device| d.reachable && d.pair_state == PairState::Paired;
        let device = self.find(wanted, ready)?;
        self.core.send_ping(&device.id)?;
        // The write happens on a core thread; do not exit before it does.
        std::thread::sleep(Duration::from_millis(500));
        println!("Pinged {}.", device.name);
        Ok(())
    }

    fn listen(&self) -> Result<(), Failure> {
        let mut out = std::io::stdout().lock();
        loop {
            let line = match self.events.recv()? {
                Event::Devices(devices) => {
                    let devices: Vec<_> = devices.iter().map(device_json).collect();
                    serde_json::json!({ "event": "devices", "devices": devices })
                }
                Event::PairingFailed(device, reason) => serde_json::json!({
                    "event": "pairing_failed", "device": device, "reason": reason.to_string(),
                }),
                Event::Ping(device) => serde_json::json!({ "event": "ping", "device": device }),
                Event::Network(status) => {
                    explain_network(status);
                    serde_json::json!({ "event": "network", "status": format!("{status:?}") })
                }
                Event::Packet(device, line) => {
                    let packet: serde_json::Value = serde_json::from_str(&line)?;
                    serde_json::json!({ "event": "packet", "device": device, "packet": packet })
                }
            };
            writeln!(out, "{line}")?;
            out.flush()?;
        }
    }

    /// Waits for the one device matching `wanted` (an ID, or the start of a
    /// name) to satisfy `ready`.
    fn find(&self, wanted: &str, ready: impl Fn(&Device) -> bool) -> Result<Device, Failure> {
        let deadline = Instant::now() + FIND_TIMEOUT;
        let mut devices = self.core.devices();
        loop {
            let matching: Vec<&Device> = devices.iter().filter(|d| matches(d, wanted)).collect();
            match matching.as_slice() {
                [device] if ready(device) => return Ok((*device).clone()),
                [_, _, ..] => {
                    return Err(format!("\"{wanted}\" matches more than one device").into());
                }
                _ => {}
            }
            match self.next_event(deadline) {
                Some(Event::Devices(list)) => devices = list,
                Some(Event::Network(status)) => explain_network(status),
                Some(_) => {}
                None => {
                    let seen = if devices.iter().any(|d| matches(d, wanted)) {
                        "it is not reachable (or not paired)"
                    } else {
                        "no such device was found"
                    };
                    return Err(format!("\"{wanted}\": {seen}").into());
                }
            }
        }
    }

    fn next_event(&self, deadline: Instant) -> Option<Event> {
        let left = deadline.saturating_duration_since(Instant::now());
        self.events.recv_timeout(left).ok()
    }
}

fn matches(device: &Device, wanted: &str) -> bool {
    device.id == wanted
        || device
            .name
            .to_lowercase()
            .starts_with(&wanted.to_lowercase())
}

fn print_devices(devices: &[Device]) {
    if devices.is_empty() {
        println!("(no devices)");
    }
    for device in devices {
        let state = match device.pair_state {
            PairState::Paired => "paired",
            PairState::NotPaired => "not paired",
            PairState::Requested => "pairing requested",
            PairState::RequestedByPeer => "asks to pair",
        };
        let reach = if device.reachable {
            "reachable"
        } else {
            "offline"
        };
        println!(
            "{}  {}  {}  {state}, {reach}",
            device.id,
            device.device_type.as_str(),
            device.name
        );
    }
    println!();
}

fn device_json(device: &Device) -> serde_json::Value {
    serde_json::json!({
        "id": device.id,
        "name": device.name,
        "type": device.device_type.as_str(),
        "reachable": device.reachable,
        "pair_state": format!("{:?}", device.pair_state),
        "verification_key": device.verification_key,
    })
}

fn explain_network(status: NetworkStatus) {
    match status {
        NetworkStatus::Ready => {}
        NetworkStatus::Offline => eprintln!("landline: no usable network interface or port"),
        NetworkStatus::Blocked => eprintln!(
            "landline: every send was refused. Allow this terminal under System Settings > \
             Privacy & Security > Local Network."
        ),
    }
}

fn confirm(prompt: &str) -> bool {
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    let _ = std::io::stdin().lock().read_line(&mut answer);
    matches!(answer.trim().to_lowercase().as_str(), "y" | "yes")
}
