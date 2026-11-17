//! HTTPS 用系统 OpenSSL。证书和私钥按 nginx 的 PEM 读取：
//! `SSL_CTX_use_certificate_chain_file` + `SSL_CTX_use_PrivateKey_file`，
//! 所以 PRIVATE KEY、RSA PRIVATE KEY、EC PRIVATE KEY 都能用。

use std::ffi::{CStr, CString};
use std::fs;
use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::process::Command;
use std::ptr;
use std::sync::{Arc, Once};
use std::time::Duration;

use anyhow::{bail, Context, Result};

enum SslCtx {}
enum Ssl {}

const SSL_FILETYPE_PEM: i32 = 1;
const SSL_VERIFY_PEER: i32 = 1;
const SSL_CTRL_MODE: i32 = 33;
const SSL_CTRL_SET_TLSEXT_HOSTNAME: i32 = 55;
const SSL_CTRL_SET_MIN_PROTO_VERSION: i32 = 123;
const SSL_MODE_AUTO_RETRY: i64 = 4;
const TLS1_2_VERSION: i64 = 0x0303;
const SSL_ERROR_SSL: i32 = 1;
const SSL_ERROR_SYSCALL: i32 = 5;
const SSL_ERROR_ZERO_RETURN: i32 = 6;

extern "C" {
    fn OPENSSL_init_ssl(opts: u64, settings: *const libc::c_void) -> i32;
    fn TLS_server_method() -> *const libc::c_void;
    fn TLS_client_method() -> *const libc::c_void;
    fn SSL_CTX_new(method: *const libc::c_void) -> *mut SslCtx;
    fn SSL_CTX_free(ctx: *mut SslCtx);
    fn SSL_CTX_ctrl(ctx: *mut SslCtx, cmd: i32, larg: libc::c_long, parg: *mut libc::c_void) -> libc::c_long;
    fn SSL_CTX_use_certificate_chain_file(ctx: *mut SslCtx, file: *const libc::c_char) -> i32;
    fn SSL_CTX_use_PrivateKey_file(ctx: *mut SslCtx, file: *const libc::c_char, typ: i32) -> i32;
    fn SSL_CTX_check_private_key(ctx: *const SslCtx) -> i32;
    fn SSL_CTX_load_verify_locations(
        ctx: *mut SslCtx,
        file: *const libc::c_char,
        path: *const libc::c_char,
    ) -> i32;
    fn SSL_CTX_set_verify(ctx: *mut SslCtx, mode: i32, cb: *const libc::c_void);
    fn SSL_new(ctx: *mut SslCtx) -> *mut Ssl;
    fn SSL_free(ssl: *mut Ssl);
    fn SSL_set_fd(ssl: *mut Ssl, fd: i32) -> i32;
    fn SSL_ctrl(ssl: *mut Ssl, cmd: i32, larg: libc::c_long, parg: *mut libc::c_void) -> libc::c_long;
    fn SSL_set1_host(ssl: *mut Ssl, host: *const libc::c_char) -> i32;
    fn SSL_accept(ssl: *mut Ssl) -> i32;
    fn SSL_connect(ssl: *mut Ssl) -> i32;
    fn SSL_read(ssl: *mut Ssl, buf: *mut libc::c_void, num: i32) -> i32;
    fn SSL_write(ssl: *mut Ssl, buf: *const libc::c_void, num: i32) -> i32;
    fn SSL_shutdown(ssl: *mut Ssl) -> i32;
    fn SSL_get_error(ssl: *const Ssl, ret: i32) -> i32;
    fn ERR_get_error() -> libc::c_ulong;
    fn ERR_error_string_n(err: libc::c_ulong, buf: *mut libc::c_char, len: usize);
}

fn init() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        OPENSSL_init_ssl(0, ptr::null());
    });
}

pub struct ServerCtx {
    ctx: *mut SslCtx,
}

unsafe impl Send for ServerCtx {}
unsafe impl Sync for ServerCtx {}

impl Drop for ServerCtx {
    fn drop(&mut self) {
        if !self.ctx.is_null() {
            unsafe { SSL_CTX_free(self.ctx) }
            self.ctx = ptr::null_mut();
        }
    }
}

pub struct SslStream {
    ssl: *mut Ssl,
    _tcp: std::net::TcpStream,
}

unsafe impl Send for SslStream {}

impl Drop for SslStream {
    fn drop(&mut self) {
        if !self.ssl.is_null() {
            unsafe {
                // 只发一次 close_notify。再调一次会在阻塞套接字上等对端，
                // 对端如果还在读响应就会卡住。
                let _ = SSL_shutdown(self.ssl);
                SSL_free(self.ssl);
            }
            self.ssl = ptr::null_mut();
        }
    }
}

