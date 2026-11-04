//! HAP PIN pairing and pair verification. Never accepts an unsigned peer.
use super::{
    hap::{decode_tlv, encode_tlv},
    rtsp::Client,
};
use anyhow::{Context, Result, bail, ensure};
use chacha20poly1305::{
    ChaCha20Poly1305, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use hkdf::Hkdf;
use num_bigint::BigUint;
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha512};
use std::collections::BTreeMap;
use subtle::ConstantTimeEq;
use x25519_dalek::{EphemeralSecret, PublicKey};
use zeroize::{Zeroize, ZeroizeOnDrop};

const N: &str = concat!(
    "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD1",
    "29024E088A67CC74020BBEA63B139B22514A08798E3404DD",
    "EF9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245",
    "E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7ED",
    "EE386BFB5A899FA5AE9F24117C4B1FE649286651ECE45B3D",
    "C2007CB8A163BF0598DA48361C55D39A69163FA8FD24CF5F",
    "83655D23DCA3AD961C62F356208552BB9ED529077096966D",
    "670C354E4ABC9804F1746C08CA18217C32905E462E36CE3B",
    "E39E772C180E86039B2783A2EC07A28FB5C55DF06F4C52C9",
    "DE2BCBF6955817183995497CEA956AE515D2261898FA0510",
    "15728E5A8AAAC42DAD33170D04507A33A85521ABDF1CBA64",
    "ECFB850458DBEF0A8AEA71575D060C7DB3970F85A6E1E4C7",
    "ABF5AE8CDB0933D71E8C94E04A25619DCEE3D2261AD2EE6B",
    "F12FFA06D98A0864D87602733EC86A64521F2B18177B200C",
    "BBE117577A615D6C770988C0BAD946E208E24FA074E5AB31",
    "43DB5BFCE0FD108E4B82D120A93AD2CAFFFFFFFFFFFFFFFF"
);
const ACL: &[u8] = b"\xe1\x57com.apple.ScreenCapture\x01";

/// Contains a private signing key: store in a protected credential store, never log.
#[derive(Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
pub struct Credentials {
    pub identifier: String,
    pub receiver_identifier: String,
    pub receiver_public_key: [u8; 32],
    pub private_key: [u8; 32],
    pub pairing_type: u8,
}

impl Credentials {
    /// Encrypt a serialized credential with a caller-managed, random 256-bit key.
    /// The key must be kept separately (e.g. in the desktop secret service).
    pub fn seal(&self, key: &[u8; 32]) -> Result<Vec<u8>> {
        let mut plaintext = zeroize::Zeroizing::new(serde_json::to_vec(self)?);
        let mut nonce = [0u8; 12];
        OsRng.fill_bytes(&mut nonce);
        let cipher = ChaCha20Poly1305::new(key.into());
        let encrypted = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &plaintext,
                    aad: b"airplay-credentials-v1",
                },
            )
            .map_err(|_| anyhow::anyhow!("credential encryption failed"))?;
        plaintext.zeroize();
        let mut output = b"APC1".to_vec();
        output.extend_from_slice(&nonce);
        output.extend_from_slice(&encrypted);
        Ok(output)
    }
    pub fn open(key: &[u8; 32], data: &[u8]) -> Result<Self> {
        ensure!(
            data.len() >= 32 && &data[..4] == b"APC1",
            "invalid credential envelope"
        );
        let cipher = ChaCha20Poly1305::new(key.into());
        let plaintext = zeroize::Zeroizing::new(
            cipher
                .decrypt(
                    Nonce::from_slice(&data[4..16]),
                    Payload {
                        msg: &data[16..],
                        aad: b"airplay-credentials-v1",
                    },
                )
                .map_err(|_| anyhow::anyhow!("credential authentication failed"))?,
        );
        Ok(serde_json::from_slice(&plaintext)?)
    }

    /// Persist an authenticated envelope atomically. The key is never written here.
    pub fn save(&self, path: &std::path::Path, key: &[u8; 32]) -> Result<()> {
        use std::{fs::OpenOptions, io::Write, os::unix::fs::OpenOptionsExt};
        let parent = path.parent().context("credential path has no parent")?;
        let mut random = [0u8; 16];
        OsRng.fill_bytes(&mut random);
        let suffix: String = random.iter().map(|v| format!("{v:02x}")).collect();
        let temporary = parent.join(format!(".airplay-credential-{suffix}.tmp"));
        let mut created = false;
        let result = (|| -> Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&temporary)?;
            created = true;
            file.write_all(&self.seal(key)?)?;
            file.sync_all()?;
            std::fs::rename(&temporary, path)?;
            Ok(())
        })();
        if result.is_err() && created {
            let _ = std::fs::remove_file(&temporary);
        }
        result
    }

    pub fn load(path: &std::path::Path, key: &[u8; 32]) -> Result<Self> {
        use std::{
            fs::OpenOptions,
            io::Read,
            os::unix::fs::{MetadataExt, OpenOptionsExt},
        };
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        let metadata = file.metadata()?;
        // SAFETY: geteuid has no arguments or side effects.
        ensure!(
            metadata.is_file()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o077 == 0,
            "unsafe credential file permissions or ownership"
        );
        ensure!(metadata.len() <= 65536, "credential file too large");
        let mut bytes = Vec::new();
        file.take(65537).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 65536, "credential file too large");
        Self::open(key, &bytes)
    }
}

