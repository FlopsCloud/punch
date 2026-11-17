use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU16, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, watch, Notify};
use tokio::time::timeout;

use crate::bench;
use crate::{debug, error, info, warn};
use crate::config::TcpTune;
use crate::ioframe::{read_assign, read_msg, write_msg};
use crate::netutil::{classify_dial, connect_tuned, dial};
use crate::proto::{encode_data_hello, CtrlMsg, DataHello, DATA_BOUND, DATA_IDLE, PROTO_VERSION};
use crate::relay::relay;
use crate::util::lock;

#[derive(Clone, Debug)]
pub struct PortMap {
    pub bind_ip: String,
    pub bind_port: u16,
    pub target_host: String,
    pub target_port: u16,
}

#[derive(Clone, Debug)]
pub struct Options {
    pub server: SocketAddr,
    pub api_key: String,
    pub name: String,
    pub maps: Vec<PortMap>,
    pub bench: Option<SocketAddr>,
    pub pool: Option<u16>,
}

pub async fn run(opts: Options, mut shutdown: watch::Receiver<bool>) -> Result<()> {
    if let Some(addr) = opts.bench {
        let bound = bench::serve(addr, shutdown.clone()).await?;
        info!("bench http listening {bound}");
        println!("bench http  http://{bound}/bench?size=1048576");
    }
    let mut backoff = Duration::from_secs(1);
    loop {
        if *shutdown.borrow() {
            return Ok(());
        }
        match session(&opts, shutdown.clone()).await {
            Ok(()) => return Ok(()),
            Err(err) => {
                warn!("control disconnected, retrying: {err:#}");
                tokio::select! {
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() {
                            return Ok(());
                        }
                    }
                    _ = tokio::time::sleep(backoff) => {}
                }
                backoff = (backoff * 2).min(Duration::from_secs(15));
            }
        }
    }
}

struct Session {
    ctrl: mpsc::UnboundedSender<CtrlMsg>,
    hello: Mutex<Option<oneshot::Sender<Result<(), String>>>>,
    map_wait: Mutex<HashMap<u32, oneshot::Sender<CtrlMsg>>>,
    client_id: AtomicU64,
    token: Mutex<[u8; 16]>,
    data_addr: Mutex<Option<SocketAddr>>,
    pool_idle: AtomicU16,
    pool_max: AtomicU16,
    tune: TcpTune,
    server_ip: IpAddr,
    next_req: AtomicU32,
}

struct CancelOnDrop(watch::Sender<bool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        let _ = self.0.send(true);
    }
}

