use super::hap::RecordCipher;
use anyhow::{Context, Result, bail, ensure};
use std::{collections::BTreeMap, net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
};

const MAX_HEADER: usize = 64 * 1024;
const MAX_BODY: usize = 8 * 1024 * 1024;

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

/// One serialized request at a time on a persistent RTSP connection.
pub struct Client {
    stream: BufReader<TcpStream>,
    sequence: u32,
    timeout: Duration,
    poisoned: bool,
    crypto: Option<(RecordCipher, RecordCipher)>,
    plaintext: Vec<u8>,
}

impl Client {
    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.stream.get_ref().local_addr()?)
    }
    pub fn is_encrypted(&self) -> bool {
        self.crypto.is_some() && !self.poisoned
    }
    pub async fn connect(address: SocketAddr, timeout: Duration) -> Result<Self> {
        let stream = tokio::time::timeout(timeout, TcpStream::connect(address)).await??;
        stream.set_nodelay(true)?;
        Ok(Self {
            stream: BufReader::new(stream),
            sequence: 0,
            timeout,
            poisoned: false,
            crypto: None,
            plaintext: Vec::new(),
        })
    }

    /// Transition to HAP framing only after a verified pair-verify response.
    pub fn enable_encryption(&mut self, write_key: [u8; 32], read_key: [u8; 32]) -> Result<()> {
        ensure!(
            !self.poisoned && self.crypto.is_none(),
            "control channel cannot be upgraded"
        );
        self.crypto = Some((RecordCipher::new(write_key), RecordCipher::new(read_key)));
        Ok(())
    }

    pub async fn request(
        &mut self,
        method: &str,
        uri: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Result<Response> {
        ensure!(
            !self.poisoned,
            "RTSP connection cannot be reused after a failed request"
        );
        ensure!(
            method.bytes().all(|c| c.is_ascii_uppercase() || c == b'_'),
            "invalid RTSP method"
        );
        ensure!(!uri.contains(['\r', '\n', ' ']), "invalid RTSP URI");
        ensure!(body.len() <= MAX_BODY, "request body too large");
        self.sequence = self
            .sequence
            .checked_add(1)
            .context("RTSP sequence exhausted")?;
        let mut request = format!(
            "{method} {uri} RTSP/1.0\r\nCSeq: {}\r\nContent-Length: {}\r\n",
            self.sequence,
            body.len()
        );
        if !headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("user-agent"))
        {
            request.push_str("User-Agent: AirPlay/935.7.1\r\n");
        }
        for (name, value) in headers {
            ensure!(
                !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
                "invalid RTSP header name"
            );
            ensure!(!value.contains(['\r', '\n']), "invalid RTSP header value");
            ensure!(
                !name.eq_ignore_ascii_case("content-length") && !name.eq_ignore_ascii_case("cseq"),
                "reserved RTSP header"
            );
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str("\r\n");
        let sequence = self.sequence;
        // Set before awaiting: cancellation also makes a partially written request unusable.
        self.poisoned = true;
        let response = tokio::time::timeout(self.timeout, async {
            if let Some((write, read)) = self.crypto.as_mut() {
                let mut message = request.into_bytes();
                message.extend_from_slice(body);
                for chunk in message.chunks(1024) {
                    self.stream.get_mut().write_all(&write.seal(chunk)?).await?;
                }
                self.stream.get_mut().flush().await?;
                read_encrypted_response(&mut self.stream, read, &mut self.plaintext, sequence).await
            } else {
                self.stream.get_mut().write_all(request.as_bytes()).await?;
                self.stream.get_mut().write_all(body).await?;
                self.stream.get_mut().flush().await?;
                read_response(&mut self.stream, sequence).await
            }
        })
        .await
        .context("RTSP request timed out; connection must not be reused")??;
        self.poisoned = false;
        Ok(response)
    }
}

async fn read_encrypted_response(
    stream: &mut BufReader<TcpStream>,
    cipher: &mut RecordCipher,
    plaintext: &mut Vec<u8>,
    sequence: u32,
) -> Result<Response> {
    loop {
        if let Some(header_end) = plaintext.windows(4).position(|w| w == b"\r\n\r\n") {
            ensure!(header_end + 4 <= MAX_HEADER, "RTSP header too large");
            let (_, _, length) = parse_header(&plaintext[..header_end + 4], sequence)?;
            let end = header_end + 4 + length;
            if plaintext.len() >= end {
                let remainder = plaintext.split_off(end);
                let message = std::mem::replace(plaintext, remainder);
                let mut reader = &message[..];
                return read_response(&mut reader, sequence).await;
            }
        } else {
            ensure!(plaintext.len() <= MAX_HEADER, "RTSP header too large");
        }
        let mut prefix = [0; 2];
        stream.read_exact(&mut prefix).await?;
        let len = u16::from_le_bytes(prefix) as usize;
        ensure!(len <= 1024, "HAP record too large");
        let mut record = vec![0; len + 18];
        record[..2].copy_from_slice(&prefix);
        stream.read_exact(&mut record[2..]).await?;
        plaintext.extend_from_slice(&cipher.open(&record)?);
    }
}