pub struct Pairer {
    pub identifier: String,
    pub client_name: String,
    pub pairing_type: u8,
}

fn hash(parts: &[&[u8]]) -> [u8; 64] {
    let mut h = Sha512::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}
pub(super) fn derive(secret: &[u8], salt: &[u8], info: &[u8]) -> [u8; 32] {
    let hkdf = Hkdf::<Sha512>::new(Some(salt), secret);
    let mut out = [0; 32];
    hkdf.expand(info, &mut out).expect("valid HKDF length");
    out
}
fn fixed_nonce(label: &[u8; 8]) -> [u8; 12] {
    let mut n = [0; 12];
    n[4..].copy_from_slice(label);
    n
}
fn encrypt(key: &[u8; 32], label: &[u8; 8], data: &[u8]) -> Result<Vec<u8>> {
    ChaCha20Poly1305::new(key.into())
        .encrypt(Nonce::from_slice(&fixed_nonce(label)), data)
        .map_err(|_| anyhow::anyhow!("pairing encryption failed"))
}
fn decrypt(key: &[u8; 32], label: &[u8; 8], data: &[u8]) -> Result<Vec<u8>> {
    ChaCha20Poly1305::new(key.into())
        .decrypt(
            Nonce::from_slice(&fixed_nonce(label)),
            Payload {
                msg: data,
                aad: &[],
            },
        )
        .map_err(|_| anyhow::anyhow!("pairing authentication failed"))
}
fn field(m: &BTreeMap<u8, Vec<u8>>, tag: u8) -> Result<&[u8]> {
    Ok(m.get(&tag).context(format!("missing pairing TLV {tag}"))?)
}
fn state(m: &BTreeMap<u8, Vec<u8>>, expected: u8) -> Result<()> {
    if let Some(code) = m.get(&7) {
        bail!("receiver pairing error {:?}", code);
    }
    ensure!(field(m, 6)? == [expected], "unexpected pairing state");
    Ok(())
}
fn pad(data: &[u8]) -> Result<Vec<u8>> {
    ensure!(data.len() <= 384, "SRP integer exceeds group");
    let mut out = vec![0; 384 - data.len()];
    out.extend_from_slice(data);
    Ok(out)
}

