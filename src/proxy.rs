use std::collections::HashMap;
use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpSocket, TcpStream, UdpSocket, lookup_host};
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

use crate::adapter::EgressTarget;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    pub listen_ip: IpAddr,
    pub listen_port: u16,
    pub egress: Vec<EgressTarget>,
    pub udp_enabled: bool,
}

#[derive(Debug, Clone)]
pub enum ConnectionEvent {
    Opened(ConnectionOpened),
    Closed(ConnectionClosed),
}

#[derive(Debug, Clone)]
pub struct ConnectionOpened {
    pub id: String,
    pub protocol: ConnectionProtocol,
    pub client: SocketAddr,
    pub target: String,
    pub egress_name: String,
    pub egress_ip: IpAddr,
    pub opened_at: Instant,
}

#[derive(Debug, Clone)]
pub struct ConnectionClosed {
    pub id: String,
    pub up_bytes: u64,
    pub down_bytes: u64,
    pub reason: String,
    pub closed_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionProtocol {
    Tcp,
    Udp,
}

impl ConnectionProtocol {
    pub fn label(self) -> &'static str {
        match self {
            Self::Tcp => "TCP",
            Self::Udp => "UDP",
        }
    }
}

pub async fn run_proxy(
    config: ProxyConfig,
    cancel: CancellationToken,
    log: mpsc::Sender<String>,
    monitor: Option<mpsc::Sender<ConnectionEvent>>,
) -> Result<()> {
    if config.egress.is_empty() {
        return Err(anyhow!(
            "at least one egress adapter address must be selected"
        ));
    }

    let listen_addr = SocketAddr::new(config.listen_ip, config.listen_port);
    let listener = TcpListener::bind(listen_addr)
        .await
        .with_context(|| format!("failed to bind SOCKS5 listener on {listen_addr}"))?;
    let local_addr = listener.local_addr()?;
    log_line(&log, format!("SOCKS5 proxy listening on {local_addr}"));

    let pool = Arc::new(WeightedPool::new(config.egress.clone()));
    log_line(
        &log,
        format!("selected egress adapters: {}", pool.describe()),
    );
    log_line(&log, "egress policy: sticky per destination IP");

    let connection_id = Arc::new(AtomicU64::new(1));
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                log_line(&log, "proxy shutdown requested");
                break;
            }
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(value) => value,
                    Err(error) => {
                        log_line(&log, format!("accept failed: {error}"));
                        continue;
                    }
                };

                let id = connection_id.fetch_add(1, Ordering::Relaxed);
                let pool = pool.clone();
                let child_cancel = cancel.clone();
                let child_log = log.clone();
                let child_monitor = monitor.clone();
                let udp_enabled = config.udp_enabled;
                let listen_ip = config.listen_ip;

                let context = ClientContext {
                    id,
                    peer,
                    pool,
                    cancel: child_cancel,
                    log: child_log.clone(),
                    monitor: child_monitor,
                    udp_enabled,
                    listen_ip,
                };

                tokio::spawn(async move {
                    if let Err(error) = handle_client(stream, context).await {
                        log_line(&child_log, format!("#{id} {peer}: {error}"));
                    }
                });
            }
        }
    }

    Ok(())
}

struct ClientContext {
    id: u64,
    peer: SocketAddr,
    pool: Arc<WeightedPool>,
    cancel: CancellationToken,
    log: mpsc::Sender<String>,
    monitor: Option<mpsc::Sender<ConnectionEvent>>,
    udp_enabled: bool,
    listen_ip: IpAddr,
}

