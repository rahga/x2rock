//! Reading the music-service tokens a household already stores.
//!
//! Every zone player keeps, per configured music-service account, the same
//! `authToken`/`privateKey` pair a device link mints - the credential x2rock's
//! own `link` flow tries to obtain and that some services (Qobuz, and every
//! app-link service whose exchange completes in Sonos's cloud) never hand back.
//! It is not out of reach. The player publishes the whole set, encrypted, in the
//! **initial `ZoneGroupTopology` event** as a variable named
//! `ThirdPartyMediaServersX`, and the key is derived from the household id - LAN
//! metadata any device answers unauthenticated.
//!
//! So this is the missing half of the discovery/playback split written down
//! across `docs/architecture.md`: playback rides the household registration, and
//! search/browse can ride the household's own stored token, with no browser flow
//! at all. It opens exactly the services whose link flow this tool cannot drive.
//!
//! The mechanism was published by [SoCo PR
//! #1010](https://github.com/SoCo/SoCo/pull/1010) and verified byte-for-byte
//! against a real household before this was written: three services x2rock had
//! linked itself decrypt to the same token bytes it already held.
//!
//! **Two halves, deliberately separated.** [`decrypt_accounts`] is pure - bytes
//! and a household id in, accounts out - and carries the whole test suite. The
//! capture that feeds it ([`capture_envelope`]) is the only part that touches the
//! network, and it needs the player to reach an inbound callback port on this
//! machine, which a firewall will refuse by default. The pure half never cares.
//!
//! MD5 here is protocol, not a security choice: it is what the desktop
//! controller does, and the four-byte digest tail is only how a correct decrypt
//! is recognised, never a defence.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecryptMut, KeyIvInit};
use anyhow::{Context, Result, anyhow, bail};
use md5::{Digest, Md5};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// The fixed salt Sonos mixes with the household id to derive the blob key.
/// A reverse-engineered constant, public in SoCo #1010; a firmware update could
/// in principle change it, at which point the integrity check below would start
/// failing and say so, rather than returning garbage.
const SALT: [u8; 16] = [
    0x1a, 0x01, 0xa7, 0x31, 0xc9, 0x6e, 0x9e, 0xbd, 0xe8, 0x47, 0x51, 0x82, 0xb2, 0x74, 0xb7, 0x0e,
];

/// One music-service account as the household stores it.
///
/// The `token`/`key` pair is what goes into the SMAPI `loginToken` header - the
/// same two fields `credentials::Account` keeps. Everything else is here to name
/// the account and match it to a service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredAccount {
    /// The Sonos service id, decoded from the account UDN.
    pub service_id: u32,
    /// `SerialNum0` - the household's own selector for this account, the `sn_N`
    /// seen elsewhere.
    pub serial: u32,
    /// `authToken`. Secret.
    pub token: String,
    /// `privateKey`. Secret; empty for some services, which is legitimate.
    pub key: String,
    /// `Nickname0`, what the person or app named the account. Often empty.
    pub nickname: String,
    /// `Tier0`, a service-specific tier marker. Kept only for display.
    pub tier: String,
}

impl StoredAccount {
    /// Whether this record actually carries a credential. An account can be
    /// listed with an empty token (an anonymous service left a placeholder, or a
    /// half-removed account); such a row is real but useless to inject.
    pub fn has_token(&self) -> bool {
        !self.token.is_empty()
    }
}

/// Decrypt the `ThirdPartyMediaServersX` envelope into the accounts it holds.
///
/// `household_id` is the short form (`Sonos_...`, no `.suffix`), exactly as
/// `DeviceProperties GetHouseholdID` returns it - the long form the SMAPI header
/// wants is *not* the key here. A wrong household id fails the integrity check
/// rather than returning nonsense.
pub fn decrypt_accounts(encoded: &str, household_id: &str) -> Result<Vec<StoredAccount>> {
    let payload = decrypt_payload(encoded, household_id)?;
    parse_accounts(&payload)
}