impl Pairer {
    async fn post(
        &self,
        client: &mut Client,
        path: &str,
        body: &[u8],
        verify: bool,
    ) -> Result<Vec<u8>> {
        let kind = self.pairing_type.to_string();
        let mut headers = vec![
            ("Content-Type", "application/octet-stream"),
            ("X-Apple-HKP", kind.as_str()),
        ];
        if self.pairing_type != 3 {
            headers.push(("X-Apple-Client-Name", &self.client_name));
            headers.push(("X-Apple-Client-ID", &self.identifier));
        }
        if verify && self.pairing_type != 3 {
            headers.push(("X-Apple-PD", "1"));
        }
        if path == "/pair-pin-start" && self.pairing_type != 3 {
            headers.push(("X-Apple-SupportedPINLengths", "4"));
        }
        let response = client.request("POST", path, &headers, body).await?;
        ensure!(
            (200..300).contains(&response.status)
                || (path == "/pair-pin-start" && response.status == 453),
            "{path} returned {}",
            response.status
        );
        Ok(response.body)
    }
    pub async fn start_pin(&self, client: &mut Client) -> Result<()> {
        self.post(client, "/pair-pin-start", &[], false).await?;
        Ok(())
    }
    pub async fn pair_pin(&self, client: &mut Client, pin: &str) -> Result<Credentials> {
        ensure!(
            pin.len() == 4 && pin.bytes().all(|c| c.is_ascii_digit()),
            "PIN must contain four digits"
        );
        ensure!(
            !self.identifier.is_empty() && matches!(self.pairing_type, 3 | 5),
            "invalid pairing identity or type"
        );
        let response = self
            .post(
                client,
                "/pair-setup",
                &encode_tlv(&[(0, &[0]), (6, &[1])]),
                false,
            )
            .await?;
        let m2 = decode_tlv(&response)?;
        state(&m2, 2)?;
        let salt = field(&m2, 2)?;
        let server_public = field(&m2, 3)?;
        ensure!(
            !salt.is_empty() && salt.len() <= 64 && server_public.len() <= 384,
            "invalid SRP parameters"
        );
        let n = BigUint::parse_bytes(N.as_bytes(), 16).unwrap();
        let g = BigUint::from(5u8);
        let b = BigUint::from_bytes_be(server_public);
        ensure!(
            b != BigUint::from(0u8) && b < n,
            "invalid SRP server public key"
        );
        let mut random = [0u8; 32];
        OsRng.fill_bytes(&mut random);
        let a = BigUint::from_bytes_be(&random);
        random.zeroize();
        let a_pub = g.modpow(&a, &n).to_bytes_be();
        let b_pub = b.to_bytes_be();
        let x = BigUint::from_bytes_be(&hash(&[salt, &hash(&[b"Pair-Setup:", pin.as_bytes()])]));
        let k = BigUint::from_bytes_be(&hash(&[&pad(&n.to_bytes_be())?, &pad(&g.to_bytes_be())?]));
        let u = BigUint::from_bytes_be(&hash(&[&pad(&a_pub)?, &pad(&b_pub)?]));
        ensure!(u != BigUint::from(0u8), "invalid SRP scrambling parameter");
        let base = (&n + &b - ((k * g.modpow(&x, &n)) % &n)) % &n;
        let secret = base.modpow(&(a + u * x), &n).to_bytes_be();
        let key = hash(&[&secret]);
        let hn = hash(&[&n.to_bytes_be()]);
        let hg = hash(&[&g.to_bytes_be()]);
        let xor: Vec<u8> = hn.iter().zip(hg).map(|(a, b)| a ^ b).collect();
        let proof = hash(&[&xor, &hash(&[b"Pair-Setup"]), salt, &a_pub, &b_pub, &key]);
        let m3 = encode_tlv(&[(6, &[3]), (3, &pad(&a_pub)?), (4, &proof)]);
        let m4 = decode_tlv(&self.post(client, "/pair-setup", &m3, false).await?)?;
        state(&m4, 4)?;
        let expected = hash(&[&a_pub, &proof, &key]);
        ensure!(
            bool::from(expected.as_slice().ct_eq(field(&m4, 4)?)),
            "SRP server proof mismatch"
        );
        let mut seed = [0; 32];
        OsRng.fill_bytes(&mut seed);
        let signing = SigningKey::from_bytes(&seed);
        let public = signing.verifying_key().to_bytes();
        let sign_key = derive(
            &key,
            b"Pair-Setup-Controller-Sign-Salt",
            b"Pair-Setup-Controller-Sign-Info",
        );
        let signature = signing
            .sign(&[sign_key.as_slice(), self.identifier.as_bytes(), &public].concat())
            .to_bytes();
        let mut items: Vec<(u8, &[u8])> = vec![
            (1, self.identifier.as_bytes()),
            (3, &public),
            (10, &signature),
        ];
        if self.pairing_type == 5 {
            items.push((18, ACL));
        }
        let session_key = derive(&key, b"Pair-Setup-Encrypt-Salt", b"Pair-Setup-Encrypt-Info");
        let sealed = encrypt(&session_key, b"PS-Msg05", &encode_tlv(&items))?;
        let m6 = decode_tlv(
            &self
                .post(
                    client,
                    "/pair-setup",
                    &encode_tlv(&[(5, &sealed), (6, &[5])]),
                    false,
                )
                .await?,
        )?;
        state(&m6, 6)?;
        let inner = decode_tlv(&decrypt(&session_key, b"PS-Msg06", field(&m6, 5)?)?)?;
        let receiver_identifier = std::str::from_utf8(field(&inner, 1)?)?.to_owned();
        ensure!(!receiver_identifier.is_empty(), "empty receiver identity");
        let receiver_public_key: [u8; 32] = field(&inner, 3)?
            .try_into()
            .context("invalid receiver key")?;
        let signature = Signature::from_slice(field(&inner, 10)?)?;
        let accessory_key = derive(
            &key,
            b"Pair-Setup-Accessory-Sign-Salt",
            b"Pair-Setup-Accessory-Sign-Info",
        );
        VerifyingKey::from_bytes(&receiver_public_key)?
            .verify_strict(
                &[
                    accessory_key.as_slice(),
                    receiver_identifier.as_bytes(),
                    &receiver_public_key,
                ]
                .concat(),
                &signature,
            )
            .context("invalid receiver setup signature")?;
        Ok(Credentials {
            identifier: self.identifier.clone(),
            receiver_identifier,
            receiver_public_key,
            private_key: seed,
            pairing_type: self.pairing_type,
        })
    }
    /// Verifies the stored receiver key before enabling encrypted RTSP.
    pub async fn verify(&self, client: &mut Client, credentials: &Credentials) -> Result<[u8; 32]> {
        ensure!(
            self.identifier == credentials.identifier
                && self.pairing_type == credentials.pairing_type,
            "pairing identity mismatch"
        );
        let ephemeral = EphemeralSecret::random_from_rng(OsRng);
        let public = PublicKey::from(&ephemeral);
        let m1 = encode_tlv(&[(6, &[1]), (3, public.as_bytes())]);
        let m2 = decode_tlv(&self.post(client, "/pair-verify", &m1, true).await?)?;
        state(&m2, 2)?;
        let peer_bytes: [u8; 32] = field(&m2, 3)?
            .try_into()
            .context("invalid receiver ephemeral key")?;
        let shared = ephemeral.diffie_hellman(&PublicKey::from(peer_bytes));
        ensure!(shared.was_contributory(), "invalid receiver ephemeral key");
        let secret = shared.as_bytes();
        let key = derive(
            secret,
            b"Pair-Verify-Encrypt-Salt",
            b"Pair-Verify-Encrypt-Info",
        );
        let inner = decode_tlv(&decrypt(&key, b"PV-Msg02", field(&m2, 5)?)?)?;
        let peer_id = field(&inner, 1)?;
        ensure!(
            peer_id == credentials.receiver_identifier.as_bytes(),
            "receiver identity changed"
        );
        let signature = Signature::from_slice(field(&inner, 10)?)?;
        VerifyingKey::from_bytes(&credentials.receiver_public_key)?
            .verify_strict(
                &[peer_bytes.as_slice(), peer_id, public.as_bytes()].concat(),
                &signature,
            )
            .context("invalid receiver verification signature")?;
        let signing = SigningKey::from_bytes(&credentials.private_key);
        let proof = signing
            .sign(
                &[
                    public.as_bytes(),
                    credentials.identifier.as_bytes(),
                    &peer_bytes,
                ]
                .concat(),
            )
            .to_bytes();
        let inner = encode_tlv(&[(1, credentials.identifier.as_bytes()), (10, &proof)]);
        let sealed = encrypt(&key, b"PV-Msg03", &inner)?;
        let m4 = decode_tlv(
            &self
                .post(
                    client,
                    "/pair-verify",
                    &encode_tlv(&[(6, &[3]), (5, &sealed)]),
                    true,
                )
                .await?,
        )?;
        state(&m4, 4)?;
        client.enable_encryption(
            derive(secret, b"Control-Salt", b"Control-Write-Encryption-Key"),
            derive(secret, b"Control-Salt", b"Control-Read-Encryption-Key"),
        )?;
        Ok(*secret)
    }
}