async fn handle_client(mut inbound: TcpStream, context: ClientContext) -> Result<()> {
    read_handshake(&mut inbound).await?;
    let request = read_request(&mut inbound).await?;

    match request.command {
        SocksCommand::Connect => {
            let target_label = request.target.to_string();
            let (mut outbound, selected) = connect_via_pool(&request.target, &context.pool).await?;
            let connection_id = context.id.to_string();
            let egress_name = selected.name.clone();
            let egress_ip = selected.ip;
            let bound = outbound
                .local_addr()
                .unwrap_or_else(|_| zero_socket_addr_for(outbound.peer_addr().ok()));
            write_reply(&mut inbound, 0x00, bound).await?;
            emit_connection(
                &context.monitor,
                ConnectionEvent::Opened(ConnectionOpened {
                    id: connection_id.clone(),
                    protocol: ConnectionProtocol::Tcp,
                    client: context.peer,
                    target: target_label.clone(),
                    egress_name: egress_name.clone(),
                    egress_ip,
                    opened_at: Instant::now(),
                }),
            );
            log_line(
                &context.log,
                format!(
                    "#{} CONNECT {target_label} via {} ({})",
                    context.id, egress_name, egress_ip
                ),
            );

            let (up_bytes, down_bytes, reason) = tokio::select! {
                result = tokio::io::copy_bidirectional(&mut inbound, &mut outbound) => {
                    match result {
                        Ok((from_client, from_server)) => {
                            log_line(
                                &context.log,
                                format!("#{} closed: {from_client} bytes up, {from_server} bytes down", context.id),
                            );
                            (from_client, from_server, "closed".to_owned())
                        }
                        Err(error) => {
                            log_line(&context.log, format!("#{} relay error: {error}", context.id));
                            (0, 0, format!("relay error: {error}"))
                        }
                    }
                }
                _ = context.cancel.cancelled() => (0, 0, "cancelled".to_owned()),
            };
            emit_connection(
                &context.monitor,
                ConnectionEvent::Closed(ConnectionClosed {
                    id: connection_id,
                    up_bytes,
                    down_bytes,
                    reason,
                    closed_at: Instant::now(),
                }),
            );
            Ok(())
        }
        SocksCommand::UdpAssociate => {
            if !context.udp_enabled {
                write_reply(&mut inbound, 0x07, zero_socket_addr_for(Some(context.peer))).await?;
                return Err(anyhow!("UDP ASSOCIATE disabled"));
            }
            run_udp_association(
                inbound,
                UdpAssociationContext {
                    id: context.id,
                    peer: context.peer,
                    listen_ip: context.listen_ip,
                    pool: context.pool,
                    cancel: context.cancel,
                    log: context.log,
                    monitor: context.monitor,
                },
            )
            .await
        }
        SocksCommand::Bind => {
            write_reply(&mut inbound, 0x07, zero_socket_addr_for(Some(context.peer))).await?;
            Err(anyhow!("SOCKS BIND is not supported"))
        }
    }
}

async fn read_handshake(stream: &mut TcpStream) -> Result<()> {
    let mut header = [0_u8; 2];
    stream.read_exact(&mut header).await?;
    if header[0] != 0x05 {
        return Err(anyhow!("unsupported SOCKS version {}", header[0]));
    }

    let method_count = header[1] as usize;
    let mut methods = vec![0_u8; method_count];
    stream.read_exact(&mut methods).await?;
    if !methods.contains(&0x00) {
        stream.write_all(&[0x05, 0xff]).await?;
        return Err(anyhow!("client offered no-auth unsupported methods only"));
    }

    stream.write_all(&[0x05, 0x00]).await?;
    Ok(())
}

async fn read_request(stream: &mut TcpStream) -> Result<SocksRequest> {
    let mut header = [0_u8; 4];
    stream.read_exact(&mut header).await?;
    if header[0] != 0x05 {
        return Err(anyhow!("unsupported request version {}", header[0]));
    }

    let command = match header[1] {
        0x01 => SocksCommand::Connect,
        0x02 => SocksCommand::Bind,
        0x03 => SocksCommand::UdpAssociate,
        other => return Err(anyhow!("unsupported SOCKS command {other}")),
    };
    let target = read_socks_target(stream, header[3]).await?;
    Ok(SocksRequest { command, target })
}

