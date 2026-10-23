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
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};

use crate::netid::local_ip_toward;
use crate::sonos::http;
use crate::sonos::upnp::PORT;

/// The speaker salt: the fixed 16 bytes mixed with the household id to derive
/// the blob key. x2rock does not carry it; the person supplies it, and
/// [`crate::salt`] keeps it. Every decrypt here takes it as an argument.
///
/// What it recovers is not a new capability. For a service whose AppLink flow
/// `link` can complete itself, the token minted that way is an exact match for
/// the one this decrypts: the same `{token, key}` SMAPI `loginToken`, for the
/// same account - `getAppLink` nests a `deviceLink` and returns the same
/// `authToken`/`privateKey` shape a device link does (see `smapi.rs`). The salt
/// is a shortcut to a credential the device could mint for itself; for the
/// cloud-completing services the Sonos app finishes on your behalf it is the only
/// route to that token, but it is still the same token, not a larger one.
///
/// And it is spent the same way a minted token is: SMAPI search and browse, and
/// play where the service allows, spoken straight to the service's own SMAPI
/// endpoint. It never touches the Sonos cloud. A firmware update could in
/// principle change it, at which point the integrity check below fails and says
/// so, rather than returning garbage.
pub type Salt = [u8; 16];

/// One music-service account as the household stores it.
///
/// The `token`/`key` pair is what goes into the SMAPI `loginToken` header - the
/// same two fields `credentials::Account` keeps. Everything else is here to name
/// the account and match it to a service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredAccount {
    /// The Sonos service id, decoded from the account UDN.
    pub service_id: u32,
    /// `SerialNum<i>` - the household's own selector for this account, the
    /// `sn_N` seen elsewhere.
    pub serial: u32,
    /// `authToken`. Secret.
    pub token: String,
    /// `privateKey`. Secret; empty for some services, which is legitimate.
    pub key: String,
    /// `Nickname<i>`, what the person or app named the account. Often empty.
    ///
    /// The Sonos app names the *second* account of a service after its key -
    /// `iHeartRadio 885ebbcc` beside a plain `iHeartRadio` - which is how a
    /// person tells them apart, and why this is worth keeping.
    pub nickname: String,
    /// `Tier<i>`, a service-specific tier marker. Kept only for display.
    pub tier: String,
    /// The account's own key, out of `Username<i>`: `X_#Svc<type>-<key>-Token`.
    ///
    /// An account *selector*, not an identity: the same account at the same
    /// service gets a different key in a different household, so it is assigned
    /// per registration. Independent of the nickname, which a person can rename
    /// at will, and literally `0` for some registrations - so it does **not** on
    /// its own distinguish two accounts. The serial does that.
    ///
    /// Not a secret: the same string is in the cdudn of every favorite that
    /// plays from the account, unencrypted.
    pub account_key: String,
    /// `Flags<i>`, unparsed. `4` where `Username<i>` carries a real key and `0`
    /// where it carries a literal zero or nothing - thirteen records for
    /// thirteen across two households, Deezer falling on both sides, so it is
    /// not a property of the service. What decides it is unknown. Kept because
    /// it is cheap, not because anything reads it.
    pub flags: String,
}

impl StoredAccount {
    /// Whether this record actually carries a credential. An account can be
    /// listed with an empty token (an anonymous service left a placeholder, or a
    /// half-removed account); such a row is real but useless to inject.
    pub fn has_token(&self) -> bool {
        !self.token.is_empty()
    }
}

/// The short household id the account blob is keyed to: the long one up to its
/// first dot. The SMAPI header and the credentials record want the long form,
/// `GetHouseholdID` returns the short one.
pub fn short_household(long: &str) -> &str {
    long.split_once('.').map_or(long, |(short, _)| short)
}

/// Decrypt the `ThirdPartyMediaServersX` envelope into the accounts it holds.
///
/// `household_id` is the short form (`Sonos_...`, no `.suffix`), exactly as
/// `DeviceProperties GetHouseholdID` returns it - the long form the SMAPI header
/// wants is *not* the key here. A wrong household id fails the integrity check
/// rather than returning nonsense.
pub fn decrypt_accounts(
    encoded: &str,
    household_id: &str,
    salt: &Salt,
) -> Result<Vec<StoredAccount>> {
    let payload = decrypt_payload(encoded, household_id, salt)?;
    parse_accounts(&payload)
}