#[cfg(test)]
mod tests {
    use super::super::hap::RecordCipher;
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    async fn request(socket: &mut BufReader<tokio::net::TcpStream>) -> Vec<u8> {
        let mut length = 0;
        loop {
            let mut line = String::new();
            socket.read_line(&mut line).await.unwrap();
            assert!(!line.is_empty());
            if line == "\r\n" {
                break;
            }
            if let Some(value) = line.strip_prefix("Content-Length: ") {
                length = value.trim().parse().unwrap();
            }
        }
        let mut body = vec![0; length];
        socket.read_exact(&mut body).await.unwrap();
        body
    }
    async fn response(socket: &mut BufReader<tokio::net::TcpStream>, seq: u8, body: &[u8]) {
        let header = format!(
            "RTSP/1.0 200 OK\r\nCSeq: {seq}\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        socket.get_mut().write_all(header.as_bytes()).await.unwrap();
        socket.get_mut().write_all(body).await.unwrap();
    }
    #[test]
    fn credentials_authenticated() {
        let creds = Credentials {
            identifier: "sender".into(),
            receiver_identifier: "receiver".into(),
            receiver_public_key: [3; 32],
            private_key: [9; 32],
            pairing_type: 5,
        };
        let mut encrypted = creds.seal(&[8; 32]).unwrap();
        assert_eq!(
            Credentials::open(&[8; 32], &encrypted).unwrap().private_key,
            [9; 32]
        );
        assert!(Credentials::open(&[7; 32], &encrypted).is_err());
        encrypted[20] ^= 1;
        assert!(Credentials::open(&[8; 32], &encrypted).is_err());
    }
    #[test]
    fn credentials_file_permissions_and_symlink() {
        use std::os::unix::fs::{MetadataExt, symlink};
        let creds = Credentials {
            identifier: "a".into(),
            receiver_identifier: "b".into(),
            receiver_public_key: [3; 32],
            private_key: [4; 32],
            pairing_type: 5,
        };
        let mut random = [0u8; 8];
        OsRng.fill_bytes(&mut random);
        let directory = std::env::temp_dir().join(format!(
            "airplay-credentials-{:x}",
            u64::from_le_bytes(random)
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("credentials");
        creds.save(&path, &[9; 32]).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(
            Credentials::load(&path, &[9; 32]).unwrap().private_key,
            [4; 32]
        );
        let link = directory.join("link");
        symlink(&path, &link).unwrap();
        assert!(Credentials::load(&link, &[9; 32]).is_err());
        std::fs::remove_file(link).unwrap();
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
    #[tokio::test]
    async fn verify_then_encrypted_rtsp() {
        let receiver = SigningKey::from_bytes(&[4; 32]);
        let sender = SigningKey::from_bytes(&[5; 32]);
        let creds = Credentials {
            identifier: "sender".into(),
            receiver_identifier: "receiver".into(),
            receiver_public_key: receiver.verifying_key().to_bytes(),
            private_key: [5; 32],
            pairing_type: 5,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let m1 = decode_tlv(&request(&mut stream).await).unwrap();
            let sender_ephemeral: [u8; 32] = field(&m1, 3).unwrap().try_into().unwrap();
            let ephemeral = EphemeralSecret::random_from_rng(OsRng);
            let public = PublicKey::from(&ephemeral);
            let shared = ephemeral.diffie_hellman(&PublicKey::from(sender_ephemeral));
            let key = derive(
                shared.as_bytes(),
                b"Pair-Verify-Encrypt-Salt",
                b"Pair-Verify-Encrypt-Info",
            );
            let signature = receiver
                .sign(&[public.as_bytes().as_slice(), b"receiver", &sender_ephemeral].concat())
                .to_bytes();
            let sealed = encrypt(
                &key,
                b"PV-Msg02",
                &encode_tlv(&[(1, b"receiver"), (10, &signature)]),
            )
            .unwrap();
            response(
                &mut stream,
                1,
                &encode_tlv(&[(6, &[2]), (3, public.as_bytes()), (5, &sealed)]),
            )
            .await;
            let m3 = decode_tlv(&request(&mut stream).await).unwrap();
            let inner =
                decode_tlv(&decrypt(&key, b"PV-Msg03", field(&m3, 5).unwrap()).unwrap()).unwrap();
            let proof = Signature::from_slice(field(&inner, 10).unwrap()).unwrap();
            sender
                .verifying_key()
                .verify_strict(
                    &[sender_ephemeral.as_slice(), b"sender", public.as_bytes()].concat(),
                    &proof,
                )
                .unwrap();
            response(&mut stream, 2, &encode_tlv(&[(6, &[4])])).await;
            let mut head = [0; 2];
            stream.read_exact(&mut head).await.unwrap();
            let len = u16::from_le_bytes(head) as usize;
            let mut record = vec![0; len + 18];
            record[..2].copy_from_slice(&head);
            stream.read_exact(&mut record[2..]).await.unwrap();
            let mut read = RecordCipher::new(derive(
                shared.as_bytes(),
                b"Control-Salt",
                b"Control-Write-Encryption-Key",
            ));
            let plain = read.open(&record).unwrap();
            assert!(plain.starts_with(b"GET /info RTSP/1.0\r\n"));
            let mut write = RecordCipher::new(derive(
                shared.as_bytes(),
                b"Control-Salt",
                b"Control-Read-Encryption-Key",
            ));
            let reply = write
                .seal(b"RTSP/1.0 200 OK\r\nCSeq: 3\r\nContent-Length: 2\r\n\r\nok")
                .unwrap();
            stream.get_mut().write_all(&reply).await.unwrap();
        });
        let mut client = Client::connect(addr, Duration::from_secs(3)).await.unwrap();
        let pairer = Pairer {
            identifier: "sender".into(),
            client_name: "sender".into(),
            pairing_type: 5,
        };
        pairer.verify(&mut client, &creds).await.unwrap();
        let response = client.request("GET", "/info", &[], &[]).await.unwrap();
        assert_eq!(response.body, b"ok");
        task.await.unwrap();
    }
    #[tokio::test]
    async fn verify_rejects_impostor() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let m1 = decode_tlv(&request(&mut stream).await).unwrap();
            let peer: [u8; 32] = field(&m1, 3).unwrap().try_into().unwrap();
            let ephemeral = EphemeralSecret::random_from_rng(OsRng);
            let public = PublicKey::from(&ephemeral);
            let shared = ephemeral.diffie_hellman(&PublicKey::from(peer));
            let key = derive(
                shared.as_bytes(),
                b"Pair-Verify-Encrypt-Salt",
                b"Pair-Verify-Encrypt-Info",
            );
            let impostor = SigningKey::from_bytes(&[2; 32]);
            let signature = impostor
                .sign(&[public.as_bytes().as_slice(), b"receiver", &peer].concat())
                .to_bytes();
            let sealed = encrypt(
                &key,
                b"PV-Msg02",
                &encode_tlv(&[(1, b"receiver"), (10, &signature)]),
            )
            .unwrap();
            response(
                &mut stream,
                1,
                &encode_tlv(&[(6, &[2]), (3, public.as_bytes()), (5, &sealed)]),
            )
            .await;
        });
        let genuine = SigningKey::from_bytes(&[1; 32]);
        let creds = Credentials {
            identifier: "sender".into(),
            receiver_identifier: "receiver".into(),
            receiver_public_key: genuine.verifying_key().to_bytes(),
            private_key: [3; 32],
            pairing_type: 5,
        };
        let mut client = Client::connect(address, Duration::from_secs(2))
            .await
            .unwrap();
        let pairer = Pairer {
            identifier: "sender".into(),
            client_name: "test".into(),
            pairing_type: 5,
        };
        assert!(pairer.verify(&mut client, &creds).await.is_err());
        task.await.unwrap();
    }
    #[tokio::test]
    async fn pin_pairing_complete_srp_and_signed_identity() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let m1 = decode_tlv(&request(&mut stream).await).unwrap();
            state(&m1, 1).unwrap();
            let n = BigUint::parse_bytes(N.as_bytes(), 16).unwrap();
            let g = BigUint::from(5u8);
            let salt = [0x73; 16];
            let server_private = BigUint::from(123456789u64);
            let x = BigUint::from_bytes_be(&hash(&[&salt, &hash(&[b"Pair-Setup:1234"])]));
            let verifier = g.modpow(&x, &n);
            let k = BigUint::from_bytes_be(&hash(&[
                &pad(&n.to_bytes_be()).unwrap(),
                &pad(&g.to_bytes_be()).unwrap(),
            ]));
            let b = ((&k * &verifier) + g.modpow(&server_private, &n)) % &n;
            let bp = b.to_bytes_be();
            response(
                &mut stream,
                1,
                &encode_tlv(&[(6, &[2]), (2, &salt), (3, &pad(&bp).unwrap())]),
            )
            .await;
            let m3 = decode_tlv(&request(&mut stream).await).unwrap();
            state(&m3, 3).unwrap();
            let a = BigUint::from_bytes_be(field(&m3, 3).unwrap());
            let ap = a.to_bytes_be();
            let u = BigUint::from_bytes_be(&hash(&[&pad(&ap).unwrap(), &pad(&bp).unwrap()]));
            let secret = (a * verifier.modpow(&u, &n) % &n)
                .modpow(&server_private, &n)
                .to_bytes_be();
            let key = hash(&[&secret]);
            let hn = hash(&[&n.to_bytes_be()]);
            let hg = hash(&[&g.to_bytes_be()]);
            let xor: Vec<u8> = hn.iter().zip(hg).map(|(a, b)| a ^ b).collect();
            let proof = hash(&[&xor, &hash(&[b"Pair-Setup"]), &salt, &ap, &bp, &key]);
            assert_eq!(field(&m3, 4).unwrap(), proof);
            let server_proof = hash(&[&ap, &proof, &key]);
            response(
                &mut stream,
                2,
                &encode_tlv(&[(6, &[4]), (4, &server_proof)]),
            )
            .await;
            let m5 = decode_tlv(&request(&mut stream).await).unwrap();
            state(&m5, 5).unwrap();
            let session_key = derive(&key, b"Pair-Setup-Encrypt-Salt", b"Pair-Setup-Encrypt-Info");
            let inner =
                decode_tlv(&decrypt(&session_key, b"PS-Msg05", field(&m5, 5).unwrap()).unwrap())
                    .unwrap();
            let controller_id = field(&inner, 1).unwrap();
            let controller_key: [u8; 32] = field(&inner, 3).unwrap().try_into().unwrap();
            let controller_sign = derive(
                &key,
                b"Pair-Setup-Controller-Sign-Salt",
                b"Pair-Setup-Controller-Sign-Info",
            );
            let signature = Signature::from_slice(field(&inner, 10).unwrap()).unwrap();
            VerifyingKey::from_bytes(&controller_key)
                .unwrap()
                .verify_strict(
                    &[controller_sign.as_slice(), controller_id, &controller_key].concat(),
                    &signature,
                )
                .unwrap();
            assert_eq!(field(&inner, 18).unwrap(), ACL);
            let receiver = SigningKey::from_bytes(&[17; 32]);
            let receiver_pub = receiver.verifying_key().to_bytes();
            let sign_key = derive(
                &key,
                b"Pair-Setup-Accessory-Sign-Salt",
                b"Pair-Setup-Accessory-Sign-Info",
            );
            let sig = receiver
                .sign(&[sign_key.as_slice(), b"receiver", &receiver_pub].concat())
                .to_bytes();
            let identity = encode_tlv(&[(1, b"receiver"), (3, &receiver_pub), (10, &sig)]);
            let sealed = encrypt(&session_key, b"PS-Msg06", &identity).unwrap();
            response(&mut stream, 3, &encode_tlv(&[(6, &[6]), (5, &sealed)])).await;
            receiver_pub
        });
        let mut client = Client::connect(addr, Duration::from_secs(5)).await.unwrap();
        let pairer = Pairer {
            identifier: "sender".into(),
            client_name: "test".into(),
            pairing_type: 5,
        };
        let creds = pairer.pair_pin(&mut client, "1234").await.unwrap();
        assert_eq!(creds.receiver_identifier, "receiver");
        assert_eq!(creds.receiver_public_key, task.await.unwrap());
    }
}