async fn read_socks_target(stream: &mut TcpStream, atyp: u8) -> Result<SocksTarget> {
    let target = match atyp {
        0x01 => {
            let mut ip = [0_u8; 4];
            stream.read_exact(&mut ip).await?;
            let port = read_port(stream).await?;
            SocksTarget::Ip(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(ip)), port))
        }
        0x03 => {
            let mut len = [0_u8; 1];
            stream.read_exact(&mut len).await?;
            let mut domain = vec![0_u8; len[0] as usize];
            stream.read_exact(&mut domain).await?;
            let port = read_port(stream).await?;
            SocksTarget::Domain(
                String::from_utf8(domain).context("domain is not valid UTF-8")?,
                port,
            )
        }
        0x04 => {
            let mut ip = [0_u8; 16];
            stream.read_exact(&mut ip).await?;
            let port = read_port(stream).await?;
            SocksTarget::Ip(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(ip)), port))
        }
        other => return Err(anyhow!("unsupported address type {other}")),
    };
    Ok(target)
}

async fn read_port(stream: &mut TcpStream) -> Result<u16> {
    let mut port = [0_u8; 2];
    stream.read_exact(&mut port).await?;
    Ok(u16::from_be_bytes(port))
}

async fn write_reply(stream: &mut TcpStream, code: u8, bind: SocketAddr) -> Result<()> {
    let mut reply = Vec::with_capacity(22);
    reply.push(0x05);
    reply.push(code);
    reply.push(0x00);
    match bind.ip() {
        IpAddr::V4(ip) => {
            reply.push(0x01);
            reply.extend_from_slice(&ip.octets());
        }
        IpAddr::V6(ip) => {
            reply.push(0x04);
            reply.extend_from_slice(&ip.octets());
        }
    }
    reply.extend_from_slice(&bind.port().to_be_bytes());
    stream.write_all(&reply).await?;
    Ok(())
}

async fn connect_via_pool(
    target: &SocksTarget,
    pool: &WeightedPool,
) -> Result<(TcpStream, EgressTarget)> {
    let candidates = resolve_target(target, pool).await?;
    let mut last_error: Option<anyhow::Error> = None;

    for remote in candidates {
        let selections = pool.ordered_for(remote.ip());
        if selections.is_empty() {
            last_error = Some(anyhow!(
                "no selected egress adapter can reach {}",
                family_name(remote.ip())
            ));
            continue;
        }
        for selected in selections {
            match connect_bound(selected.ip, remote).await {
                Ok(stream) => return Ok((stream, selected)),
                Err(error) => {
                    last_error = Some(anyhow!(
                        "failed to connect {remote} via {} ({}): {error}",
                        selected.name,
                        selected.ip
                    ));
                }
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow!("target did not resolve to a usable address")))
}

async fn resolve_target(target: &SocksTarget, pool: &WeightedPool) -> Result<Vec<SocketAddr>> {
    let mut addresses = match target {
        SocksTarget::Ip(addr) => vec![*addr],
        SocksTarget::Domain(host, port) => lookup_host((host.as_str(), *port))
            .await
            .with_context(|| format!("failed to resolve {host}:{port}"))?
            .collect(),
    };

    addresses.retain(|addr| pool.has_family(addr.ip()));
    if addresses.is_empty() {
        return Err(anyhow!(
            "target has no address matching the selected adapter families"
        ));
    }

    addresses.sort_by_key(|addr| match addr.ip() {
        IpAddr::V4(_) => 0,
        IpAddr::V6(_) => 1,
    });
    Ok(addresses)
}

async fn connect_bound(local_ip: IpAddr, remote: SocketAddr) -> io::Result<TcpStream> {
    if local_ip.is_ipv4() != remote.is_ipv4() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "egress address family does not match remote",
        ));
    }

    let socket = match remote {
        SocketAddr::V4(_) => TcpSocket::new_v4()?,
        SocketAddr::V6(_) => TcpSocket::new_v6()?,
    };
    socket.bind(SocketAddr::new(local_ip, 0))?;
    let stream = socket.connect(remote).await?;
    let _ = stream.set_nodelay(true);
    Ok(stream)
}