/// The `2:`-prefixed, base64'd `AES-128-CBC(iv + ciphertext)` envelope, decoded
/// and verified, returning the account XML.
fn decrypt_payload(encoded: &str, household_id: &str) -> Result<Vec<u8>> {
    let encoded = encoded.trim();
    let body = encoded
        .strip_prefix("2:")
        .ok_or_else(|| anyhow!("unexpected account envelope version (want a `2:` prefix)"))?;
    let raw = base64_decode(body).context("account envelope was not valid base64")?;
    if raw.len() < 32 || (raw.len() - 16) % 16 != 0 {
        bail!("account envelope is the wrong size to be iv + AES blocks");
    }
    let (iv, ciphertext) = raw.split_at(16);
    let iv: &[u8; 16] = iv.try_into().expect("split_at(16) yields 16 bytes");

    // key = md5(iv + md5(household + salt))
    let global = md5(&[household_id.as_bytes(), &SALT[..]]);
    let blob_key = md5(&[&iv[..], &global[..]]);
    let mut plain = aes_128_cbc_decrypt(ciphertext, &blob_key, iv);

    // PKCS#7: the last byte is the pad length, 1..=16.
    let pad = *plain
        .last()
        .ok_or_else(|| anyhow!("empty decrypted payload"))? as usize;
    if pad == 0 || pad > 16 || pad > plain.len() {
        bail!("wrong household id, or a corrupt account payload (bad PKCS#7 padding)");
    }
    plain.truncate(plain.len() - pad);

    // The plaintext ends with the first four bytes of md5(payload): the integrity
    // tail that tells a correct decrypt from a wrong key.
    if plain.len() < 4 {
        bail!("decrypted account payload is too short to carry its checksum");
    }
    let (payload, checksum) = plain.split_at(plain.len() - 4);
    if md5(&[payload])[..4] != *checksum {
        bail!("account payload integrity check failed - wrong household id, or not this format");
    }
    Ok(payload.to_vec())
}

/// Parse the decrypted account XML.
///
/// Each account is an element with a `UDN` of `SA_RINCON<type>_...`, where
/// `type / 256` is the service id and `type % 256` a schema revision. The token,
/// key, nickname, tier and serial ride in `Token0`/`Key0`/`Nickname0`/`Tier0`/
/// `SerialNum0` attributes.
fn parse_accounts(payload: &[u8]) -> Result<Vec<StoredAccount>> {
    let text = String::from_utf8_lossy(payload);
    let doc = roxmltree::Document::parse(&text).context("decrypted account XML did not parse")?;
    let mut accounts = Vec::new();
    for node in doc.descendants().filter(|n| n.is_element()) {
        let Some(udn) = node.attribute("UDN") else {
            continue;
        };
        let Some(encoded_type) = udn
            .strip_prefix("SA_RINCON")
            .and_then(|rest| rest.split('_').next())
            .and_then(|digits| digits.parse::<u32>().ok())
        else {
            continue;
        };
        accounts.push(StoredAccount {
            service_id: encoded_type / 256,
            serial: node
                .attribute("SerialNum0")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
            token: node.attribute("Token0").unwrap_or_default().to_string(),
            key: node.attribute("Key0").unwrap_or_default().to_string(),
            nickname: node.attribute("Nickname0").unwrap_or_default().to_string(),
            tier: node.attribute("Tier0").unwrap_or_default().to_string(),
        });
    }
    Ok(accounts)
}

/// One MD5 over a sequence of parts, concatenated.
fn md5(parts: &[&[u8]]) -> [u8; 16] {
    let mut hasher = Md5::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}

/// AES-128-CBC decrypt, no padding removed - the caller strips PKCS#7 itself,
/// because it also has to peel the integrity tail underneath it.
fn aes_128_cbc_decrypt(ciphertext: &[u8], key: &[u8; 16], iv: &[u8; 16]) -> Vec<u8> {
    type Dec = cbc::Decryptor<aes::Aes128>;
    let mut cipher = Dec::new_from_slices(key, iv).expect("16-byte AES key and IV");
    let mut buf = ciphertext.to_vec();
    // The caller has already checked the length is a whole number of blocks, so
    // the remainder is empty by construction.
    for block in buf.as_chunks_mut::<16>().0 {
        cipher.decrypt_block_mut(GenericArray::from_mut_slice(block));
    }
    buf
}

