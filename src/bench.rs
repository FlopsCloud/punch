//! 客户端上的明文 HTTP，专门用来把隧道的吞吐打出来。
//!
//! `GET /bench?size=1048576` 按块吐出数据，不把整包堆进一块用户态缓冲。
//! `POST /bench` 把请求体读完后回报字节数，用来测反方向。

use std::io;
use std::net::SocketAddr;
use std::sync::OnceLock;
use std::time::Duration;

use tokio::sync::watch;

use crate::config::TcpTune;
use crate::minihttp::{query_get, HttpConn, Method};
use crate::netutil::{self, tune_tokio};

pub async fn serve(addr: SocketAddr, mut shutdown: watch::Receiver<bool>) -> io::Result<SocketAddr> {
    let tune = TcpTune::default();
    let listener = netutil::bind_tcp(addr, &tune)?;
    let bound = listener.local_addr()?;
    tokio::spawn(async move {
        loop {
            if *shutdown.borrow() {
                break;
            }
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
                acc = listener.accept() => {
                    let Ok((sock, _)) = acc else {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        continue;
                    };
                    tune_tokio(&sock, &tune);
                    tokio::spawn(async move {
                        let _ = bench_conn(HttpConn::new(sock)).await;
                    });
                }
            }
        }
    });
    Ok(bound)
}

async fn bench_conn(mut conn: HttpConn<tokio::net::TcpStream>) -> io::Result<()> {
    loop {
        let Some(req) = conn.next_request(32 * 1024 * 1024).await? else {
            break;
        };
        let keep = req.keep_alive;
        let done = match (req.method, req.path.as_str()) {
            (Method::Get, "/health") => {
                write_fixed(&mut conn, 200, "OK", "text/plain", b"ok\n", keep).await?
            }
            (Method::Get, "/" | "/bench" | "/io") => {
                let size = query_get(&req.query, "size")
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(64 * 1024)
                    .min(32 * 1024 * 1024);
                write_body(&mut conn, size, keep).await?
            }
            (Method::Post, "/bench" | "/io") => {
                let n = req.body.len();
                let body = format!("{{\"bytes\":{n}}}\n");
                write_fixed(
                    &mut conn,
                    200,
                    "OK",
                    "application/json",
                    body.as_bytes(),
                    keep,
                )
                .await?
            }
            _ => {
                write_fixed(
                    &mut conn,
                    404,
                    "Not Found",
                    "text/plain",
                    b"punch bench: GET /bench?size=1048576  POST /bench\n",
                    keep,
                )
                .await?
            }
        };
        if done || !keep {
            break;
        }
    }
    Ok(())
}

async fn write_fixed(
    conn: &mut HttpConn<tokio::net::TcpStream>,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
    keep: bool,
) -> io::Result<bool> {
    let head = header(status, reason, content_type, body.len(), keep);
    conn.write_all(head.as_bytes()).await?;
    conn.write_all(body).await?;
    Ok(false)
}

async fn write_body(
    conn: &mut HttpConn<tokio::net::TcpStream>,
    size: usize,
    keep: bool,
) -> io::Result<bool> {
    let head = header(
        200,
        "OK",
        "application/octet-stream",
        size,
        keep,
    );
    conn.write_all(head.as_bytes()).await?;
    let chunk = payload();
    let mut left = size;
    while left > 0 {
        let n = left.min(chunk.len());
        conn.write_all(&chunk[..n]).await?;
        left -= n;
    }
    Ok(false)
}

fn header(status: u16, reason: &str, content_type: &str, len: usize, keep: bool) -> String {
    let conn = if keep { "keep-alive" } else { "close" };
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {len}\r\nConnection: {conn}\r\nCache-Control: no-store\r\nX-Punch-Bench: 1\r\n\r\n"
    )
}

fn payload() -> &'static [u8] {
    static CHUNK: OnceLock<Vec<u8>> = OnceLock::new();
    CHUNK.get_or_init(|| vec![0xA5; 256 * 1024])
}