struct UdpAssociationContext {
    id: u64,
    peer: SocketAddr,
    listen_ip: IpAddr,
    pool: Arc<WeightedPool>,
    cancel: CancellationToken,
    log: mpsc::Sender<String>,
    monitor: Option<mpsc::Sender<ConnectionEvent>>,
}

struct UdpRelayContext {
    id: u64,
    relay: Arc<UdpSocket>,
    client_addr: Arc<Mutex<Option<SocketAddr>>>,
    mappings: Arc<AsyncMutex<HashMap<SocketAddr, Arc<UdpSocket>>>>,
    pool: Arc<WeightedPool>,
    log: mpsc::Sender<String>,
    monitor: Option<mpsc::Sender<ConnectionEvent>>,
    cancel: CancellationToken,
}

async fn run_udp_association(mut control: TcpStream, context: UdpAssociationContext) -> Result<()> {
    let UdpAssociationContext {
        id,
        peer,
        listen_ip,
        pool,
        cancel,
        log,
        monitor,
    } = context;
    let relay_bind = relay_bind_addr(listen_ip, peer);
    let relay = Arc::new(UdpSocket::bind(relay_bind).await?);
    let relay_addr = relay.local_addr()?;
    write_reply(&mut control, 0x00, relay_addr).await?;
    log_line(
        &log,
        format!("#{id} UDP relay listening on {relay_addr} for {peer}"),
    );

    let client_addr = Arc::new(Mutex::new(None::<SocketAddr>));
    let mappings = Arc::new(AsyncMutex::new(HashMap::<SocketAddr, Arc<UdpSocket>>::new()));
    let relay_task = tokio::spawn(run_udp_relay(UdpRelayContext {
        id,
        relay: relay.clone(),
        client_addr,
        mappings: mappings.clone(),
        pool,
        log: log.clone(),
        monitor: monitor.clone(),
        cancel: cancel.clone(),
    }));

    let mut scratch = [0_u8; 1];
    tokio::select! {
        _ = cancel.cancelled() => {}
        result = control.read(&mut scratch) => {
            match result {
                Ok(_) => {}
                Err(error) => log_line(&log, format!("#{id} UDP control connection error: {error}")),
            }
        }
    }

    relay_task.abort();
    let remotes = {
        let guard = mappings.lock().await;
        guard.keys().copied().collect::<Vec<_>>()
    };
    for remote in remotes {
        emit_connection(
            &monitor,
            ConnectionEvent::Closed(ConnectionClosed {
                id: udp_flow_id(id, remote),
                up_bytes: 0,
                down_bytes: 0,
                reason: "UDP association closed".to_owned(),
                closed_at: Instant::now(),
            }),
        );
    }
    log_line(&log, format!("#{id} UDP association closed"));
    Ok(())
}