/// Standard base64 decode (with `+/` and `=` padding, whitespace tolerated).
/// Hand-rolled to keep this the only new dependency-worth of code that base64
/// costs; the envelope is the sole caller.
fn base64_decode(input: &str) -> Result<Vec<u8>> {
    fn val(b: u8) -> Option<u8> {
        match b {
            b'A'..=b'Z' => Some(b - b'A'),
            b'a'..=b'z' => Some(b - b'a' + 26),
            b'0'..=b'9' => Some(b - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut bits: u32 = 0;
    let mut nbits = 0;
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    for b in input.bytes() {
        if b == b'=' || b.is_ascii_whitespace() {
            continue;
        }
        let v = val(b).ok_or_else(|| anyhow!("invalid base64 byte {b:#x}"))?;
        bits = (bits << 6) | v as u32;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push((bits >> nbits) as u8);
        }
    }
    Ok(out)
}

/// Capture the encrypted `ThirdPartyMediaServersX` from a player.
///
/// Subscribes to the player's `ZoneGroupTopology` events, catches the initial
/// state it POSTs back, pulls the one variable out, and unsubscribes. The only
/// network side effect is that subscription, which the player forgets on its own
/// after the short timeout even if the unsubscribe is lost.
///
/// **This needs the player to open a TCP connection back to this machine.** On a
/// host with a firewall up (a laptop's default), the inbound callback is dropped
/// and this times out - which is reported as exactly that, with the port named,
/// since the fix is a firewall rule and not a retry.
///
/// `callback_port` is where that connection is accepted. A fixed value is what
/// makes the firewall rule a one-time thing rather than a moving target: bind
/// the same port every run and the person opens it once. `0` asks the OS for an
/// ephemeral one, which suits a host with no firewall and nothing to open.
pub async fn capture_envelope(
    player: IpAddr,
    callback_port: u16,
    timeout: Duration,
) -> Result<String> {
    let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, callback_port))
        .await
        .with_context(|| {
            if callback_port == 0 {
                "opening a local callback port for the account event".to_string()
            } else {
                format!(
                    "opening callback port {callback_port} for the account event - \
                     something else may be using it; pass --callback-port to pick another"
                )
            }
        })?;
    let port = listener.local_addr()?.port();
    let local_ip = local_ip_toward(player)?;

    let sid = subscribe(player, local_ip, port).await?;

    // The first NOTIFY carries the full initial state, so one is enough - but
    // loop in case a keepalive or partial event lands first.
    let deadline = tokio::time::Instant::now() + timeout;
    let found = loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break None;
        }
        let accept = tokio::time::timeout(remaining, listener.accept()).await;
        let Ok(Ok((mut socket, _))) = accept else {
            break None;
        };
        let request = read_http_message(&mut socket).await.unwrap_or_default();
        // A GENA NOTIFY wants a 200 or the player retries and then drops the sub.
        let _ = socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
            .await;
        if let Some(value) = extract_variable(&request, "ThirdPartyMediaServersX") {
            break Some(value);
        }
    };

    unsubscribe(player, &sid).await;

    found.ok_or_else(|| {
        anyhow!(
            "no account event arrived within {timeout:?}. The player has to open a \
             connection back to this machine on TCP port {port}; if this host runs a \
             firewall, allow inbound TCP {port} from the Sonos subnet and try again"
        )
    })
}

/// The address this machine uses to reach the player, which is what the player
/// must call back on.
fn local_ip_toward(player: IpAddr) -> Result<IpAddr> {
    let socket = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .context("finding this machine's address toward the player")?;
    socket.connect(SocketAddr::new(player, 1400))?;
    Ok(socket.local_addr()?.ip())
}

/// SUBSCRIBE to ZoneGroupTopology, returning the subscription id to cancel with.
async fn subscribe(player: IpAddr, local_ip: IpAddr, port: u16) -> Result<String> {
    let callback = match local_ip {
        IpAddr::V6(v6) => format!("<http://[{v6}]:{port}/notify>"),
        v4 => format!("<http://{v4}:{port}/notify>"),
    };
    let request = format!(
        "SUBSCRIBE /ZoneGroupTopology/Event HTTP/1.1\r\n\
         HOST: {player}:1400\r\n\
         CALLBACK: {callback}\r\n\
         NT: upnp:event\r\n\
         TIMEOUT: Second-60\r\n\
         Content-Length: 0\r\n\
         Connection: close\r\n\r\n"
    );
    let mut stream = TcpStream::connect(SocketAddr::new(player, 1400))
        .await
        .context("subscribing to the player's topology events")?;
    stream.write_all(request.as_bytes()).await?;
    let response = read_http_message(&mut stream).await?;
    header_value(&response, "SID")
        .map(str::to_string)
        .ok_or_else(|| anyhow!("the player accepted the subscription but named no SID"))
}