/// The `2:`-prefixed, base64'd `AES-128-CBC(iv + ciphertext)` envelope, decoded
/// and verified, returning the account XML.
fn decrypt_payload(encoded: &str, household_id: &str, salt: &Salt) -> Result<Vec<u8>> {
    let encoded = encoded.trim();
    let body = encoded
        .strip_prefix("2:")
        .ok_or_else(|| anyhow!("unexpected account envelope version (want a `2:` prefix)"))?;
    let raw = super::base64::decode(body).context("account envelope was not valid base64")?;
    if raw.len() < 32 || (raw.len() - 16) % 16 != 0 {
        bail!("account envelope is the wrong size to be iv + AES blocks");
    }
    let (iv, ciphertext) = raw.split_at(16);
    let iv: &[u8; 16] = iv.try_into().expect("split_at(16) yields 16 bytes");

    // key = md5(iv + md5(household + salt))
    let global = md5(&[household_id.as_bytes(), &salt[..]]);
    let blob_key = md5(&[&iv[..], &global[..]]);
    let mut plain = aes_128_cbc_decrypt(ciphertext, &blob_key, iv);

    // PKCS#7: the last byte is the pad length, 1..=16.
    let pad = *plain
        .last()
        .ok_or_else(|| anyhow!("empty decrypted payload"))? as usize;
    if pad == 0 || pad > 16 || pad > plain.len() {
        bail!("wrong household id or salt, or a corrupt account payload (bad PKCS#7 padding)");
    }
    plain.truncate(plain.len() - pad);

    // The plaintext ends with the first four bytes of md5(payload): the integrity
    // tail that tells a correct decrypt from a wrong key.
    if plain.len() < 4 {
        bail!("decrypted account payload is too short to carry its checksum");
    }
    let (payload, checksum) = plain.split_at(plain.len() - 4);
    if md5(&[payload])[..4] != *checksum {
        bail!(
            "account payload integrity check failed - wrong household id or salt, or not this format"
        );
    }
    Ok(payload.to_vec())
}

/// One record as the payload carries it: the element's name, and its attributes
/// in the order they appear. Deliberately untyped - see [`decrypt_elements`].
pub type StoredRecord = (String, Vec<(String, String)>);

/// Every element of the decrypted payload, as its tag and raw attributes.
///
/// [`decrypt_accounts`] models what x2rock knows how to use; this models
/// nothing. It is for *looking* - at a household's records before importing
/// them, and at the attributes nothing here reads yet (`Md<i>`, `Flags<i>`,
/// `Password<i>`, and whatever a firmware adds next). An unknown attribute is
/// only discoverable if something is willing to print one it was not expecting.
///
/// Secrets are returned intact: this is a library function, and the caller that
/// prints them is the one that has to decide what a terminal may see.
pub fn decrypt_elements(
    encoded: &str,
    household_id: &str,
    salt: &Salt,
) -> Result<Vec<StoredRecord>> {
    let payload = decrypt_payload(encoded, household_id, salt)?;
    let text = String::from_utf8_lossy(&payload);
    let doc = roxmltree::Document::parse(&text).context("decrypted account XML did not parse")?;
    Ok(doc
        .descendants()
        .filter(|n| n.is_element() && n.attribute("UDN").is_some())
        .map(|n| {
            (
                n.tag_name().name().to_string(),
                n.attributes()
                    .map(|a| (a.name().to_string(), a.value().to_string()))
                    .collect(),
            )
        })
        .collect())
}

/// The number in a `SA_RINCON<type>_...` UDN: `type / 256` is the service
/// id and `type % 256` a schema revision.
pub fn encoded_type(udn: &str) -> Option<u32> {
    udn.strip_prefix("SA_RINCON")?
        .split('_')
        .next()?
        .parse()
        .ok()
}

