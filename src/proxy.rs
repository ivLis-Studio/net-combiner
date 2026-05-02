use std::collections::HashMap;
use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpSocket, TcpStream, UdpSocket, lookup_host};
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinError;
use tokio_util::sync::CancellationToken;

use crate::adapter::EgressTarget;

const CONNECTION_UPDATE_INTERVAL: Duration = Duration::from_millis(500);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const LOAD_BYTE_UNIT: u64 = 256 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    pub listen_ip: IpAddr,
    pub listen_port: u16,
    pub egress: Vec<EgressTarget>,
    pub udp_enabled: bool,
    pub egress_strategy: EgressStrategy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum EgressStrategy {
    PerConnection,
    #[default]
    PerDestination,
}

impl EgressStrategy {
    pub fn label(self) -> &'static str {
        match self {
            Self::PerConnection => "per-connection load balance",
            Self::PerDestination => "sticky per destination IP",
        }
    }
}

#[derive(Debug, Clone)]
pub enum ConnectionEvent {
    Opened(ConnectionOpened),
    Updated(ConnectionUpdated),
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
pub struct ConnectionUpdated {
    pub id: String,
    pub up_bytes: u64,
    pub down_bytes: u64,
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

    let pool = Arc::new(WeightedPool::new(
        config.egress.clone(),
        config.egress_strategy,
    ));
    log_line(
        &log,
        format!("selected egress adapters: {}", pool.describe()),
    );
    log_line(
        &log,
        format!("egress policy: {}", config.egress_strategy.label()),
    );

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
            let (outbound, selected, remote) =
                match connect_via_pool(&request.target, &context.pool).await {
                    Ok(value) => value,
                    Err(error) => {
                        let _ = write_reply(
                            &mut inbound,
                            0x04,
                            zero_socket_addr_for(Some(context.peer)),
                        )
                        .await;
                        return Err(error.context(format!("CONNECT {target_label} failed")));
                    }
                };
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
            context.pool.record_open(remote.ip(), egress_ip);

            let (up_bytes, down_bytes, reason) = relay_tcp(
                inbound,
                outbound,
                TcpRelayContext {
                    id: connection_id.clone(),
                    cancel: context.cancel,
                    monitor: context.monitor.clone(),
                    pool: context.pool.clone(),
                    remote_ip: remote.ip(),
                    egress_ip,
                },
            )
            .await;
            context
                .pool
                .record_close(remote.ip(), egress_ip, up_bytes + down_bytes);
            log_line(
                &context.log,
                format!(
                    "#{} closed: {up_bytes} bytes up, {down_bytes} bytes down ({reason})",
                    context.id
                ),
            );
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
) -> Result<(TcpStream, EgressTarget, SocketAddr)> {
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
            match tokio::time::timeout(CONNECT_TIMEOUT, connect_bound(selected.ip, remote)).await {
                Ok(Ok(stream)) => return Ok((stream, selected, remote)),
                Ok(Err(error)) => {
                    last_error = Some(anyhow!(
                        "failed to connect {remote} via {} ({}): {error}",
                        selected.name,
                        selected.ip
                    ));
                }
                Err(_) => {
                    last_error = Some(anyhow!(
                        "timed out connecting {remote} via {} ({}) after {} ms",
                        selected.name,
                        selected.ip,
                        CONNECT_TIMEOUT.as_millis()
                    ));
                }
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow!("target did not resolve to a usable address")))
}

struct TcpRelayContext {
    id: String,
    cancel: CancellationToken,
    monitor: Option<mpsc::Sender<ConnectionEvent>>,
    pool: Arc<WeightedPool>,
    remote_ip: IpAddr,
    egress_ip: IpAddr,
}