impl Read for SslStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let n = unsafe {
            SSL_read(
                self.ssl,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len().min(i32::MAX as usize) as i32,
            )
        };
        if n > 0 {
            return Ok(n as usize);
        }
        let code = unsafe { SSL_get_error(self.ssl, n) };
        if code == SSL_ERROR_ZERO_RETURN {
            return Ok(0);
        }
        if code == SSL_ERROR_SYSCALL {
            let os = io::Error::last_os_error();
            let queued = drain_errors();
            if n == 0 && os.raw_os_error().unwrap_or(0) == 0 && queued == "unknown openssl error" {
                return Ok(0);
            }
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                format!("SSL_read: {queued} ({os})"),
            ));
        }
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("SSL_read: {}", drain_errors()),
        ))
    }
}

impl Write for SslStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let n = unsafe {
            SSL_write(
                self.ssl,
                buf.as_ptr() as *const libc::c_void,
                buf.len().min(i32::MAX as usize) as i32,
            )
        };
        if n > 0 {
            return Ok(n as usize);
        }
        Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            format!("SSL_write: {}", ssl_error(self.ssl, n)),
        ))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub fn load_server_config(cert_path: &Path, key_path: &Path) -> Result<Arc<ServerCtx>> {
    init();
    let ctx = unsafe { SSL_CTX_new(TLS_server_method()) };
    if ctx.is_null() {
        bail!("SSL_CTX_new: {}", drain_errors());
    }
    let server = ServerCtx { ctx };
    if unsafe {
        SSL_CTX_ctrl(
            ctx,
            SSL_CTRL_SET_MIN_PROTO_VERSION,
            TLS1_2_VERSION as libc::c_long,
            ptr::null_mut(),
        )
    } == 0
    {
        bail!("无法把 TLS 下限设成 1.2: {}", drain_errors());
    }
    let cert = c_path(cert_path)?;
    let key = c_path(key_path)?;
    if unsafe { SSL_CTX_use_certificate_chain_file(ctx, cert.as_ptr()) } != 1 {
        bail!(
            "读取证书 {} 失败: {}",
            cert_path.display(),
            drain_errors()
        );
    }
    if unsafe { SSL_CTX_use_PrivateKey_file(ctx, key.as_ptr(), SSL_FILETYPE_PEM) } != 1 {
        bail!(
            "读取私钥 {} 失败: {}",
            key_path.display(),
            drain_errors()
        );
    }
    if unsafe { SSL_CTX_check_private_key(ctx) } != 1 {
        bail!("证书和私钥不匹配: {}", drain_errors());
    }
    Ok(Arc::new(server))
}

impl ServerCtx {
    pub fn accept(&self, stream: std::net::TcpStream) -> io::Result<SslStream> {
        let _ = stream.set_nodelay(true);
        let ssl = new_ssl(self.ctx)?;
        let mut guard = SslGuard(ssl);
        unsafe {
            if SSL_set_fd(ssl, stream.as_raw_fd()) != 1 {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("SSL_set_fd: {}", drain_errors()),
                ));
            }
            if SSL_accept(ssl) != 1 {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    format!("SSL_accept: {}", ssl_error(ssl, 0)),
                ));
            }
        }
        guard.0 = ptr::null_mut();
        Ok(SslStream { ssl, _tcp: stream })
    }
}

pub fn write_self_signed(cert_path: &Path, key_path: &Path) -> Result<()> {
    let bin = openssl_bin()?;
    let cert = path_str(cert_path)?;
    let key = path_str(key_path)?;
    let output = Command::new(&bin)
        .args([
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-keyout",
            key,
            "-out",
            cert,
            "-days",
            "3650",
            "-nodes",
            "-subj",
            "/CN=punch",
            "-addext",
            "subjectAltName=DNS:localhost,IP:127.0.0.1",
        ])
        .stdin(std::process::Stdio::null())
        .output()
        .with_context(|| format!("运行 {bin} 生成证书"))?;
    if !output.status.success() {
        let _ = fs::remove_file(cert_path);
        let _ = fs::remove_file(key_path);
        let err = String::from_utf8_lossy(&output.stderr);
        bail!("openssl 生成证书失败: {err}");
    }
    Ok(())
}