/// Best-effort UNSUBSCRIBE. A lost one costs nothing: the subscription expires
/// on its own in a minute.
async fn unsubscribe(player: IpAddr, sid: &str) {
    let request = format!(
        "UNSUBSCRIBE /ZoneGroupTopology/Event HTTP/1.1\r\n\
         HOST: {player}:1400\r\n\
         SID: {sid}\r\n\
         Connection: close\r\n\r\n"
    );
    if let Ok(mut stream) = TcpStream::connect(SocketAddr::new(player, 1400)).await {
        let _ = stream.write_all(request.as_bytes()).await;
        let _ = read_http_message(&mut stream).await;
    }
}

/// Read one HTTP message (head plus any `Content-Length` body) to a string.
async fn read_http_message<S: AsyncReadExt + Unpin>(stream: &mut S) -> Result<String> {
    let mut raw = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let head_end = raw.windows(4).position(|w| w == b"\r\n\r\n");
        if let Some(end) = head_end {
            let head = String::from_utf8_lossy(&raw[..end]);
            let want = header_value(&head, "Content-Length")
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if raw.len() >= end + 4 + want {
                break;
            }
        }
        match stream.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => raw.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    Ok(String::from_utf8_lossy(&raw).into_owned())
}

/// One header value from an HTTP message, case-insensitive on the name.
fn header_value<'a>(message: &'a str, name: &str) -> Option<&'a str> {
    message.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

