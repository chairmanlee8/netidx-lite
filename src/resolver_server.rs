use crate::{
    channel::Channel,
    path::Path,
    resolver_store::Store,
};
use async_std::{
    prelude::*,
    task, future, stream,
    net::{TcpStream, TcpListener}
};
use futures::{
    channel::oneshot,
    future::FutureExt as _,
};
use std::{
    mem, io,
    sync::{Arc, atomic::{AtomicUsize, Ordering}},
    time::Duration,
    net::SocketAddr,
};
use serde::Serialize;
use failure::Error;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum ClientHello {
    ReadOnly,
    WriteOnly { ttl: u64, write_addr: SocketAddr }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ServerHello { pub ttl_expired: bool }

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum ToResolver {
    Resolve(Vec<Path>),
    List(Path),
    Publish(Vec<Path>),
    Unpublish(Vec<Path>),
    Clear,
    Heartbeat
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum FromResolver {
    Resolved(Vec<Vec<SocketAddr>>),
    List(Vec<Path>),
    Published,
    Unpublished,
    Error(String)
}

type ClientInfo = Option<oneshot::Sender<()>>;

fn handle_batch(
    store: &Store<ClientInfo>,
    msgs: impl Iterator<Item = ToResolver>,
    con: &mut Channel,
    wa: Option<SocketAddr>
) -> Result<(), Error> {
    match wa {
        None => {
            let s = store.read();
            for m in msgs {
                match m {
                    ToResolver::Heartbeat => (),
                    ToResolver::Resolve(paths) => {
                        let res = paths.iter().map(|p| s.resolve(p)).collect();
                        con.queue_send(&FromResolver::Resolved(res))?
                    },
                    ToResolver::List(path) => {
                        con.queue_send(&FromResolver::List(s.list(&path)))?
                    }
                    ToResolver::Publish(_)
                        | ToResolver::Unpublish(_)
                        | ToResolver::Clear =>
                        con.queue_send(&FromResolver::Error("read only".into()))?,
                }
            }
        }
        Some(write_addr) => {
            let mut s = store.write();
            for m in msgs {
                match m {
                    ToResolver::Heartbeat => (),
                    ToResolver::Resolve(_) | ToResolver::List(_) =>
                        con.queue_send(&FromResolver::Error("write only".into()))?,
                    ToResolver::Publish(paths) => {
                        if !paths.iter().all(Path::is_absolute) {
                            con.queue_send(
                                &FromResolver::Error("absolute paths required".into())
                            )?
                        } else {
                            for path in paths {
                                s.publish(path, write_addr);
                            }
                            con.queue_send(&FromResolver::Published)?
                        }
                    }
                    ToResolver::Unpublish(paths) => {
                        for path in paths {
                            s.unpublish(path, write_addr);
                        }
                        con.queue_send(&FromResolver::Unpublished)?
                    }
                    ToResolver::Clear => {
                        s.unpublish_addr(write_addr);
                        s.gc();
                        con.queue_send(&FromResolver::Unpublished)?
                    }
                }
            }
        }
    }
    Ok(())
}

static HELLO_TIMEOUT: Duration = Duration::from_secs(10);
static READER_TTL: Duration = Duration::from_secs(120);
static MAX_TTL: u64 = 3600;

async fn client_loop(
    store: Store<ClientInfo>,
    s: TcpStream,
    server_stop: impl Future<Output = Result<(), oneshot::Canceled>> + Unpin,
) -> Result<(), Error> {
    s.set_nodelay(true)?;
    let mut con = Channel::new(s);
    let (tx_stop, rx_stop) = oneshot::channel();
    let hello: ClientHello = future::timeout(HELLO_TIMEOUT, con.receive()).await??;
    let (ttl, ttl_expired, write_addr) = match hello {
        ClientHello::ReadOnly => (READER_TTL, false, None),
        ClientHello::WriteOnly {ttl, write_addr} => {
            if ttl <= 0 || ttl > MAX_TTL { bail!("invalid ttl") }
            let mut store = store.write();
            let clinfos = store.clinfo_mut();
            let ttl = Duration::from_secs(ttl);
            match clinfos.get_mut(&write_addr) {
                None => {
                    clinfos.insert(write_addr, Some(tx_stop));
                    (ttl, true, Some(write_addr))
                },
                Some(cl) => {
                    if let Some(old_stop) = mem::replace(cl, Some(tx_stop)) {
                        let _ = old_stop.send(());
                    }
                    (ttl, false, Some(write_addr))
                }
            }
        }
    };
    future::timeout(HELLO_TIMEOUT, con.send_one(&ServerHello { ttl_expired })).await??;
    enum M { Stop, Timeout, Msg(Result<(), io::Error>) };
    let mut con = Some(con);
    let server_stop = server_stop.into_stream().map(|_| M::Stop);
    let rx_stop = rx_stop.into_stream().map(|_| M::Stop);
    let timeout = stream::interval(ttl).map(|_| M::Timeout);
    let mut evts = server_stop.merge(rx_stop).merge(timeout);
    let mut batch = Vec::new();
    let mut act = false;
    async fn receive_batch(
        con: &mut Option<Channel>,
        batch: &mut Vec<ToResolver>
    ) -> Result<(), io::Error> {
        match con {
            Some(ref mut con) => con.receive_batch(batch).await,
            None => future::pending().await
        }
    }
    loop {
        let msg = receive_batch(&mut con, &mut batch).map(|r| Some(M::Msg(r)));
        match evts.next().race(msg).await {
            None | Some(M::Stop) => break Ok(()),
            Some(M::Timeout) => {
                if act {
                    act = false;
                } else {
                    if let Some(write_addr) = write_addr {
                        let mut store = store.write();
                        if let Some(ref mut cl) = store.clinfo_mut().remove(&write_addr) {
                            if let Some(stop) = mem::replace(cl, None) {
                                let _ = stop.send(());
                            }
                        }
                        store.unpublish_addr(write_addr);
                        store.gc();
                    }
                    bail!("client timed out");
                }
            },
            Some(M::Msg(Err(e))) => {
                batch.clear();
                con = None;
                // CR estokes: use proper log module
                println!("error reading message: {}", e)
            },
            Some(M::Msg(Ok(()))) => {
                act = true;
                let c = con.as_mut().unwrap();
                match handle_batch(&store, batch.drain(..), c, write_addr) {
                    Err(_) => { con = None },
                    Ok(()) => match c.flush().await {
                        Err(_) => { con = None }, // CR estokes: Log this
                        Ok(()) => ()
                    }
                }
            },
        }
    }
}

async fn server_loop(
    addr: SocketAddr,
    max_connections: usize,
    stop: oneshot::Receiver<()>,
    ready: oneshot::Sender<SocketAddr>,
) -> Result<SocketAddr, Error> {
    enum M { Stop, Cl(Result<(TcpStream, SocketAddr), io::Error>) };
    let connections = Arc::new(AtomicUsize::new(0));
    let published: Store<ClientInfo> = Store::new();
    let mut listener = TcpListener::bind(addr).await?;
    let local_addr = listener.local_addr()?;
    let mut stop = stop.shared();
    let _ = ready.send(local_addr);
    loop {
        let cl = listener.accept().map(|r| M::Cl(r));
        let st = stop.clone().map(|_| M::Stop);
        match cl.race(st).await {
            M::Stop => return Ok(local_addr),
            M::Cl(Err(_)) => (),
            M::Cl(Ok((client, _))) => {
                if connections.fetch_add(1, Ordering::Relaxed) < max_connections {
                    let connections = connections.clone();
                    let published = published.clone();
                    let stop = stop.clone();
                    task::spawn(async move {
                        let _ = client_loop(published, client, stop).await;
                        connections.fetch_sub(1, Ordering::Relaxed);
                    });
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct Server {
    stop: Option<oneshot::Sender<()>>,
    local_addr: SocketAddr,
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(stop) = mem::replace(&mut self.stop, None) {
            let _ = stop.send(());
        }
    }
}

impl Server {
    pub async fn new(addr: SocketAddr, max_connections: usize) -> Result<Server, Error> {
        let (send_stop, recv_stop) = oneshot::channel();
        let (send_ready, recv_ready) = oneshot::channel();
        let local_addr = 
            task::spawn(server_loop(addr, max_connections, recv_stop, send_ready))
            .race(recv_ready.map(|r| r.map_err(Error::from))).await?;
        Ok(Server {
            stop: Some(send_stop),
            local_addr
        })
    }

    pub fn local_addr(&self) -> &SocketAddr {
        &self.local_addr
    }
}

#[cfg(test)]
mod test {
    use std::net::SocketAddr;
    use crate::{
        path::Path,
        resolver_server::Server,
        resolver::{WriteOnly, ReadOnly, Resolver},
    };

    async fn init_server() -> Server {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        Server::new(addr, 100).await.expect("start server")
    }

    fn p(p: &str) -> Path {
        Path::from(p)
    }

    #[test]
    fn publish_resolve() {
        use async_std::task;
        task::block_on(async {
            let server = init_server().await;
            let paddr: SocketAddr = "127.0.0.1:1".parse().unwrap();
            let mut w = Resolver::<WriteOnly>::new_w(server.local_addr(), paddr).unwrap();
            let mut r = Resolver::<ReadOnly>::new_r(server.local_addr()).unwrap();
            let paths = vec![
                p("/foo/bar"),
                p("/foo/baz"),
                p("/app/v0"),
                p("/app/v1"),
            ];
            w.publish(paths.clone()).await.unwrap();
            for addrs in r.resolve(paths.clone()).await.unwrap() {
                assert_eq!(addrs.len(), 1);
                assert_eq!(addrs[0], paddr);
            }
            assert_eq!(
                r.list(p("/")).await.unwrap(),
                vec![p("/app"), p("/foo")]
            );
            assert_eq!(
                r.list(p("/foo")).await.unwrap(),
                vec![p("/foo/bar"), p("/foo/baz")]
            );
            assert_eq!(
                r.list(p("/app")).await.unwrap(),
                vec![p("/app/v0"), p("/app/v1")]
            );
        });
    }
}