async fn relay_tcp(
    inbound: TcpStream,
    outbound: TcpStream,
    context: TcpRelayContext,
) -> (u64, u64, String) {
    let (client_read, client_write) = inbound.into_split();
    let (server_read, server_write) = outbound.into_split();

    let up_counter = Arc::new(AtomicU64::new(0));
    let down_counter = Arc::new(AtomicU64::new(0));
    let mut up_task = tokio::spawn(copy_counted(client_read, server_write, up_counter.clone()));
    let mut down_task = tokio::spawn(copy_counted(
        server_read,
        client_write,
        down_counter.clone(),
    ));
    let mut up_done = false;
    let mut down_done = false;
    let mut last_recorded_total = 0_u64;
    let mut ticker = tokio::time::interval(CONNECTION_UPDATE_INTERVAL);

    loop {
        tokio::select! {
            _ = context.cancel.cancelled() => {
                up_task.abort();
                down_task.abort();
                let (up, down) = emit_transfer_snapshot(&context, &up_counter, &down_counter, &mut last_recorded_total);
                return (up, down, "cancelled".to_owned());
            }
            _ = ticker.tick() => {
                emit_transfer_snapshot(&context, &up_counter, &down_counter, &mut last_recorded_total);
            }
            result = &mut up_task, if !up_done => {
                match copy_task_result(result) {
                    Ok(_) => {
                        up_done = true;
                        emit_transfer_snapshot(&context, &up_counter, &down_counter, &mut last_recorded_total);
                    }
                    Err(error) => {
                        down_task.abort();
                        let (up, down) = emit_transfer_snapshot(&context, &up_counter, &down_counter, &mut last_recorded_total);
                        return (up, down, format!("relay error: {error}"));
                    }
                }
            }
            result = &mut down_task, if !down_done => {
                match copy_task_result(result) {
                    Ok(_) => {
                        down_done = true;
                        emit_transfer_snapshot(&context, &up_counter, &down_counter, &mut last_recorded_total);
                    }
                    Err(error) => {
                up_task.abort();
                        let (up, down) = emit_transfer_snapshot(&context, &up_counter, &down_counter, &mut last_recorded_total);
                        return (up, down, format!("relay error: {error}"));
                    }
                }
            }
        }

        if up_done && down_done {
            let (up, down) = emit_transfer_snapshot(
                &context,
                &up_counter,
                &down_counter,
                &mut last_recorded_total,
            );
            return (up, down, "closed".to_owned());
        }
    }
}

async fn copy_counted<R, W>(
    mut reader: R,
    mut writer: W,
    counter: Arc<AtomicU64>,
) -> io::Result<u64>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buffer = [0_u8; 32 * 1024];
    let mut total = 0_u64;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            writer.shutdown().await?;
            return Ok(total);
        }
        writer.write_all(&buffer[..read]).await?;
        total += read as u64;
        counter.store(total, Ordering::Relaxed);
    }
}

fn copy_task_result(result: std::result::Result<io::Result<u64>, JoinError>) -> io::Result<u64> {
    match result {
        Ok(value) => value,
        Err(error) => Err(io::Error::other(error)),
    }
}

fn emit_transfer_snapshot(
    context: &TcpRelayContext,
    up_counter: &AtomicU64,
    down_counter: &AtomicU64,
    last_recorded_total: &mut u64,
) -> (u64, u64) {
    let up = up_counter.load(Ordering::Relaxed);
    let down = down_counter.load(Ordering::Relaxed);
    let total = up + down;
    if total > *last_recorded_total {
        context.pool.record_transfer(
            context.remote_ip,
            context.egress_ip,
            total - *last_recorded_total,
        );
        *last_recorded_total = total;
    }
    emit_connection(
        &context.monitor,
        ConnectionEvent::Updated(ConnectionUpdated {
            id: context.id.clone(),
            up_bytes: up,
            down_bytes: down,
        }),
    );
    (up, down)
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
    mappings: Arc<AsyncMutex<HashMap<SocketAddr, Arc<UdpFlow>>>>,
    pool: Arc<WeightedPool>,
    log: mpsc::Sender<String>,
    monitor: Option<mpsc::Sender<ConnectionEvent>>,
    cancel: CancellationToken,
}

struct UdpFlow {
    socket: Arc<UdpSocket>,
    egress_ip: IpAddr,
    up_bytes: AtomicU64,
    down_bytes: AtomicU64,
}

