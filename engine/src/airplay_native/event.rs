//! Receiver-to-sender encrypted event channel. Callers must interpret commands.
use super::{hap::RecordCipher, pairing::derive};
use anyhow::{Context, Result, ensure};
use std::net::SocketAddr;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc,
};
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
pub struct Event {
    pub method: String,
    pub path: String,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

/// Serve encrypted events and acknowledge valid commands. The connection ends on
/// authentication or framing errors; a full queue applies backpressure.
pub async fn serve(
    address: SocketAddr,
    secret: &[u8; 32],
    events: mpsc::Sender<Event>,
    cancel: CancellationToken,
) -> Result<()> {
    let mut stream = TcpStream::connect(address).await?;
    let mut read = RecordCipher::new(derive(
        secret,
        b"Events-Salt",
        b"Events-Write-Encryption-Key",
    ));
    let mut write = RecordCipher::new(derive(
        secret,
        b"Events-Salt",
        b"Events-Read-Encryption-Key",
    ));
    let mut buffer = Vec::new();
    loop {
        while let Some((used, cseq, event)) = parse(&buffer)? {
            buffer.drain(..used);
            tokio::select! { _ = cancel.cancelled() => return Ok(()), result = events.send(event) => { result.context("event consumer closed")?; } }
            let response = format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nContent-Length: 0\r\n\r\n");
            let record = write.seal(response.as_bytes())?;
            tokio::select! { _ = cancel.cancelled() => return Ok(()), result = stream.write_all(&record) => result? }
        }
        let mut header = [0; 2];
        tokio::select! { _ = cancel.cancelled() => return Ok(()), result = stream.read_exact(&mut header) => { result?; } }
        let len = u16::from_le_bytes(header) as usize;
        ensure!(len <= 1024, "event record too large");
        let mut record = vec![0; len + 18];
        record[..2].copy_from_slice(&header);
        tokio::select! { _ = cancel.cancelled() => return Ok(()), result = stream.read_exact(&mut record[2..]) => { result?; } }
        buffer.extend_from_slice(&read.open(&record)?);
    }
}

fn parse(data: &[u8]) -> Result<Option<(usize, u64, Event)>> {
    let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") else {
        ensure!(data.len() <= 16 * 1024, "event headers too large");
        return Ok(None);
    };
    ensure!(end + 4 <= 16 * 1024, "event headers too large");
    let text = std::str::from_utf8(&data[..end])?;
    let mut lines = text.split("\r\n");
    let mut first = lines
        .next()
        .context("missing event request")?
        .split_whitespace();
    let method = first.next().context("missing event method")?.to_owned();
    let path = first.next().context("missing event path")?.to_owned();
    ensure!(
        first.next() == Some("RTSP/1.0") && first.next().is_none(),
        "invalid event request"
    );
    let mut cseq = None;
    let mut length = None;
    let mut content_type = None;
    for line in lines {
        let (name, value) = line.split_once(':').context("invalid event header")?;
        let value = value.trim();
        if name.eq_ignore_ascii_case("cseq") {
            ensure!(cseq.is_none(), "duplicate event CSeq");
            cseq = Some(value.parse()?);
        } else if name.eq_ignore_ascii_case("content-length") {
            ensure!(length.is_none(), "duplicate event length");
            length = Some(value.parse::<usize>()?);
        } else if name.eq_ignore_ascii_case("content-type") {
            ensure!(content_type.is_none(), "duplicate event content type");
            content_type = Some(value.to_owned());
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            anyhow::bail!("unsupported event transfer encoding");
        }
    }
    let length = length.unwrap_or(0);
    ensure!(length <= 1024 * 1024, "event body too large");
    let used = end + 4 + length;
    if data.len() < used {
        return Ok(None);
    }
    let event = Event {
        method,
        path,
        content_type,
        body: data[end + 4..used].to_vec(),
    };
    Ok(Some((used, cseq.context("event missing CSeq")?, event)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_pipelined_event() {
        let mut data =
            b"POST /command RTSP/1.0\r\nCSeq: 1\r\nContent-Length: 3\r\n\r\nabc".to_vec();
        data.extend_from_slice(b"GET / RTSP/1.0\r\nCSeq: 2\r\n\r\n");
        let (used, seq, event) = parse(&data).unwrap().unwrap();
        assert_eq!(seq, 1);
        assert_eq!(event.body, b"abc");
        assert_eq!(parse(&data[used..]).unwrap().unwrap().1, 2);
        assert!(parse(b"GET / RTSP/1.0\r\nCSeq: 1\r\nCSeq: 2\r\n\r\n").is_err());
    }
}