async fn session(opts: &Options, mut shutdown: watch::Receiver<bool>) -> Result<()> {
    let sock = timeout(
        Duration::from_secs(10),
        connect_tuned(opts.server, &TcpTune::default()),
    )
    .await
    .context("连接服务端超时")?
    .context("连接服务端失败")?;
    let (mut rd, mut wr) = sock.into_split();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let _cancel = CancelOnDrop(cancel_tx.clone());
    let session = Arc::new(Session {
        ctrl: tx,
        hello: Mutex::new(None),
        map_wait: Mutex::new(HashMap::new()),
        client_id: AtomicU64::new(0),
        token: Mutex::new([0u8; 16]),
        data_addr: Mutex::new(None),
        pool_idle: AtomicU16::new(0),
        pool_max: AtomicU16::new(0),
        tune: TcpTune::default(),
        server_ip: opts.server.ip(),
        next_req: AtomicU32::new(1),
    });

    let mut writer_stop = cancel_rx.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                changed = writer_stop.changed() => {
                    if changed.is_err() || *writer_stop.borrow() {
                        break;
                    }
                }
                msg = rx.recv() => {
                    match msg {
                        Some(msg) => {
                            if write_msg(&mut wr, &msg).await.is_err() {
                                let _ = writer_stop.borrow();
                                break;
                            }
                        }
                        None => break,
                    }
                }
            }
        }
        let _ = cancel_tx.send(true);
    });

    let reader = {
        let session = session.clone();
        let mut stop = cancel_rx.clone();
        tokio::spawn(async move { read_loop(&session, &mut rd, &mut stop).await })
    };

    let (hello_tx, hello_rx) = oneshot::channel();
    *lock(&session.hello) = Some(hello_tx);
    session
        .ctrl
        .send(CtrlMsg::Hello {
            version: PROTO_VERSION,
            name: opts.name.clone(),
            api_key: opts.api_key.clone(),
        })
        .map_err(|_| anyhow!("control writer closed"))?;
    timeout(Duration::from_secs(10), hello_rx)
        .await
        .context("等待服务端握手超时")?
        .map_err(|_| anyhow!("handshake cancelled"))?
        .map_err(|err| anyhow!(err))?;

    let advertised = session.pool_idle.load(Ordering::Relaxed).max(1);
    let max = session.pool_max.load(Ordering::Relaxed).max(advertised);
    let mut target = opts.pool.unwrap_or(advertised);
    if target > max {
        target = max;
    }
    if target == 0 {
        target = 1;
    }
    info!(
        "control connected client={} data={:?} pool={target}",
        opts.name,
        lock(&session.data_addr)
    );
    let pool_stop = cancel_rx;
    let pool_session = session.clone();
    tokio::spawn(async move {
        run_pool(pool_session, target as usize, pool_stop).await;
    });

    for map in &opts.maps {
        register_map(&session, map).await;
    }
    let _ = session.ctrl.send(CtrlMsg::SyncDone);

    tokio::select! {
        changed = shutdown.changed() => {
            if changed.is_err() || *shutdown.borrow() {
                return Ok(());
            }
        }
        res = reader => {
            if *shutdown.borrow() {
                return Ok(());
            }
            return match res {
                Ok(Ok(())) => Err(anyhow!("control closed")),
                Ok(Err(err)) => Err(err.into()),
                Err(err) => Err(err.into()),
            };
        }
    }
    Ok(())
}

async fn read_loop(
    session: &Arc<Session>,
    rd: &mut tokio::net::tcp::OwnedReadHalf,
    stop: &mut watch::Receiver<bool>,
) -> io::Result<()> {
    let mut last = std::time::Instant::now();
    let mut ticker = tokio::time::interval(Duration::from_secs(15));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    return Ok(());
                }
            }
            _ = ticker.tick() => {
                if last.elapsed() > Duration::from_secs(45) {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "control heartbeat timeout"));
                }
            }
            msg = read_msg(rd) => {
                let msg = msg?;
                last = std::time::Instant::now();
                handle_server(session, msg);
            }
        }
    }
}

fn handle_server(session: &Arc<Session>, msg: CtrlMsg) {
    let forward = match &msg {
        CtrlMsg::MapAddResult { req_id, .. } => Some(*req_id),
        _ => None,
    };
    if let Some(req_id) = forward {
        if let Some(tx) = lock(&session.map_wait).remove(&req_id) {
            let _ = tx.send(msg);
        }
        return;
    }
    match msg {
        CtrlMsg::HelloOk {
            client_id,
            data_port,
            token,
            pool_idle,
            pool_max,
        } => {
            session.client_id.store(client_id, Ordering::Relaxed);
            *lock(&session.token) = token;
            *lock(&session.data_addr) = Some(SocketAddr::new(session.server_ip, data_port));
            session.pool_idle.store(pool_idle, Ordering::Relaxed);
            session.pool_max.store(pool_max, Ordering::Relaxed);
            if let Some(tx) = lock(&session.hello).take() {
                let _ = tx.send(Ok(()));
            }
        }
        CtrlMsg::HelloErr { message } => {
            if let Some(tx) = lock(&session.hello).take() {
                let _ = tx.send(Err(message));
            }
        }
        CtrlMsg::Ping { ts_ms } => {
            let _ = session.ctrl.send(CtrlMsg::Pong { ts_ms });
        }
        CtrlMsg::Pong { .. } | CtrlMsg::SyncDone | CtrlMsg::MapDel { .. } => {}
        CtrlMsg::MapOpen {
            map_id,
            target_host,
            target_port,
            ..
        } => {
            let ctrl = session.ctrl.clone();
            tokio::spawn(async move {
                let (status, message) = probe(&target_host, target_port).await;
                let _ = ctrl.send(CtrlMsg::MapOpenAck {
                    map_id,
                    status,
                    message,
                });
            });
        }
        CtrlMsg::OpenStream {
            stream_id,
            map_id,
            target_host,
            target_port,
        } => {
            let session = session.clone();
            tokio::spawn(async move {
                if let Err(message) =
                    open_bound(&session, stream_id, &target_host, target_port).await
                {
                    let _ = session.ctrl.send(CtrlMsg::StreamFail {
                        stream_id,
                        map_id,
                        message,
                    });
                }
            });
        }
        other => {
            debug!("ignore server message {other:?}");
        }
    }
}