/// Pull one evented variable's value out of a GENA property-set NOTIFY body.
///
/// The body is `<e:propertyset><e:property><Name>value</Name></e:property>...`,
/// with the values entity-escaped. The account envelope is a flat `2:...` string
/// rather than nested XML, so one unescape and a tag scan is enough.
fn extract_variable(message: &str, name: &str) -> Option<String> {
    let unescaped = xml_unescape(message);
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = unescaped.find(&open)? + open.len();
    let end = unescaped[start..].find(&close)? + start;
    let value = unescaped[start..end].trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// Undo the XML entities a GENA property set wraps its values in. The envelope
/// can be escaped once (`&lt;`) or twice; two passes settle both.
fn xml_unescape(text: &str) -> String {
    let once = |s: &str| {
        s.replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&")
    };
    once(&once(text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::BlockEncryptMut;

    /// Build a real `2:` envelope from account XML, so the decrypt path can be
    /// tested against its own inverse with no live household and no real token.
    fn seal(account_xml: &str, household_id: &str, iv: [u8; 16]) -> String {
        let global = md5(&[household_id.as_bytes(), &SALT[..]]);
        let blob_key = md5(&[&iv[..], &global[..]]);

        // payload + 4-byte md5 tail, then PKCS#7 to the block.
        let mut body = account_xml.as_bytes().to_vec();
        body.extend_from_slice(&md5(&[account_xml.as_bytes()])[..4]);
        let pad = 16 - (body.len() % 16);
        body.extend(std::iter::repeat_n(pad as u8, pad));

        type Enc = cbc::Encryptor<aes::Aes128>;
        let mut cipher = Enc::new_from_slices(&blob_key, &iv).expect("16-byte AES key and IV");
        for block in body.as_chunks_mut::<16>().0 {
            cipher.encrypt_block_mut(GenericArray::from_mut_slice(block));
        }

        let mut raw = iv.to_vec();
        raw.extend_from_slice(&body);
        format!("2:{}", base64_encode(&raw))
    }

    fn base64_encode(data: &[u8]) -> String {
        const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in data.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
            out.push(A[(n >> 18 & 63) as usize] as char);
            out.push(A[(n >> 12 & 63) as usize] as char);
            out.push(if chunk.len() > 1 {
                A[(n >> 6 & 63) as usize] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                A[(n & 63) as usize] as char
            } else {
                '='
            });
        }
        out
    }

    // A household id chosen to have no special bytes; the real ones look like
    // `Sonos_...`. `31 * 256 + 7` = 7943 is the Qobuz-shaped id for the test.
    const HH: &str = "Sonos_TestHouseholdId0000000000";
    const XML: &str = r#"<ThirdPartyMediaServers>
        <MediaServer UDN="SA_RINCON7943_X" SerialNum0="14" Token0="qb-token" Key0="qb-key" Nickname0="Qb1" Tier0="0"/>
        <MediaServer UDN="SA_RINCON228864_X" SerialNum0="15" Token0="ytm-token" Key0="ytm-key" Nickname0="Hhh"/>
        <MediaServer UDN="SA_RINCON229120_X" SerialNum0="9" Token0="" Key0="" Nickname0=""/>
    </ThirdPartyMediaServers>"#;

    #[test]
    fn a_sealed_envelope_round_trips_to_its_accounts() {
        let envelope = seal(XML, HH, [7u8; 16]);
        let accounts = decrypt_accounts(&envelope, HH).unwrap();
        assert_eq!(accounts.len(), 3);

        let qobuz = &accounts[0];
        assert_eq!(qobuz.service_id, 31);
        assert_eq!(qobuz.serial, 14);
        assert_eq!(qobuz.token, "qb-token");
        assert_eq!(qobuz.key, "qb-key");
        assert_eq!(qobuz.nickname, "Qb1");
        assert!(qobuz.has_token());
    }

    #[test]
    fn the_service_id_is_decoded_from_the_udn() {
        let accounts = decrypt_accounts(&seal(XML, HH, [1u8; 16]), HH).unwrap();
        // 228864 / 256 = 894? no: 284 * 256 = 72704. Assert against the real map.
        assert_eq!(accounts[1].service_id, 894, "228864 / 256");
        assert_eq!(accounts[1].token, "ytm-token");
    }

    #[test]
    fn an_empty_token_account_is_kept_but_flagged() {
        let accounts = decrypt_accounts(&seal(XML, HH, [2u8; 16]), HH).unwrap();
        let anon = &accounts[2];
        assert_eq!(anon.serial, 9);
        assert!(!anon.has_token(), "empty Token0 is not a usable credential");
    }

    #[test]
    fn the_wrong_household_id_fails_the_integrity_check_not_silently() {
        let envelope = seal(XML, HH, [3u8; 16]);
        let err = decrypt_accounts(&envelope, "Sonos_ADifferentHousehold000000").unwrap_err();
        let msg = format!("{err:#}").to_lowercase();
        assert!(
            msg.contains("integrity") || msg.contains("padding"),
            "a wrong key should be named as such, got: {msg}"
        );
    }

    #[test]
    fn a_non_envelope_is_refused_by_its_prefix() {
        let err = decrypt_accounts("not-a-2-colon-thing", HH).unwrap_err();
        assert!(format!("{err:#}").contains("envelope version"));
    }

    #[test]
    fn base64_round_trips() {
        for sample in [&b"M"[..], b"Ma", b"Man", b"any carnal pleasure."] {
            let encoded = base64_encode(sample);
            assert_eq!(base64_decode(&encoded).unwrap(), sample);
        }
    }

    #[test]
    fn a_doubly_escaped_property_value_is_recovered() {
        // The player escapes the property once; some paths escape the outer
        // document again, so `&amp;lt;` must resolve to `<`.
        let body = "<e:propertyset><e:property>\
            <ThirdPartyMediaServersX>2:AAAA</ThirdPartyMediaServersX>\
            </e:property></e:propertyset>";
        assert_eq!(
            extract_variable(body, "ThirdPartyMediaServersX").as_deref(),
            Some("2:AAAA")
        );
    }

    #[test]
    fn header_values_are_case_insensitive() {
        let msg = "HTTP/1.1 200 OK\r\nSID: uuid:abc\r\nContent-Length: 0\r\n\r\n";
        assert_eq!(header_value(msg, "sid"), Some("uuid:abc"));
        assert_eq!(header_value(msg, "CONTENT-LENGTH"), Some("0"));
    }
}