/// Parse the decrypted account XML.
///
/// Each element with a `UDN` of `SA_RINCON<type>_...` is one *service*, where
/// `type / 256` is the service id and `type % 256` a schema revision.
///
/// **The trailing digit is read as an index, which is a guard rather than a
/// known fact.** Every element declares `NumAccounts`, and the `0` suffix on
/// `Token0`/`Key0`/`Nickname0`/`Tier0`/`SerialNum0`/`Username0` is consistent
/// with `Token1` and the rest following for a second account. Nothing seen
/// proves it: every element in both households tested declares
/// `NumAccounts="1"`, and a service with two accounts is served as two
/// elements, one per account - which is also how the format it descends from
/// did it, one `<Account>` per account. So the count and the suffix may simply
/// be leftovers.
///
/// The walk costs four lines and makes the question moot either way, which is
/// the only reason it is here. A missing or unreadable `NumAccounts` is taken
/// as one, and a run of indices stops early at an entry with no token or serial
/// rather than padding the list with empty records.
fn parse_accounts(payload: &[u8]) -> Result<Vec<StoredAccount>> {
    let text = String::from_utf8_lossy(payload);
    let doc = roxmltree::Document::parse(&text).context("decrypted account XML did not parse")?;
    let mut accounts = Vec::new();
    for node in doc.descendants().filter(|n| n.is_element()) {
        let Some(udn) = node.attribute("UDN") else {
            continue;
        };
        let Some(encoded_type) = encoded_type(udn) else {
            continue;
        };
        let count = node
            .attribute("NumAccounts")
            .and_then(|n| n.parse::<usize>().ok())
            .unwrap_or(1);
        for i in 0..count {
            let at = |name: &str| node.attribute(format!("{name}{i}").as_str());
            // An index the element claims but does not carry is the end of the
            // real run, not an empty account to report.
            if i > 0 && at("Token").is_none() && at("SerialNum").is_none() {
                break;
            }
            accounts.push(StoredAccount {
                service_id: encoded_type / 256,
                serial: at("SerialNum").and_then(|s| s.parse().ok()).unwrap_or(0),
                token: at("Token").unwrap_or_default().to_string(),
                key: at("Key").unwrap_or_default().to_string(),
                nickname: at("Nickname").unwrap_or_default().to_string(),
                tier: at("Tier").unwrap_or_default().to_string(),
                account_key: at("Username").map(account_key_in).unwrap_or_default(),
                flags: at("Flags").unwrap_or_default().to_string(),
            });
        }
    }
    Ok(accounts)
}