async fn register_map(session: &Session, map: &PortMap) {
    let req_id = session.next_req.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = oneshot::channel();
    lock(&session.map_wait).insert(req_id, tx);
    if session
        .ctrl
        .send(CtrlMsg::MapAdd {
            req_id,
            bind_ip: map.bind_ip.clone(),
            bind_port: map.bind_port,
            target_host: map.target_host.clone(),
            target_port: map.target_port,
        })
        .is_err()
    {
        return;
    }
    let result = timeout(Duration::from_secs(5), rx).await;
    let msg = match result {
        Ok(Ok(msg)) => msg,
        _ => {
            lock(&session.map_wait).remove(&req_id);
            error!(
                "服务端没有确认这条映射 {}:{}",
                map.bind_ip, map.bind_port
            );
            return;
        }
    };
    if let CtrlMsg::MapAddResult {
        ok,
        map_id,
        message,
        ..
    } = msg
    {
        if ok {
            info!("map {map_id} {message}");
            println!(
                "map {map_id}  {bind} -> {target}  {message}",
                bind = format!("{}:{}", map.bind_ip, map.bind_port),
                target = format!("{}:{}", map.target_host, map.target_port),
            );
            let ctrl = session.ctrl.clone();
            let host = map.target_host.clone();
            let port = map.target_port;
            tokio::spawn(async move {
                let (status, message) = probe(&host, port).await;
                if status != 0 {
                    warn!("map {map_id} {message}");
                    let _ = ctrl.send(CtrlMsg::ListenFail { map_id, message });
                }
            });
        } else {
            error!(
                "map 失败 {}:{} {message}",
                map.bind_ip, map.bind_port
            );
            println!(
                "map 失败  {}:{}  {message}",
                map.bind_ip, map.bind_port
            );
        }
    }
}

async fn probe(host: &str, port: u16) -> (u8, String) {
    match timeout(Duration::from_millis(500), TcpStream::connect((host, port))).await {
        Ok(Ok(sock)) => {
            drop(sock);
            (0, String::new())
        }
        Ok(Err(err)) => (1, classify_dial(&err)),
        Err(_) => (
            1,
            format!("target unreachable: dial {host}:{port} timed out"),
        ),
    }
}

async fn open_bound(
    session: &Session,
    stream_id: u32,
    host: &str,
    port: u16,
) -> Result<(), String> {
    let target = dial_target(&session.tune, host, port).await?;
    let addr = lock(&session.data_addr).ok_or_else(|| "data channel unknown".to_string())?;
    let mut data = connect_tuned(addr, &session.tune)
        .await
        .map_err(|err| format!("target unreachable: data channel: {err}"))?;
    let hello = DataHello {
        kind: DATA_BOUND,
        client_id: session.client_id.load(Ordering::Relaxed),
        token: *lock(&session.token),
        stream_id,
    };
    data.write_all(&encode_data_hello(&hello))
        .await
        .map_err(|err| format!("target unreachable: {err}"))?;
    let _ = relay(data, target, session.tune.pipe_size).await;
    Ok(())
}