async fn run_udp_relay(context: UdpRelayContext) -> Result<()> {
    let UdpRelayContext {
        id,
        relay,
        client_addr,
        mappings,
        pool,
        log,
        monitor,
        cancel,
    } = context;
    let mut buffer = vec![0_u8; 65_535];

    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            received = relay.recv_from(&mut buffer) => {
                let (length, sender) = received?;
                if !udp_sender_allowed(&client_addr, sender) {
                    continue;
                }

                let packet = match parse_udp_packet(&buffer[..length]) {
                    Ok(packet) => packet,
                    Err(error) => {
                        log_line(&log, format!("#{id} invalid UDP packet: {error}"));
                        continue;
                    }
                };

                let targets = match resolve_target(&packet.target, &pool).await {
                    Ok(targets) => targets,
                    Err(error) => {
                        log_line(&log, format!("#{id} UDP resolve failed: {error}"));
                        continue;
                    }
                };
                let Some(remote) = targets.first().copied() else {
                    continue;
                };

                let outbound = {
                    let mut guard = mappings.lock().await;
                    if let Some(socket) = guard.get(&remote) {
                        socket.clone()
                    } else {
                        let mut bound = None;
                        for candidate in pool.ordered_for(remote.ip()) {
                            match bind_udp_socket(candidate.ip, remote).await {
                                Ok(socket) => {
                                    bound = Some((candidate, Arc::new(socket)));
                                    break;
                                }
                                Err(error) => {
                                    log_line(
                                        &log,
                                        format!(
                                            "#{id} failed to bind UDP via {} ({}): {error}",
                                            candidate.name, candidate.ip
                                        ),
                                    );
                                }
                            }
                        }
                        let Some((selected, socket)) = bound else {
                            continue;
                        };
                        log_line(&log, format!("#{id} UDP {remote} via {} ({})", selected.name, selected.ip));
                        emit_connection(
                            &monitor,
                            ConnectionEvent::Opened(ConnectionOpened {
                                id: udp_flow_id(id, remote),
                                protocol: ConnectionProtocol::Udp,
                                client: sender,
                                target: remote.to_string(),
                                egress_name: selected.name.clone(),
                                egress_ip: selected.ip,
                                opened_at: Instant::now(),
                            }),
                        );
                        spawn_udp_response_reader(id, socket.clone(), relay.clone(), client_addr.clone(), remote, log.clone());
                        guard.insert(remote, socket.clone());
                        socket
                    }
                };

                if let Err(error) = outbound.send(packet.payload).await {
                    log_line(&log, format!("#{id} UDP send to {remote} failed: {error}"));
                }
            }
        }
    }
}

fn udp_sender_allowed(client_addr: &Arc<Mutex<Option<SocketAddr>>>, sender: SocketAddr) -> bool {
    let mut guard = client_addr.lock().expect("client addr mutex poisoned");
    match *guard {
        Some(known) => known.ip() == sender.ip(),
        None => {
            *guard = Some(sender);
            true
        }
    }
}

fn spawn_udp_response_reader(
    id: u64,
    outbound: Arc<UdpSocket>,
    relay: Arc<UdpSocket>,
    client_addr: Arc<Mutex<Option<SocketAddr>>>,
    remote: SocketAddr,
    log: mpsc::Sender<String>,
) {
    tokio::spawn(async move {
        let mut buffer = vec![0_u8; 65_535];
        loop {
            let length = match outbound.recv(&mut buffer).await {
                Ok(length) => length,
                Err(error) => {
                    log_line(
                        &log,
                        format!("#{id} UDP receive from {remote} failed: {error}"),
                    );
                    return;
                }
            };

            let Some(client) = *client_addr.lock().expect("client addr mutex poisoned") else {
                continue;
            };
            let packet = build_udp_packet(remote, &buffer[..length]);
            if let Err(error) = relay.send_to(&packet, client).await {
                log_line(&log, format!("#{id} UDP send to client failed: {error}"));
                return;
            }
        }
    });
}

async fn bind_udp_socket(local_ip: IpAddr, remote: SocketAddr) -> io::Result<UdpSocket> {
    if local_ip.is_ipv4() != remote.is_ipv4() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "egress address family does not match remote",
        ));
    }
    let bind = SocketAddr::new(local_ip, 0);
    let socket = UdpSocket::bind(bind).await?;
    socket.connect(remote).await?;
    Ok(socket)
}