/// The account key inside a `Username` value: `X_#Svc<type>-<key>-Token`.
///
/// Anything that does not have that shape is handed back whole - it is a
/// display-and-keying aid, and a value nobody has seen is better carried
/// verbatim than silently emptied.
fn account_key_in(username: &str) -> String {
    username
        .rsplit_once("-Token")
        .map(|(head, _)| head)
        .and_then(|head| head.rsplit_once('-'))
        .map(|(_, key)| key.to_string())
        .unwrap_or_else(|| username.to_string())
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
        // Bounded by the overall deadline too: a peer that connects and then
        // says nothing must not hold the capture past the time it was given.
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let request = http::read_message(&mut socket, EXCHANGE.min(left), MAX_MESSAGE)
            .await
            .unwrap_or_default();
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

/// SUBSCRIBE to ZoneGroupTopology, returning the subscription id to cancel with.
async fn subscribe(player: IpAddr, local_ip: IpAddr, port: u16) -> Result<String> {
    let callback = match local_ip {
        IpAddr::V6(v6) => format!("<http://[{v6}]:{port}/notify>"),
        v4 => format!("<http://{v4}:{port}/notify>"),
    };
    let request = format!(
        "SUBSCRIBE /ZoneGroupTopology/Event HTTP/1.1\r\n\
         HOST: {player}:{PORT}\r\n\
         CALLBACK: {callback}\r\n\
         NT: upnp:event\r\n\
         TIMEOUT: Second-60\r\n\
         Content-Length: 0\r\n\
         Connection: close\r\n\r\n"
    );
    let mut stream =
        tokio::time::timeout(EXCHANGE, TcpStream::connect(SocketAddr::new(player, PORT)))
            .await
            .map_err(|_| anyhow!("{player} did not answer on port {PORT} within {EXCHANGE:?}"))?
            .context("subscribing to the player's topology events")?;
    stream.write_all(request.as_bytes()).await?;
    let response = http::read_message(&mut stream, EXCHANGE, MAX_MESSAGE)
        .await
        .context("subscribing to the player's topology events")?;
    http::header(&response, "SID")
        .map(str::to_string)
        .ok_or_else(|| anyhow!("the player accepted the subscription but named no SID"))
}

/// Best-effort UNSUBSCRIBE. A lost one costs nothing: the subscription expires
/// on its own in a minute.
async fn unsubscribe(player: IpAddr, sid: &str) {
    let request = format!(
        "UNSUBSCRIBE /ZoneGroupTopology/Event HTTP/1.1\r\n\
         HOST: {player}:{PORT}\r\n\
         SID: {sid}\r\n\
         Connection: close\r\n\r\n"
    );
    // Bounded as a whole: a speaker that stops answering would otherwise hold
    // the command until the OS gave up on the connect, minutes later, to save
    // a subscription that expires in one.
    let _ = tokio::time::timeout(UNSUBSCRIBE, async {
        let mut stream = TcpStream::connect(SocketAddr::new(player, PORT)).await?;
        stream.write_all(request.as_bytes()).await?;
        http::read_message(&mut stream, UNSUBSCRIBE, MAX_MESSAGE).await
    })
    .await;
}

/// How long one HTTP exchange with a player may take - a SUBSCRIBE's answer,
/// or the NOTIFY it sends back. Each is one small message on a LAN.
const EXCHANGE: Duration = Duration::from_secs(5);

/// The whole UNSUBSCRIBE, connect to answer. Best effort; see [`unsubscribe`].
const UNSUBSCRIBE: Duration = Duration::from_secs(3);

/// The most one message may hold. The account event is 10-20 KB; anything near
/// this is not a player's NOTIFY, and is not read into memory to find out.
const MAX_MESSAGE: usize = 512 * 1024;

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

    /// A synthetic salt, not Sonos's: the round trip below proves the
    /// arithmetic with any 16 bytes, so the real value never enters the tree.
    const SALT: Salt = *b"x2rock-test-salt";

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
        format!("2:{}", crate::sonos::base64::encode(&raw))
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
        let accounts = decrypt_accounts(&envelope, HH, &SALT).unwrap();
        assert_eq!(accounts.len(), 3);

        let qobuz = &accounts[0];
        assert_eq!(qobuz.service_id, 31);
        assert_eq!(qobuz.serial, 14);
        assert_eq!(qobuz.token, "qb-token");
        assert_eq!(qobuz.key, "qb-key");
        assert_eq!(qobuz.nickname, "Qb1");
        assert!(qobuz.has_token());
        // No `Username0` in this vector, so no account key - the field is
        // absent rather than defaulted to something that looks like one.
        assert_eq!(qobuz.account_key, "");
    }

    #[test]
    fn an_element_holding_two_accounts_yields_both() {
        // What `NumAccounts` is for. No household has been seen serving this
        // shape - the two iHeartRadio accounts in the household this was
        // written against arrive as two elements - so it is pinned with a
        // synthetic vector rather than left to be discovered by losing a token.
        const TWO: &str = r#"<ThirdPartyMediaServers>
            <MediaServer UDN="SA_RINCON1543_X" NumAccounts="2"
              SerialNum0="24" Token0="ihr-a" Key0="" Nickname0="iHeartRadio"
              Username0="X_#Svc1543-81dee58d-Token" Flags0="4" Tier0="0"
              SerialNum1="25" Token1="ihr-b" Key1="" Nickname1="iHeartRadio 885ebbcc"
              Username1="X_#Svc1543-885ebbcc-Token" Flags1="4" Tier1="0"/>
        </ThirdPartyMediaServers>"#;
        let accounts = decrypt_accounts(&seal(TWO, HH, [3u8; 16]), HH, &SALT).unwrap();
        assert_eq!(accounts.len(), 2, "both indices are read");
        assert_eq!(accounts[0].serial, 24);
        assert_eq!(accounts[0].token, "ihr-a");
        assert_eq!(accounts[0].account_key, "81dee58d");
        assert_eq!(accounts[1].serial, 25);
        assert_eq!(accounts[1].token, "ihr-b");
        assert_eq!(accounts[1].nickname, "iHeartRadio 885ebbcc");
        assert_eq!(accounts[1].account_key, "885ebbcc");
        assert_eq!(accounts[1].service_id, accounts[0].service_id);
    }

    #[test]
    fn a_count_larger_than_what_is_there_stops_at_the_real_end() {
        // A claim is not a promise: three announced, one carried.
        const OVER: &str = r#"<ThirdPartyMediaServers>
            <MediaServer UDN="SA_RINCON7943_X" NumAccounts="3"
              SerialNum0="14" Token0="qb-token" Key0="qb-key" Nickname0="Qb1"/>
        </ThirdPartyMediaServers>"#;
        let accounts = decrypt_accounts(&seal(OVER, HH, [5u8; 16]), HH, &SALT).unwrap();
        assert_eq!(accounts.len(), 1, "no empty records padded onto the end");
        assert_eq!(accounts[0].token, "qb-token");
    }

    #[test]
    fn an_account_key_is_read_out_of_the_username() {
        assert_eq!(account_key_in("X_#Svc1543-885ebbcc-Token"), "885ebbcc");
        // A service with no per-account key writes a literal zero there.
        assert_eq!(account_key_in("X_#Svc44551-0-Token"), "0");
        // An unfamiliar shape is carried whole rather than silently emptied.
        assert_eq!(account_key_in("something-else"), "something-else");
    }

    #[test]
    fn the_raw_view_keeps_attributes_nothing_models() {
        const ODD: &str = r#"<ThirdPartyMediaServers>
            <MediaServer UDN="SA_RINCON7943_X" SerialNum0="14" Token0="t" Key0="k"
              Md0="" Flags0="4" Whatever0="new-in-some-firmware"/>
            <NotAnAccount Name="no UDN, not a record"/>
        </ThirdPartyMediaServers>"#;
        let raw = decrypt_elements(&seal(ODD, HH, [9u8; 16]), HH, &SALT).unwrap();
        assert_eq!(raw.len(), 1, "only elements carrying a UDN are records");
        let (tag, attrs) = &raw[0];
        assert_eq!(tag, "MediaServer");
        let named = |k: &str| attrs.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        // Including the ones the modelled parser throws away, which is the
        // entire reason this exists.
        assert_eq!(named("Flags0"), Some("4"));
        assert_eq!(named("Md0"), Some(""));
        assert_eq!(named("Whatever0"), Some("new-in-some-firmware"));
    }

    #[test]
    fn the_service_id_is_decoded_from_the_udn() {
        let accounts = decrypt_accounts(&seal(XML, HH, [1u8; 16]), HH, &SALT).unwrap();
        // 228864 / 256 = 894? no: 284 * 256 = 72704. Assert against the real map.
        assert_eq!(accounts[1].service_id, 894, "228864 / 256");
        assert_eq!(accounts[1].token, "ytm-token");
    }

    #[test]
    fn an_empty_token_account_is_kept_but_flagged() {
        let accounts = decrypt_accounts(&seal(XML, HH, [2u8; 16]), HH, &SALT).unwrap();
        let anon = &accounts[2];
        assert_eq!(anon.serial, 9);
        assert!(!anon.has_token(), "empty Token0 is not a usable credential");
    }

    #[test]
    fn the_wrong_household_id_fails_the_integrity_check_not_silently() {
        let envelope = seal(XML, HH, [3u8; 16]);
        let err =
            decrypt_accounts(&envelope, "Sonos_ADifferentHousehold000000", &SALT).unwrap_err();
        let msg = format!("{err:#}").to_lowercase();
        assert!(
            msg.contains("integrity") || msg.contains("padding"),
            "a wrong key should be named as such, got: {msg}"
        );
    }

    #[test]
    fn the_wrong_salt_fails_the_integrity_check_not_silently() {
        let envelope = seal(XML, HH, [3u8; 16]);
        let err = decrypt_accounts(&envelope, HH, b"not-the-salt-xyz").unwrap_err();
        let msg = format!("{err:#}").to_lowercase();
        assert!(
            msg.contains("salt"),
            "a wrong salt should be named as a suspect, got: {msg}"
        );
    }

    #[test]
    fn a_non_envelope_is_refused_by_its_prefix() {
        let err = decrypt_accounts("not-a-2-colon-thing", HH, &SALT).unwrap_err();
        assert!(format!("{err:#}").contains("envelope version"));
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
}
