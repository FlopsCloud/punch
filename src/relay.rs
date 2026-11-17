//! 双向转发。
//!
//! Linux：两个 TCP 套接字离开 tokio 反应器后，用 splice 经管道对接。
//! 字节留在内核里。就绪事件由 epoll 投递（tokio/mio，边缘触发）。
//! 其他 Unix 用 kqueue 或 poll，加上一块大的用户态缓冲。
//! splice 只对管道和 TCP 套接字有效；内核不接受时退回 copy。

use std::io;
use tokio::net::TcpStream;

pub fn io_mode() -> &'static str {
    #[cfg(target_os = "linux")]
    {
        "epoll+splice"
    }
    #[cfg(not(target_os = "linux"))]
    {
        "kqueue+copy"
    }
}

pub async fn relay(a: TcpStream, b: TcpStream, buf_or_pipe: usize) -> io::Result<(u64, u64)> {
    #[cfg(target_os = "linux")]
    {
        linux::splice_or_copy(a, b, buf_or_pipe).await
    }
    #[cfg(not(target_os = "linux"))]
    {
        copy_pair(a, b, buf_or_pipe).await
    }
}

pub async fn copy_pair(mut a: TcpStream, mut b: TcpStream, buf: usize) -> io::Result<(u64, u64)> {
    let n = buf.clamp(64 * 1024, 1024 * 1024);
    tokio::io::copy_bidirectional_with_sizes(&mut a, &mut b, n, n).await
}

#[cfg(target_os = "linux")]
mod linux {
    use super::copy_pair;
    use std::io::{self, ErrorKind};
    use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use tokio::io::unix::AsyncFd;
    use tokio::io::Interest;
    use tokio::net::TcpStream;

    struct Half {
        pending: usize,
        done: bool,
        total: u64,
    }

    struct PipeEnds {
        read: OwnedFd,
        write: OwnedFd,
        cap: usize,
    }

    enum LoopErr {
        Fallback { a: TcpStream, b: TcpStream },
        Fail(io::Error),
    }

    pub async fn splice_or_copy(
        a: TcpStream,
        b: TcpStream,
        pipe_size: usize,
    ) -> io::Result<(u64, u64)> {
        let want = pipe_size.clamp(64 * 1024, 4 * 1024 * 1024) as i32;
        let pipe_ab = match create_pipe(want) {
            Ok(p) => p,
            Err(_) => return copy_pair(a, b, pipe_size).await,
        };
        let pipe_ba = match create_pipe(want) {
            Ok(p) => p,
            Err(_) => return copy_pair(a, b, pipe_size).await,
        };
        let sock_a = to_async(a)?;
        let sock_b = to_async(b)?;
        match splice_loop(sock_a, sock_b, pipe_ab, pipe_ba).await {
            Ok(n) => Ok(n),
            Err(LoopErr::Fallback { a, b }) => copy_pair(a, b, pipe_size).await,
            Err(LoopErr::Fail(err)) => Err(err),
        }
    }

    fn create_pipe(size: i32) -> io::Result<PipeEnds> {
        let mut fds = [0i32; 2];
        let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        let set = unsafe { libc::fcntl(fds[0], libc::F_SETPIPE_SZ, size) };
        let cap = if set > 0 { set as usize } else { 65536 };
        let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let write = unsafe { OwnedFd::from_raw_fd(fds[1]) };
        Ok(PipeEnds { read, write, cap })
    }

    fn to_async(stream: TcpStream) -> io::Result<AsyncFd<OwnedFd>> {
        let std_stream = stream.into_std()?;
        std_stream.set_nonblocking(true)?;
        AsyncFd::new(OwnedFd::from(std_stream))
    }

    fn from_async(fd: AsyncFd<OwnedFd>) -> io::Result<TcpStream> {
        let std_stream = std::net::TcpStream::from(fd.into_inner());
        std_stream.set_nonblocking(true)?;
        TcpStream::from_std(std_stream)
    }