fn parse_udp_packet(buffer: &[u8]) -> Result<UdpPacket<'_>> {
    if buffer.len() < 4 {
        return Err(anyhow!("packet is too short"));
    }
    if buffer[0] != 0 || buffer[1] != 0 {
        return Err(anyhow!("reserved bytes are invalid"));
    }
    if buffer[2] != 0 {
        return Err(anyhow!("fragmented UDP packets are not supported"));
    }

    let atyp = buffer[3];
    let mut cursor = 4;
    let target = match atyp {
        0x01 => {
            if buffer.len() < cursor + 4 + 2 {
                return Err(anyhow!("IPv4 UDP header is too short"));
            }
            let ip = Ipv4Addr::new(
                buffer[cursor],
                buffer[cursor + 1],
                buffer[cursor + 2],
                buffer[cursor + 3],
            );
            cursor += 4;
            let port = u16::from_be_bytes([buffer[cursor], buffer[cursor + 1]]);
            cursor += 2;
            SocksTarget::Ip(SocketAddr::new(IpAddr::V4(ip), port))
        }
        0x03 => {
            if buffer.len() <= cursor {
                return Err(anyhow!("domain UDP header is too short"));
            }
            let len = buffer[cursor] as usize;
            cursor += 1;
            if buffer.len() < cursor + len + 2 {
                return Err(anyhow!("domain UDP header length is invalid"));
            }
            let domain = String::from_utf8(buffer[cursor..cursor + len].to_vec())
                .context("UDP domain is not valid UTF-8")?;
            cursor += len;
            let port = u16::from_be_bytes([buffer[cursor], buffer[cursor + 1]]);
            cursor += 2;
            SocksTarget::Domain(domain, port)
        }
        0x04 => {
            if buffer.len() < cursor + 16 + 2 {
                return Err(anyhow!("IPv6 UDP header is too short"));
            }
            let mut octets = [0_u8; 16];
            octets.copy_from_slice(&buffer[cursor..cursor + 16]);
            cursor += 16;
            let port = u16::from_be_bytes([buffer[cursor], buffer[cursor + 1]]);
            cursor += 2;
            SocksTarget::Ip(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(octets)), port))
        }
        other => return Err(anyhow!("unsupported UDP address type {other}")),
    };

    Ok(UdpPacket {
        target,
        payload: &buffer[cursor..],
    })
}

fn build_udp_packet(remote: SocketAddr, payload: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(4 + 18 + payload.len());
    output.extend_from_slice(&[0, 0, 0]);
    match remote.ip() {
        IpAddr::V4(ip) => {
            output.push(0x01);
            output.extend_from_slice(&ip.octets());
        }
        IpAddr::V6(ip) => {
            output.push(0x04);
            output.extend_from_slice(&ip.octets());
        }
    }
    output.extend_from_slice(&remote.port().to_be_bytes());
    output.extend_from_slice(payload);
    output
}

fn relay_bind_addr(listen_ip: IpAddr, peer: SocketAddr) -> SocketAddr {
    if listen_ip.is_unspecified() {
        zero_socket_addr_for(Some(peer))
    } else {
        SocketAddr::new(listen_ip, 0)
    }
}

fn zero_socket_addr_for(peer: Option<SocketAddr>) -> SocketAddr {
    match peer {
        Some(SocketAddr::V6(_)) => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
        _ => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
    }
}

#[derive(Debug)]
struct UdpPacket<'a> {
    target: SocksTarget,
    payload: &'a [u8],
}

#[derive(Debug)]
struct SocksRequest {
    command: SocksCommand,
    target: SocksTarget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SocksCommand {
    Connect,
    Bind,
    UdpAssociate,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum SocksTarget {
    Ip(SocketAddr),
    Domain(String, u16),
}

impl fmt::Display for SocksTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SocksTarget::Ip(addr) => write!(formatter, "{addr}"),
            SocksTarget::Domain(host, port) => write!(formatter, "{host}:{port}"),
        }
    }
}

#[derive(Debug)]
struct WeightedPool {
    v4: Mutex<WeightedState>,
    v6: Mutex<WeightedState>,
}