async fn read_response<R: AsyncBufRead + Unpin>(stream: &mut R, sequence: u32) -> Result<Response> {
    let mut header = Vec::new();
    loop {
        // fill_buf avoids unbounded allocation on an unterminated line.
        let data = stream.fill_buf().await?;
        if data.is_empty() {
            bail!("EOF in RTSP header");
        }
        let count = data
            .iter()
            .position(|&b| b == b'\n')
            .map_or(data.len(), |n| n + 1);
        ensure!(header.len() + count <= MAX_HEADER, "RTSP header too large");
        header.extend_from_slice(&data[..count]);
        stream.consume(count);
        if header.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let (code, headers, length) = parse_header(&header, sequence)?;
    let mut body = vec![0; length];
    stream.read_exact(&mut body).await?;
    Ok(Response {
        status: code,
        headers,
        body,
    })
}

fn parse_header(header: &[u8], sequence: u32) -> Result<(u16, BTreeMap<String, String>, usize)> {
    let text = std::str::from_utf8(header).context("invalid RTSP header encoding")?;
    let mut lines = text.split("\r\n");
    let mut status = lines.next().context("missing status")?.split_whitespace();
    ensure!(
        matches!(status.next(), Some("RTSP/1.0") | Some("HTTP/1.1")),
        "invalid protocol"
    );
    let code: u16 = status.next().context("missing status code")?.parse()?;
    let mut headers = BTreeMap::new();
    for line in lines.filter(|l| !l.is_empty()) {
        let (key, value) = line.split_once(':').context("invalid response header")?;
        let key = key.trim().to_ascii_lowercase();
        ensure!(!headers.contains_key(&key), "duplicate RTSP header {key}");
        headers.insert(key, value.trim().to_owned());
    }
    if let Some(cseq) = headers.get("cseq") {
        ensure!(cseq.parse::<u32>()? == sequence, "RTSP CSeq mismatch");
    }
    ensure!(
        !headers.contains_key("transfer-encoding"),
        "unsupported transfer encoding"
    );
    let length: usize = headers
        .get("content-length")
        .map(|s| s.parse())
        .transpose()?
        .unwrap_or(0);
    ensure!(length <= MAX_BODY, "RTSP body too large");
    Ok((code, headers, length))
}

/// RFC 2069 digest, as used by AirPlay password-protected receivers.
/// Reject qop and non-MD5 challenges instead of silently generating an invalid response.
/// Kept for password-protected receivers; exercised by the unit tests.
#[allow(dead_code)]
pub fn digest_authorization(
    challenge: &str,
    password: &str,
    method: &str,
    uri: &str,
) -> Result<String> {
    let (scheme, rest) = challenge
        .trim()
        .split_once(' ')
        .context("invalid digest challenge")?;
    ensure!(
        scheme.eq_ignore_ascii_case("digest"),
        "unsupported auth scheme"
    );
    let mut params = BTreeMap::new();
    let mut part = String::new();
    let mut quoted = false;
    for c in rest.chars().chain(std::iter::once(',')) {
        if c == '"' {
            quoted = !quoted;
        }
        if c == ',' && !quoted {
            let (k, v) = part.split_once('=').context("invalid digest parameter")?;
            params.insert(
                k.trim().to_ascii_lowercase(),
                v.trim().trim_matches('"').to_owned(),
            );
            part.clear();
        } else {
            part.push(c);
        }
    }
    ensure!(!quoted, "unterminated digest parameter");
    ensure!(!params.contains_key("qop"), "digest qop not supported");
    ensure!(
        params
            .get("algorithm")
            .is_none_or(|a| a.eq_ignore_ascii_case("MD5")),
        "unsupported digest algorithm"
    );
    let realm = params.get("realm").context("missing realm")?;
    let nonce = params.get("nonce").context("missing nonce")?;
    ensure!(
        ![realm.as_str(), nonce.as_str(), uri]
            .iter()
            .any(|s| s.contains(['"', '\r', '\n'])),
        "unsafe digest parameter"
    );
    let hash = |s: &str| format!("{:x}", md5::compute(s));
    let ha1 = hash(&format!("AirPlay:{realm}:{password}"));
    let ha2 = hash(&format!("{method}:{uri}"));
    let response = hash(&format!("{ha1}:{nonce}:{ha2}"));
    Ok(format!(
        "Digest username=\"AirPlay\", realm=\"{realm}\", nonce=\"{nonce}\", uri=\"{uri}\", response=\"{response}\""
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn digest_rejects_qop() {
        assert!(
            digest_authorization(
                "Digest realm=\"x\", nonce=\"y\", qop=\"auth\"",
                "p",
                "GET",
                "/"
            )
            .is_err()
        );
    }
    #[test]
    fn digest_quoted_comma() {
        assert!(
            digest_authorization("Digest realm=\"x,y\", nonce=\"z\"", "p", "GET", "/")
                .unwrap()
                .contains("realm=\"x,y\"")
        );
    }
    #[tokio::test]
    async fn loopback_response() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = BufReader::new(socket);
            let mut line = String::new();
            loop {
                line.clear();
                socket.read_line(&mut line).await.unwrap();
                if line == "\r\n" {
                    break;
                }
            }
            socket
                .get_mut()
                .write_all(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\nContent-Length: 3\r\n\r\nabc")
                .await
                .unwrap();
        });
        let mut client = Client::connect(addr, Duration::from_secs(1)).await.unwrap();
        let reply = client.request("GET", "/info", &[], &[]).await.unwrap();
        assert_eq!(reply.status, 200);
        assert_eq!(reply.body, b"abc");
        server.await.unwrap();
    }
}