async fn dial_target(tune: &TcpTune, host: &str, port: u16) -> Result<TcpStream, String> {
    match timeout(Duration::from_secs(10), dial(host, port, tune)).await {
        Ok(Ok(sock)) => Ok(sock),
        Ok(Err(err)) => Err(classify_dial(&err)),
        Err(_) => Err(format!("target unreachable: dial {host}:{port} timed out")),
    }
}

struct Slot {
    idle: Arc<AtomicUsize>,
    notify: Arc<Notify>,
    armed: bool,
}

impl Slot {
    fn release(&mut self) {
        if self.armed {
            self.armed = false;
            self.idle.fetch_sub(1, Ordering::Relaxed);
            self.notify.notify_one();
        }
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.release();
    }
}

enum IdleOutcome {
    Assigned(TcpStream, u32, u32, u16, String),
    Failed,
    Cancelled,
}

async fn run_pool(session: Arc<Session>, target: usize, mut cancel: watch::Receiver<bool>) {
    let idle = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(Notify::new());
    loop {
        if *cancel.borrow() {
            break;
        }
        if idle.load(Ordering::Relaxed) >= target {
            tokio::select! {
                changed = cancel.changed() => {
                    if changed.is_err() || *cancel.borrow() {
                        break;
                    }
                }
                _ = notify.notified() => {}
            }
            continue;
        }
        idle.fetch_add(1, Ordering::Relaxed);
        let slot = Slot {
            idle: idle.clone(),
            notify: notify.clone(),
            armed: true,
        };
        let session = session.clone();
        let mut stop = cancel.clone();
        tokio::spawn(async move {
            hold_slot(session, slot, &mut stop).await;
        });
    }
}

async fn hold_slot(session: Arc<Session>, mut slot: Slot, cancel: &mut watch::Receiver<bool>) {
    match connect_idle(&session, cancel).await {
        IdleOutcome::Assigned(sock, _stream_id, map_id, port, host) => {
            slot.release();
            if let Err(message) = bridge_idle(&session, sock, &host, port).await {
                warn!("map {map_id} {message}");
                let _ = session.ctrl.send(CtrlMsg::ListenFail { map_id, message });
            }
        }
        IdleOutcome::Failed => {
            if !*cancel.borrow() {
                tokio::select! {
                    _ = cancel.changed() => {}
                    _ = tokio::time::sleep(Duration::from_millis(300)) => {}
                }
            }
        }
        IdleOutcome::Cancelled => {}
    }
}

async fn connect_idle(session: &Session, cancel: &mut watch::Receiver<bool>) -> IdleOutcome {
    if *cancel.borrow() {
        return IdleOutcome::Cancelled;
    }
    let Some(addr) = *lock(&session.data_addr) else {
        return IdleOutcome::Failed;
    };
    let mut sock = match connect_tuned(addr, &session.tune).await {
        Ok(sock) => sock,
        Err(_) => return IdleOutcome::Failed,
    };
    let hello = DataHello {
        kind: DATA_IDLE,
        client_id: session.client_id.load(Ordering::Relaxed),
        token: *lock(&session.token),
        stream_id: 0,
    };
    if sock.write_all(&encode_data_hello(&hello)).await.is_err() {
        return IdleOutcome::Failed;
    }
    tokio::select! {
        changed = cancel.changed() => {
            if changed.is_err() || *cancel.borrow() {
                IdleOutcome::Cancelled
            } else {
                IdleOutcome::Failed
            }
        }
        res = read_assign(&mut sock) => {
            match res {
                Ok((stream_id, map_id, port, host)) => {
                    IdleOutcome::Assigned(sock, stream_id, map_id, port, host)
                }
                Err(_) => IdleOutcome::Failed,
            }
        }
    }
}

async fn bridge_idle(session: &Session, sock: TcpStream, host: &str, port: u16) -> Result<(), String> {
    let target = dial_target(&session.tune, host, port).await?;
    let _ = relay(sock, target, session.tune.pipe_size).await;
    Ok(())
}