impl WeightedPool {
    fn new(targets: Vec<EgressTarget>) -> Self {
        let mut v4 = Vec::new();
        let mut v6 = Vec::new();

        for target in targets {
            match target.ip {
                IpAddr::V4(_) => v4.push(target),
                IpAddr::V6(_) => v6.push(target),
            }
        }

        Self {
            v4: Mutex::new(WeightedState::new(v4)),
            v6: Mutex::new(WeightedState::new(v6)),
        }
    }

    fn ordered_for(&self, remote_ip: IpAddr) -> Vec<EgressTarget> {
        let state = match remote_ip {
            IpAddr::V4(_) => &self.v4,
            IpAddr::V6(_) => &self.v6,
        };
        state
            .lock()
            .expect("weighted pool mutex poisoned")
            .ordered_for(remote_ip)
    }

    fn has_family(&self, remote_ip: IpAddr) -> bool {
        self.family_count(remote_ip) > 0
    }

    fn family_count(&self, remote_ip: IpAddr) -> usize {
        let state = match remote_ip {
            IpAddr::V4(_) => &self.v4,
            IpAddr::V6(_) => &self.v6,
        };
        state.lock().expect("weighted pool mutex poisoned").len()
    }

    fn describe(&self) -> String {
        let mut parts = Vec::new();
        parts.extend(
            self.v4
                .lock()
                .expect("weighted pool mutex poisoned")
                .describe(),
        );
        parts.extend(
            self.v6
                .lock()
                .expect("weighted pool mutex poisoned")
                .describe(),
        );
        if parts.is_empty() {
            "none".to_owned()
        } else {
            parts.join(", ")
        }
    }
}

#[derive(Debug)]
struct WeightedState {
    entries: Vec<WeightedEntry>,
}

impl WeightedState {
    fn new(targets: Vec<EgressTarget>) -> Self {
        Self {
            entries: targets
                .into_iter()
                .map(|target| WeightedEntry {
                    weight: i64::from(target.weight.max(1)),
                    target,
                })
                .collect(),
        }
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn ordered_for(&self, remote_ip: IpAddr) -> Vec<EgressTarget> {
        if self.entries.is_empty() {
            return Vec::new();
        }

        let total = self.entries.iter().map(|entry| entry.weight).sum::<i64>();
        let mut slot = (stable_ip_hash(remote_ip) % total as u64) as i64;
        let mut first_index = 0_usize;
        for (index, entry) in self.entries.iter().enumerate() {
            if slot < entry.weight {
                first_index = index;
                break;
            }
            slot -= entry.weight;
        }

        (0..self.entries.len())
            .map(|offset| {
                self.entries[(first_index + offset) % self.entries.len()]
                    .target
                    .clone()
            })
            .collect()
    }

    fn describe(&self) -> Vec<String> {
        self.entries
            .iter()
            .map(|entry| {
                format!(
                    "{} ({}, weight {})",
                    entry.target.name, entry.target.ip, entry.target.weight
                )
            })
            .collect()
    }
}

#[derive(Debug)]
struct WeightedEntry {
    target: EgressTarget,
    weight: i64,
}

fn stable_ip_hash(ip: IpAddr) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    let bytes: Vec<u8> = match ip {
        IpAddr::V4(addr) => addr.octets().to_vec(),
        IpAddr::V6(addr) => addr.octets().to_vec(),
    };

    bytes.into_iter().fold(FNV_OFFSET, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(FNV_PRIME)
    })
}

fn family_name(ip: IpAddr) -> &'static str {
    match ip {
        IpAddr::V4(_) => "IPv4",
        IpAddr::V6(_) => "IPv6",
    }
}

fn udp_flow_id(id: u64, remote: SocketAddr) -> String {
    format!("{id}/udp/{remote}")
}

fn emit_connection(monitor: &Option<mpsc::Sender<ConnectionEvent>>, event: ConnectionEvent) {
    if let Some(monitor) = monitor {
        let _ = monitor.send(event);
    }
}

fn log_line(log: &mpsc::Sender<String>, message: impl Into<String>) {
    let _ = log.send(message.into());
}