    fn splice_raw(src: RawFd, dst: RawFd, len: usize) -> io::Result<usize> {
        let flags = libc::SPLICE_F_MOVE | libc::SPLICE_F_NONBLOCK;
        loop {
            let n = unsafe {
                libc::splice(
                    src,
                    std::ptr::null_mut(),
                    dst,
                    std::ptr::null_mut(),
                    len,
                    flags as _,
                )
            };
            if n >= 0 {
                return Ok(n as usize);
            }
            let err = io::Error::last_os_error();
            if err.kind() == ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
    }

    fn shutdown_write(fd: &AsyncFd<OwnedFd>) {
        unsafe {
            libc::shutdown(fd.as_raw_fd(), libc::SHUT_WR);
        }
    }

    fn is_block(err: &io::Error) -> bool {
        err.kind() == ErrorKind::WouldBlock
    }

    /// 还没搬过任何字节时，EINVAL 说明这对 fd 不能 splice，退回用户态 copy。
    fn is_fallback(err: &io::Error, left: &Half, right: &Half) -> bool {
        err.raw_os_error() == Some(libc::EINVAL)
            && left.total == 0
            && right.total == 0
            && left.pending == 0
            && right.pending == 0
    }

    fn step(
        src: &AsyncFd<OwnedFd>,
        dst: &AsyncFd<OwnedFd>,
        pipe: &PipeEnds,
        half: &mut Half,
        wait_read: &mut bool,
        wait_write: &mut bool,
    ) -> Result<(), io::Error> {
        if half.done {
            return Ok(());
        }
        if half.pending == 0 {
            match src.try_io(Interest::READABLE, |fd| {
                splice_raw(fd.as_raw_fd(), pipe.write.as_raw_fd(), pipe.cap)
            }) {
                Ok(0) => {
                    half.done = true;
                    shutdown_write(dst);
                    return Ok(());
                }
                Ok(n) => half.pending = n,
                Err(e) if is_block(&e) => *wait_read = true,
                Err(e) => return Err(e),
            }
        }
        if !half.done && half.pending > 0 {
            let pending = half.pending;
            match dst.try_io(Interest::WRITABLE, |fd| {
                splice_raw(pipe.read.as_raw_fd(), fd.as_raw_fd(), pending)
            }) {
                Ok(0) => {
                    return Err(io::Error::new(
                        ErrorKind::BrokenPipe,
                        "splice write returned 0",
                    ));
                }
                Ok(n) => {
                    half.pending -= n;
                    half.total += n as u64;
                }
                Err(e) if is_block(&e) => *wait_write = true,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    async fn wait_fd(fd: &AsyncFd<OwnedFd>, read: bool, write: bool) -> io::Result<()> {
        let interest = match (read, write) {
            (true, true) => Interest::READABLE | Interest::WRITABLE,
            (true, false) => Interest::READABLE,
            (false, true) => Interest::WRITABLE,
            (false, false) => return Ok(()),
        };
        let mut guard = fd.ready(interest).await?;
        guard.retain_ready();
        Ok(())
    }

    async fn splice_loop(
        sock_a: AsyncFd<OwnedFd>,
        sock_b: AsyncFd<OwnedFd>,
        pipe_ab: PipeEnds,
        pipe_ba: PipeEnds,
    ) -> Result<(u64, u64), LoopErr> {
        let mut ab = Half { pending: 0, done: false, total: 0 };
        let mut ba = Half { pending: 0, done: false, total: 0 };

        loop {
            if ab.done && ba.done {
                return Ok((ab.total, ba.total));
            }
            let mut read_a = false;
            let mut write_a = false;
            let mut read_b = false;
            let mut write_b = false;
            let before = (ab.pending, ab.total, ab.done, ba.pending, ba.total, ba.done);

            if let Err(e) = step(&sock_a, &sock_b, &pipe_ab, &mut ab, &mut read_a, &mut write_b) {
                return map_step_err(e, &ab, &ba, sock_a, sock_b);
            }
            if let Err(e) = step(&sock_b, &sock_a, &pipe_ba, &mut ba, &mut read_b, &mut write_a) {
                return map_step_err(e, &ab, &ba, sock_a, sock_b);
            }
            if ab.done && ba.done {
                return Ok((ab.total, ba.total));
            }
            let after = (ab.pending, ab.total, ab.done, ba.pending, ba.total, ba.done);
            if before != after {
                continue;
            }
            let need_a = read_a || write_a;
            let need_b = read_b || write_b;
            if !need_a && !need_b {
                return Err(LoopErr::Fail(io::Error::new(
                    ErrorKind::Other,
                    "splice stalled",
                )));
            }
            tokio::select! {
                r = wait_fd(&sock_a, read_a, write_a), if need_a => {
                    if let Err(e) = r {
                        return Err(LoopErr::Fail(e));
                    }
                }
                r = wait_fd(&sock_b, read_b, write_b), if need_b => {
                    if let Err(e) = r {
                        return Err(LoopErr::Fail(e));
                    }
                }
            }
        }
    }

    fn map_step_err(
        err: io::Error,
        ab: &Half,
        ba: &Half,
        sock_a: AsyncFd<OwnedFd>,
        sock_b: AsyncFd<OwnedFd>,
    ) -> Result<(u64, u64), LoopErr> {
        if is_fallback(&err, ab, ba) {
            match (from_async(sock_a), from_async(sock_b)) {
                (Ok(a), Ok(b)) => Err(LoopErr::Fallback { a, b }),
                _ => Err(LoopErr::Fail(err)),
            }
        } else {
            Err(LoopErr::Fail(err))
        }
    }
}