/// 测试用。信任 `ca_pem`（自签名时就是服务端证书），握手后写请求、读到对端关闭。
pub fn https_exchange(addr: SocketAddr, ca_pem: &Path, request: &[u8]) -> Result<Vec<u8>> {
    init();
    let tcp = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(3))
        .with_context(|| format!("连接 {addr}"))?;
    tcp.set_read_timeout(Some(Duration::from_secs(5)))?;
    tcp.set_write_timeout(Some(Duration::from_secs(5)))?;
    tcp.set_nodelay(true)?;
    let ctx = unsafe { SSL_CTX_new(TLS_client_method()) };
    if ctx.is_null() {
        bail!("SSL_CTX_new(client): {}", drain_errors());
    }
    let ctx = ServerCtx { ctx };
    unsafe {
        SSL_CTX_set_verify(ctx.ctx, SSL_VERIFY_PEER, ptr::null());
    }
    let ca = c_path(ca_pem)?;
    if unsafe { SSL_CTX_load_verify_locations(ctx.ctx, ca.as_ptr(), ptr::null()) } != 1 {
        bail!("加载信任证书 {} 失败: {}", ca_pem.display(), drain_errors());
    }
    let ssl = new_ssl(ctx.ctx).context("SSL_new")?;
    let mut guard = SslGuard(ssl);
    let host = CString::new("localhost").unwrap();
    unsafe {
        if SSL_set_fd(ssl, tcp.as_raw_fd()) != 1 {
            bail!("SSL_set_fd: {}", drain_errors());
        }
        SSL_ctrl(
            ssl,
            SSL_CTRL_SET_TLSEXT_HOSTNAME,
            0,
            host.as_ptr() as *mut libc::c_void,
        );
        if SSL_set1_host(ssl, host.as_ptr()) != 1 {
            bail!("SSL_set1_host: {}", drain_errors());
        }
        if SSL_connect(ssl) != 1 {
            bail!("SSL_connect: {}", ssl_error(ssl, 0));
        }
    }
    guard.0 = ptr::null_mut();
    let mut stream = SslStream { ssl, _tcp: tcp };
    stream.write_all(request).context("写入 https 请求")?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).context("读取 https 响应")?;
    Ok(buf)
}

struct SslGuard(*mut Ssl);

impl Drop for SslGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { SSL_free(self.0) }
            self.0 = ptr::null_mut();
        }
    }
}

fn new_ssl(ctx: *mut SslCtx) -> io::Result<*mut Ssl> {
    let ssl = unsafe { SSL_new(ctx) };
    if ssl.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("SSL_new: {}", drain_errors()),
        ));
    }
    unsafe {
        SSL_ctrl(
            ssl,
            SSL_CTRL_MODE,
            SSL_MODE_AUTO_RETRY as libc::c_long,
            ptr::null_mut(),
        );
    }
    Ok(ssl)
}

fn ssl_error(ssl: *mut Ssl, ret: i32) -> String {
    let code = unsafe { SSL_get_error(ssl, ret) };
    let queued = drain_errors();
    if code == SSL_ERROR_SYSCALL {
        format!("{queued} ({})", io::Error::last_os_error())
    } else if code == SSL_ERROR_SSL {
        queued
    } else {
        format!("openssl error {code}: {queued}")
    }
}

fn drain_errors() -> String {
    let mut out = String::new();
    loop {
        let err = unsafe { ERR_get_error() };
        if err == 0 {
            break;
        }
        let mut buf = [0 as libc::c_char; 256];
        unsafe { ERR_error_string_n(err, buf.as_mut_ptr(), buf.len()) };
        let text = unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy();
        if !out.is_empty() {
            out.push_str("; ");
        }
        out.push_str(&text);
    }
    if out.is_empty() {
        out.push_str("unknown openssl error");
    }
    out
}

fn c_path(path: &Path) -> Result<CString> {
    CString::new(path_str(path)?).context("路径里不能有空字节")
}

fn path_str(path: &Path) -> Result<&str> {
    path.to_str()
        .with_context(|| format!("路径不是 utf-8: {}", path.display()))
}

fn openssl_bin() -> Result<String> {
    if let Ok(bin) = std::env::var("OPENSSL_BIN") {
        if !bin.is_empty() {
            return Ok(bin);
        }
    }
    if Command::new("openssl")
        .arg("version")
        .stdin(std::process::Stdio::null())
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        return Ok("openssl".into());
    }
    for candidate in [
        "/opt/local/bin/openssl",
        "/opt/homebrew/bin/openssl",
        "/usr/bin/openssl",
    ] {
        if Path::new(candidate).exists() {
            return Ok(candidate.into());
        }
    }
    bail!("找不到 openssl 命令，无法生成自签名证书。安装 openssl，或自己准备 web.cert 和 web.key");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_signed_pem_loads() {
        let dir = std::env::temp_dir().join(format!("punch-cert-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let cert = dir.join("cert.pem");
        let key = dir.join("key.pem");
        write_self_signed(&cert, &key).unwrap();
        load_server_config(&cert, &key).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }
}
