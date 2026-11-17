use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::proto::{
    decode_assign_prefix, decode_data_hello, DataHello, CtrlMsg, ASSIGN_PREFIX, DATA_HELLO_LEN,
};

pub async fn read_msg<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<CtrlMsg> {
    let mut lenb = [0u8; 4];
    r.read_exact(&mut lenb).await?;
    let len = u32::from_le_bytes(lenb) as usize;
    if len == 0 || len > 1024 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad frame length",
        ));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    CtrlMsg::decode(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub async fn write_msg<W: AsyncWrite + Unpin>(w: &mut W, msg: &CtrlMsg) -> io::Result<()> {
    w.write_all(&msg.encode()).await
}

pub async fn read_data_hello<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<DataHello> {
    let mut buf = [0u8; DATA_HELLO_LEN];
    r.read_exact(&mut buf).await?;
    decode_data_hello(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub async fn read_assign<R: AsyncRead + Unpin>(
    r: &mut R,
) -> io::Result<(u32, u32, u16, String)> {
    let mut prefix = [0u8; ASSIGN_PREFIX];
    r.read_exact(&mut prefix).await?;
    let (stream_id, map_id, port, host_len) =
        decode_assign_prefix(&prefix).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let mut host = vec![0u8; host_len as usize];
    if !host.is_empty() {
        r.read_exact(&mut host).await?;
    }
    let host = String::from_utf8(host)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "assign host utf8"))?;
    Ok((stream_id, map_id, port, host))
}
