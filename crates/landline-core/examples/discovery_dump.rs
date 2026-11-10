//! Manual LAN check for discovery: listens on the real ports, sends one
//! broadcast probe, and prints a line per event for about 15 seconds.
//!
//! ```sh
//! cargo run -p landline-core --example discovery_dump [-- --announce]
//! ```
//!
//! With `--announce` it also advertises a throwaway mDNS service under a
//! random device ID. KDE Connect apps that resolve it answer by sending their
//! UDP identity here, which shows up as a datagram. Nothing listens on the
//! advertised TCP port, so no connection follows.
//!
//! Datagram contents are not printed, only their size and packet type.

use std::time::Duration;

use landline_core::discovery::{
    Announcement, Discovery, DiscoveryConfig, DiscoveryEvent, ListenerState, SendReport,
};
use tokio::sync::mpsc;

const RUN_FOR: Duration = Duration::from_secs(15);

/// Not an identity packet, so KDE Connect apps discard it after logging.
const PROBE: &[u8] = b"{\"id\":0,\"type\":\"landline.probe\",\"body\":{}}\n";

#[tokio::main]
async fn main() {
    let announce = std::env::args().any(|arg| arg == "--announce");
    let (tx, mut rx) = mpsc::channel(64);
    let discovery = Discovery::start(DiscoveryConfig::default(), tx).expect("inside a runtime");

    match discovery.listener_state() {
        ListenerState::Listening { port } => println!("listener: UDP port {port}"),
        ListenerState::SendOnly { error } => {
            println!("listener: send-only ({:?}: {error})", error.kind);
        }
    }
    println!("mDNS running: {}", discovery.mdns_running());

    if announce {
        let device_id = landline_core::device_id::generate();
        let result = discovery.announce(Announcement {
            device_id,
            device_name: "landline discovery_dump".into(),
            device_type: "laptop".into(),
            protocol_version: 8,
            tcp_port: 1716,
        });
        println!("announce: {result:?}");
    }

    print_report("broadcast", &discovery.broadcast(PROBE).await);

    let deadline = tokio::time::sleep(RUN_FOR);
    tokio::pin!(deadline);
    let mut count = 0;
    loop {
        let event = tokio::select! {
            () = &mut deadline => break,
            event = rx.recv() => event,
        };
        let Some(event) = event else { break };
        count += 1;
        match event {
            DiscoveryEvent::Datagram { from, data } => {
                let kind = if data == PROBE {
                    "our own probe".to_owned()
                } else {
                    packet_type(&data)
                };
                println!("udp  from {from}  {} bytes  {kind}", data.len());
            }
            DiscoveryEvent::MdnsPeer(peer) => {
                println!(
                    "mdns {:?} port {}  type {:?}  protocol {:?}  id known: {}",
                    peer.addresses,
                    peer.port,
                    peer.device_type,
                    peer.protocol_version,
                    peer.device_id.is_some(),
                );
            }
        }
    }
    println!("{count} events in {}s", RUN_FOR.as_secs());

    tokio::task::spawn_blocking(move || drop(discovery))
        .await
        .expect("drop discovery");
}

fn packet_type(data: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(data)
        .ok()
        .and_then(|packet| packet.get("type")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| "not a JSON packet".to_owned())
}

fn print_report(what: &str, report: &SendReport) {
    for attempt in &report.attempts {
        match &attempt.result {
            Ok(()) => println!("{what}: {:?} {} sent", attempt.target, attempt.to),
            Err(error) => println!(
                "{what}: {:?} {} FAILED {:?} (os {:?}): {error}",
                attempt.target, attempt.to, error.kind, error.os_code
            ),
        }
    }
    println!(
        "{what}: sent on at least one target: {}, likely Local Network denial: {}",
        report.sent_any(),
        report.likely_local_network_denied()
    );
}
