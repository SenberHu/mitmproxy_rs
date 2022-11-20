use std::collections::HashSet;
use std::net::SocketAddr;
use std::time::Duration;
use std::{env, process, thread};

use anyhow::{Context, Result};
use log::{debug, warn};
use lru_time_cache::LruCache;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};
use tokio::sync::mpsc;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use windivert::address::WinDivertNetworkData;
use windivert::{
    WinDivert, WinDivertEvent, WinDivertFlags, WinDivertLayer, WinDivertPacket,
    WinDivertParsedPacket,
};

use mitmproxy::packet_sources::windivert::{WinDivertIPC, CONF, IPC_BUF_SIZE, PID};
use mitmproxy::MAX_PACKET_SIZE;

use crate::packet::{ConnectionId, InternetPacket, TransportProtocol};

mod packet;

#[derive(Debug)]
enum Message {
    /// We have received either a new network packet or a socket event.
    Packet(WinDivertPacket),
    /// We have received a original destination lookup request via stdin.
    Inject(Vec<u8>),
    InterceptInclude(Vec<PID>),
    InterceptExclude(Vec<PID>),
}

#[derive(Debug)]
enum ConnectionState<'a> {
    Known(ConnectionAction),
    Unknown(Vec<(WinDivertNetworkData<'a>, InternetPacket)>),
}

#[derive(Debug, Clone, Copy)]
enum ConnectionAction {
    None,
    Intercept,
}

enum Config {
    InterceptInclude(HashSet<PID>),
    InterceptExclude(HashSet<PID>),
}

impl Config {
    fn should_intercept(&self, pid: PID) -> bool {
        match self {
            Config::InterceptInclude(pids) => pids.contains(&pid),
            Config::InterceptExclude(pids) => !pids.contains(&pid),
        }
    }
}

async fn handle_ipc(
    mut ipc: NamedPipeClient,
    mut ipc_rx: UnboundedReceiver<WinDivertIPC>,
    tx: UnboundedSender<Message>,
) -> Result<()> {
    let mut buf = [0u8; IPC_BUF_SIZE];
    loop {
        tokio::select! {
            Ok(len) = ipc.read(&mut buf) => {
                dbg!(&buf[..len]);
                match bincode::decode_from_slice(&buf[..len], CONF)?.0 {
                    WinDivertIPC::Packet(p) => {
                        tx.send(Message::Inject(p))?;
                    }
                    WinDivertIPC::InterceptInclude(a) => {
                        tx.send(Message::InterceptInclude(a))?;
                    }
                    WinDivertIPC::InterceptExclude(a) => {
                        tx.send(Message::InterceptExclude(a))?;
                    }
                    WinDivertIPC::Shutdown => {
                        process::exit(0);
                    }
                }
            },
            Some(packet) = ipc_rx.recv() => {
                let len = bincode::encode_into_slice(&packet, &mut buf, CONF)?;
                ipc.write_all(&buf[..len]).await?;
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    if cfg!(debug_assertions) {
        // this increases binary size from ~300kb to 1MB for release builds.
        env_logger::init();
    }
    let args: Vec<String> = env::args().collect();
    let pipe_name = args
        .get(1)
        .map(|x| x.as_str())
        //.map(|x| x.trim_start_matches(r"\\.\pipe\"))
        .unwrap_or(r"\\.\pipe\mitmproxy-transparent-proxy");
    //.context(anyhow!("Usage: {} <pipename>", args[0]))?;

    let ipc = ClientOptions::new()
        .open(pipe_name)
        .context("Cannot open pipe")?;

    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();

    let (mut ipc_tx, ipc_rx) = mpsc::unbounded_channel::<WinDivertIPC>();

    // We currently rely on handles being automatically closed when the program exits.
    // only needed for forward mode
    // let _icmp_handle = WinDivert::new("icmp", WinDivertLayer::Network, 1042, WinDivertFlags::new().set_drop()).context("Error opening WinDivert handle")?;

    let socket_handle = WinDivert::new(
        "tcp || udp",
        WinDivertLayer::Socket,
        1041,
        WinDivertFlags::new().set_recv_only().set_sniff(),
    )?;
    let network_handle = WinDivert::new(
        "tcp || udp",
        WinDivertLayer::Network,
        1040,
        WinDivertFlags::new(),
    )?;
    let inject_handle = WinDivert::new(
        "false",
        WinDivertLayer::Network,
        1039,
        WinDivertFlags::new().set_send_only(),
    )?;

    let tx_clone = tx.clone();
    thread::spawn(move || relay_events(socket_handle, 0, 32, tx_clone));
    let tx_clone = tx.clone();
    thread::spawn(move || relay_events(network_handle, MAX_PACKET_SIZE, 8, tx_clone));

    tokio::spawn(handle_ipc(ipc, ipc_rx, tx));

    let mut connections = LruCache::<ConnectionId, ConnectionState>::with_expiry_duration(
        Duration::from_secs(60 * 10),
    );
    let mut state = Config::InterceptInclude(HashSet::new());

    loop {
        let result = rx.recv().await.unwrap();
        match result {
            Message::Packet(wd_packet) => {
                match wd_packet.parse() {
                    WinDivertParsedPacket::Network { addr, data } => {
                        let packet = match InternetPacket::new(data) {
                            Ok(p) => p,
                            Err(e) => {
                                debug!("Error parsing packet: {:?}", e);
                                continue;
                            }
                        };

                        debug!(
                            "Received packet: {} {} {}",
                            packet.connection_id(),
                            packet.tcp_flag_str(),
                            packet.payload().len()
                        );

                        let is_multicast =
                            packet.src_ip().is_multicast() || packet.dst_ip().is_multicast();
                        let is_loopback_only =
                            packet.src_ip().is_loopback() && packet.dst_ip().is_loopback();
                        if is_multicast || is_loopback_only {
                            debug!(
                                "skipping multicast={} loopback={}",
                                is_multicast, is_loopback_only
                            );
                            inject_handle.send(WinDivertParsedPacket::Network {
                                addr,
                                data: packet.inner(),
                            })?;
                            continue;
                        }

                        match connections.get_mut(&packet.connection_id()) {
                            Some(state) => match state {
                                ConnectionState::Known(s) => {
                                    process_packet(addr, packet, *s, &inject_handle, &mut ipc_tx)
                                        .await?;
                                }
                                ConnectionState::Unknown(packets) => {
                                    packets.push((addr, packet));
                                }
                            },
                            None => {
                                if addr.outbound() {
                                    // We expect a corresponding socket event soon.
                                    debug!("Adding unknown packet: {}", packet.connection_id());
                                    connections.insert(
                                        packet.connection_id(),
                                        ConnectionState::Unknown(vec![(addr, packet)]),
                                    );
                                } else {
                                    // A new inbound connection.
                                    debug!("Adding inbound redirect: {}", packet.connection_id());
                                    warn!("Unimplemented: No proper handling of inbound connections yet.");
                                    let connection_id = packet.connection_id();
                                    insert_into_connections(
                                        &mut connections,
                                        connection_id.reverse(),
                                        ConnectionAction::None,
                                        &inject_handle,
                                        &mut ipc_tx,
                                    )
                                    .await?;
                                    insert_into_connections(
                                        &mut connections,
                                        connection_id,
                                        ConnectionAction::Intercept,
                                        &inject_handle,
                                        &mut ipc_tx,
                                    )
                                    .await?;
                                    process_packet(
                                        addr,
                                        packet,
                                        ConnectionAction::Intercept,
                                        &inject_handle,
                                        &mut ipc_tx,
                                    )
                                    .await?;
                                }
                            }
                        }
                    }
                    WinDivertParsedPacket::Socket { addr } => {
                        if addr.process_id() == 4 {
                            // We get some operating system events here, which generally are not useful.
                            debug!("Skipping PID 4");
                            continue;
                        }

                        let proto = match TransportProtocol::try_from(addr.protocol()) {
                            Ok(p) => p,
                            Err(e) => {
                                debug!("Error parsing packet: {:?}", e);
                                continue;
                            }
                        };
                        let connection_id = ConnectionId {
                            proto,
                            src: SocketAddr::from((addr.local_address(), addr.local_port())),
                            dst: SocketAddr::from((addr.remote_address(), addr.remote_port())),
                        };

                        if connection_id.src.ip().is_multicast()
                            || connection_id.dst.ip().is_multicast()
                        {
                            continue;
                        }

                        match addr.event() {
                            WinDivertEvent::SocketConnect | WinDivertEvent::SocketAccept => {
                                let make_entry = match connections.get(&connection_id) {
                                    None => true,
                                    Some(e) => matches!(e, ConnectionState::Unknown(_)),
                                };

                                debug!(
                                    "{:<15?} make_entry={} pid={} {}",
                                    addr.event(),
                                    make_entry,
                                    addr.process_id(),
                                    connection_id
                                );

                                if make_entry {
                                    debug!(
                                        "Adding: {} with pid={} ({:?})",
                                        &connection_id,
                                        addr.process_id(),
                                        addr.event()
                                    );

                                    let action = if state.should_intercept(addr.process_id()) {
                                        ConnectionAction::Intercept
                                    } else {
                                        ConnectionAction::None
                                    };

                                    insert_into_connections(
                                        &mut connections,
                                        connection_id.reverse(),
                                        ConnectionAction::None,
                                        &inject_handle,
                                        &mut ipc_tx,
                                    )
                                    .await?;
                                    insert_into_connections(
                                        &mut connections,
                                        connection_id,
                                        action,
                                        &inject_handle,
                                        &mut ipc_tx,
                                    )
                                    .await?;
                                }
                            }
                            WinDivertEvent::SocketClose => {
                                // We cannot clean up here because there are still final packets on connections after this event,
                                // But at least we can release memory for unknown connections.
                                match connections.get_mut(&connection_id) {
                                    Some(ConnectionState::Unknown(packets)) => packets.clear(),
                                    _ => {}
                                }
                            }
                            _ => {}
                        }
                    }
                    _ => unreachable!(),
                }
            }
            Message::Inject(buf) => {
                let mut addr = WinDivertNetworkData::default();
                // if outbound is false, incoming connections are not re-injected into the right iface.
                addr.set_outbound(true);
                addr.set_ip_checksum(false);
                addr.set_tcp_checksum(false);
                addr.set_udp_checksum(false);

                inject_handle.send(WinDivertParsedPacket::Network { addr, data: buf })?;
            }
            Message::InterceptInclude(a) => {
                debug!("Intercepting only the following PIDs: {:?}", &a);
                state = Config::InterceptInclude(HashSet::from_iter(a.into_iter()));
            }
            Message::InterceptExclude(a) => {
                debug!("Intercepting everything but the following PIDs: {:?}", &a);
                state = Config::InterceptExclude(HashSet::from_iter(a.into_iter()));
            }
        }
    }
}

/// Repeatedly call WinDivertRecvExt o get packets and feed them into the channel.
fn relay_events(
    handle: WinDivert,
    buffer_size: usize,
    packet_count: usize,
    tx: UnboundedSender<Message>,
) {
    loop {
        let packets = handle.recv_ex(buffer_size, packet_count);
        match packets {
            Ok(Some(packets)) => {
                for packet in packets {
                    tx.send(Message::Packet(packet)).unwrap();
                }
            }
            Ok(None) => {}
            Err(err) => {
                eprintln!("WinDivert Error: {:?}", err);
                process::exit(74);
            }
        };
    }
}

async fn insert_into_connections(
    connections: &mut LruCache<ConnectionId, ConnectionState<'_>>,
    key: ConnectionId,
    state: ConnectionAction,
    inject_handle: &WinDivert,
    ipc_tx: &mut UnboundedSender<WinDivertIPC>,
) -> Result<()> {
    let existing = connections.insert(key, ConnectionState::Known(state));

    if let Some(ConnectionState::Unknown(packets)) = existing {
        for (addr, p) in packets {
            process_packet(addr, p, state, inject_handle, ipc_tx).await?;
        }
    }
    Ok(())
}

async fn process_packet(
    addr: WinDivertNetworkData<'_>,
    packet: InternetPacket,
    action: ConnectionAction,
    inject_handle: &WinDivert,
    ipc_tx: &mut UnboundedSender<WinDivertIPC>,
) -> Result<()> {
    match action {
        ConnectionAction::None => {
            debug!(
                "Injecting {} {} with action={:?} outbound={} loopback={}",
                packet.connection_id(),
                packet.tcp_flag_str(),
                &action,
                addr.outbound(),
                addr.loopback()
            );
            inject_handle
                .send(WinDivertParsedPacket::Network {
                    addr,
                    data: packet.inner(),
                })
                .context("failed to re-inject packet")?;
        }
        ConnectionAction::Intercept => {
            ipc_tx.send(WinDivertIPC::Packet(packet.inner()))?;
        }
    }
    Ok(())
}