struct UdpResponseReaderContext {
    id: u64,
    flow: Arc<UdpFlow>,
    relay: Arc<UdpSocket>,
    client_addr: Arc<Mutex<Option<SocketAddr>>>,
    remote: SocketAddr,
    pool: Arc<WeightedPool>,
    monitor: Option<mpsc::Sender<ConnectionEvent>>,
    log: mpsc::Sender<String>,
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
    let mappings = Arc::new(AsyncMutex::new(HashMap::<SocketAddr, Arc<UdpFlow>>::new()));
    let association_cancel = cancel.child_token();
    let relay_task = tokio::spawn(run_udp_relay(UdpRelayContext {
        id,
        relay: relay.clone(),
        client_addr,
        mappings: mappings.clone(),
        pool: pool.clone(),
        log: log.clone(),
        monitor: monitor.clone(),
        cancel: association_cancel.clone(),
    }));

    let mut scratch = [0_u8; 1];
    tokio::select! {
        _ = association_cancel.cancelled() => {}
        result = control.read(&mut scratch) => {
            match result {
                Ok(_) => {}
                Err(error) => log_line(&log, format!("#{id} UDP control connection error: {error}")),
            }
        }
    }

    association_cancel.cancel();
    relay_task.abort();
    let _ = relay_task.await;
    let remotes = {
        let guard = mappings.lock().await;
        guard
            .iter()
            .map(|(remote, flow)| (*remote, flow.clone()))
            .collect::<Vec<_>>()
    };
    for (remote, flow) in remotes {
        let up_bytes = flow.up_bytes.load(Ordering::Relaxed);
        let down_bytes = flow.down_bytes.load(Ordering::Relaxed);
        pool.record_close(remote.ip(), flow.egress_ip, up_bytes + down_bytes);
        emit_connection(
            &monitor,
            ConnectionEvent::Closed(ConnectionClosed {
                id: udp_flow_id(id, remote),
                up_bytes,
                down_bytes,
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

            let flow = {
                let mut guard = mappings.lock().await;
                if let Some(flow) = guard.get(&remote) {
                    flow.clone()
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
                    pool.record_open(remote.ip(), selected.ip);
                    log_line(&log, format!("#{id} UDP {remote} via {} ({})", selected.name, selected.ip));
                    let flow = Arc::new(UdpFlow {
                        socket,
                        egress_ip: selected.ip,
                        up_bytes: AtomicU64::new(0),
                        down_bytes: AtomicU64::new(0),
                    });
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
                        spawn_udp_response_reader(UdpResponseReaderContext {
                            id,
                            flow: flow.clone(),
                            relay: relay.clone(),
                            client_addr: client_addr.clone(),
                            remote,
                            pool: pool.clone(),
                            monitor: monitor.clone(),
                            log: log.clone(),
                            cancel: cancel.clone(),
                        });
                        guard.insert(remote, flow.clone());
                        flow
                    }
                };

                if let Err(error) = flow.socket.send(packet.payload).await {
                    log_line(&log, format!("#{id} UDP send to {remote} failed: {error}"));
                } else {
                    let sent = packet.payload.len() as u64;
                    let up_bytes = flow.up_bytes.fetch_add(sent, Ordering::Relaxed) + sent;
                    let down_bytes = flow.down_bytes.load(Ordering::Relaxed);
                    pool.record_transfer(remote.ip(), flow.egress_ip, sent);
                    emit_connection(
                        &monitor,
                        ConnectionEvent::Updated(ConnectionUpdated {
                            id: udp_flow_id(id, remote),
                            up_bytes,
                            down_bytes,
                        }),
                    );
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

fn spawn_udp_response_reader(context: UdpResponseReaderContext) {
    tokio::spawn(async move {
        let UdpResponseReaderContext {
            id,
            flow,
            relay,
            client_addr,
            remote,
            pool,
            monitor,
            log,
            cancel,
        } = context;
        let mut buffer = vec![0_u8; 65_535];
        loop {
            let length = tokio::select! {
                _ = cancel.cancelled() => return,
                result = flow.socket.recv(&mut buffer) => {
                    match result {
                        Ok(length) => length,
                        Err(error) => {
                            log_line(
                                &log,
                                format!("#{id} UDP receive from {remote} failed: {error}"),
                            );
                            return;
                        }
                    }
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
            let received = length as u64;
            let up_bytes = flow.up_bytes.load(Ordering::Relaxed);
            let down_bytes = flow.down_bytes.fetch_add(received, Ordering::Relaxed) + received;
            pool.record_transfer(remote.ip(), flow.egress_ip, received);
            emit_connection(
                &monitor,
                ConnectionEvent::Updated(ConnectionUpdated {
                    id: udp_flow_id(id, remote),
                    up_bytes,
                    down_bytes,
                }),
            );
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
    strategy: EgressStrategy,
}

impl WeightedPool {
    fn new(targets: Vec<EgressTarget>, strategy: EgressStrategy) -> Self {
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
            strategy,
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
            .ordered_for(remote_ip, self.strategy)
    }

    fn record_open(&self, remote_ip: IpAddr, egress_ip: IpAddr) {
        let state = match remote_ip {
            IpAddr::V4(_) => &self.v4,
            IpAddr::V6(_) => &self.v6,
        };
        state
            .lock()
            .expect("weighted pool mutex poisoned")
            .record_open(remote_ip, egress_ip, self.strategy);
    }

    fn record_transfer(&self, remote_ip: IpAddr, egress_ip: IpAddr, bytes: u64) {
        let state = match remote_ip {
            IpAddr::V4(_) => &self.v4,
            IpAddr::V6(_) => &self.v6,
        };
        state
            .lock()
            .expect("weighted pool mutex poisoned")
            .record_transfer(remote_ip, egress_ip, bytes);
    }

    fn record_close(&self, remote_ip: IpAddr, egress_ip: IpAddr, bytes: u64) {
        let state = match remote_ip {
            IpAddr::V4(_) => &self.v4,
            IpAddr::V6(_) => &self.v6,
        };
        state
            .lock()
            .expect("weighted pool mutex poisoned")
            .record_close(remote_ip, egress_ip, bytes, self.strategy);
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
    destination_map: HashMap<IpAddr, usize>,
    destination_refs: HashMap<IpAddr, u64>,
    cursor: usize,
}

impl WeightedState {
    fn new(targets: Vec<EgressTarget>) -> Self {
        Self {
            entries: targets
                .into_iter()
                .map(|target| WeightedEntry {
                    weight: u64::from(target.weight.max(1)),
                    active_connections: 0,
                    active_bytes: 0,
                    target,
                })
                .collect(),
            destination_map: HashMap::new(),
            destination_refs: HashMap::new(),
            cursor: 0,
        }
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn ordered_for(&mut self, remote_ip: IpAddr, strategy: EgressStrategy) -> Vec<EgressTarget> {
        if self.entries.is_empty() {
            return Vec::new();
        }

        let first_index = match strategy {
            EgressStrategy::PerDestination => self
                .destination_map
                .get(&remote_ip)
                .copied()
                .filter(|index| *index < self.entries.len())
                .unwrap_or_else(|| self.pick_least_loaded()),
            EgressStrategy::PerConnection => self.pick_least_loaded(),
        };
        self.cursor = (first_index + 1) % self.entries.len();

        let mut order = (0..self.entries.len()).collect::<Vec<_>>();
        order.sort_by_key(|index| {
            if *index == first_index {
                (0_u8, 0_u128, 0_usize)
            } else {
                (
                    1_u8,
                    self.load_score(*index),
                    ring_distance(first_index, *index, self.entries.len()),
                )
            }
        });
        order
            .into_iter()
            .map(|index| self.entries[index].target.clone())
            .collect()
    }

    fn pick_least_loaded(&self) -> usize {
        (0..self.entries.len())
            .min_by_key(|index| {
                (
                    self.load_score(*index),
                    ring_distance(self.cursor, *index, self.entries.len()),
                )
            })
            .unwrap_or(0)
    }

    fn load_score(&self, index: usize) -> u128 {
        let entry = &self.entries[index];
        let normalized_active = u128::from(entry.active_connections) * 1_000_000;
        let normalized_bytes = u128::from(entry.active_bytes / LOAD_BYTE_UNIT);
        (normalized_active + normalized_bytes) / u128::from(entry.weight.max(1))
    }

    fn record_open(&mut self, remote_ip: IpAddr, egress_ip: IpAddr, strategy: EgressStrategy) {
        if let Some(index) = self.index_for(egress_ip) {
            self.entries[index].active_connections += 1;
            if matches!(strategy, EgressStrategy::PerDestination) {
                self.destination_map.insert(remote_ip, index);
                *self.destination_refs.entry(remote_ip).or_insert(0) += 1;
            }
        }
    }

    fn record_transfer(&mut self, _remote_ip: IpAddr, egress_ip: IpAddr, bytes: u64) {
        if let Some(index) = self.index_for(egress_ip) {
            self.entries[index].active_bytes =
                self.entries[index].active_bytes.saturating_add(bytes);
        }
    }

    fn record_close(
        &mut self,
        remote_ip: IpAddr,
        egress_ip: IpAddr,
        bytes: u64,
        strategy: EgressStrategy,
    ) {
        if let Some(index) = self.index_for(egress_ip) {
            self.entries[index].active_connections =
                self.entries[index].active_connections.saturating_sub(1);
            self.entries[index].active_bytes =
                self.entries[index].active_bytes.saturating_sub(bytes);
        }

        if matches!(strategy, EgressStrategy::PerDestination) {
            if let Some(count) = self.destination_refs.get_mut(&remote_ip) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    self.destination_refs.remove(&remote_ip);
                    self.destination_map.remove(&remote_ip);
                }
            }
        }
    }

    fn index_for(&self, ip: IpAddr) -> Option<usize> {
        self.entries.iter().position(|entry| entry.target.ip == ip)
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
    weight: u64,
    active_connections: u64,
    active_bytes: u64,
}

fn ring_distance(start: usize, index: usize, len: usize) -> usize {
    if len == 0 {
        0
    } else {
        (index + len - start) % len
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn target(name: &str, last_octet: u8) -> EgressTarget {
        EgressTarget {
            name: name.to_owned(),
            ip: IpAddr::V4(Ipv4Addr::new(192, 0, 2, last_octet)),
            weight: 1,
        }
    }

    #[test]
    fn per_connection_prefers_idle_adapter() {
        let first = target("first", 10);
        let second = target("second", 11);
        let mut state = WeightedState::new(vec![first.clone(), second.clone()]);

        let remote_a = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 10));
        let selected = state.ordered_for(remote_a, EgressStrategy::PerConnection)[0].ip;
        state.record_open(remote_a, selected, EgressStrategy::PerConnection);

        let remote_b = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 11));
        let next = state.ordered_for(remote_b, EgressStrategy::PerConnection)[0].ip;

        assert_eq!(next, second.ip);
    }

    #[test]
    fn per_destination_stays_sticky_until_last_flow_closes() {
        let first = target("first", 10);
        let second = target("second", 11);
        let mut state = WeightedState::new(vec![first.clone(), second]);
        let remote = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 20));

        let selected = state.ordered_for(remote, EgressStrategy::PerDestination)[0].ip;
        state.record_open(remote, selected, EgressStrategy::PerDestination);
        state.record_open(remote, selected, EgressStrategy::PerDestination);

        let sticky = state.ordered_for(remote, EgressStrategy::PerDestination)[0].ip;
        assert_eq!(sticky, selected);

        state.record_close(remote, selected, 0, EgressStrategy::PerDestination);
        assert!(state.destination_map.contains_key(&remote));

        state.record_close(remote, selected, 0, EgressStrategy::PerDestination);
        assert!(!state.destination_map.contains_key(&remote));
    }

    #[test]
    fn parses_udp_domain_packet() {
        let mut packet = vec![0, 0, 0, 0x03, 11];
        packet.extend_from_slice(b"example.com");
        packet.extend_from_slice(&443_u16.to_be_bytes());
        packet.extend_from_slice(b"payload");

        let parsed = parse_udp_packet(&packet).expect("valid UDP packet");

        assert_eq!(
            parsed.target,
            SocksTarget::Domain("example.com".to_owned(), 443)
        );
        assert_eq!(parsed.payload, b"payload");
    }
}
