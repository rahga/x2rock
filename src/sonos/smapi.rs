//! SMAPI: searching a music service directly, the way a player does.
//!
//! SMAPI is SOAP 1.1 over HTTPS in the namespace `http://www.sonos.com/Services/1.1`.
//! Sonos is the *client* and the music service is the server, so a controller that
//! wants search does not ask a player for it - it calls the service itself.
//!
//! **This is the only part of x2rock that leaves the LAN**, and it is confined to
//! the CLI on purpose. See "Rule: search never enters the daemon" in
//! docs/architecture.md: the daemon publishes MPRIS and must never acquire an
//! internet timeout in front of play/pause.
//!
//! Two kinds of service can be searched. `Policy Auth="Anonymous"` needs nothing
//! but a `deviceProvider`, which is about a third of the catalogue and most of
//! the radio-shaped half of it. `DeviceLink` services need a `loginToken`, and
//! this module can now *mint* one: `getDeviceLinkCode` and `getDeviceAuthToken`
//! are the device-link flow, driven by the controller, with the browser step
//! handed to whatever browser the person already uses.
//!
//! `AppLink` is the tier that stays out of reach. It expects the Sonos app to
//! launch the service's own mobile app, there is no desktop app to hand off to,
//! and at least one of them (YouTube Music) gates the endpoint on an API key
//! before user auth is even reached.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use roxmltree::Document;
use serde::{Deserialize, Serialize};

use super::http;

/// Short, and deliberately shorter than the LAN's. Search is invoked from a bar
/// widget as a subprocess; a call that never returns is worse than one that fails.
pub const TIMEOUT: Duration = Duration::from_secs(6);

const NS: &str = "http://www.sonos.com/Services/1.1";

/// How long a device link may take before x2rock stops asking. Seven minutes is
/// the documented ceiling, and the person is in a browser for most of it.
pub const LINK_DEADLINE: Duration = Duration::from_secs(7 * 60);

/// Between polls of `getDeviceAuthToken`. Slow enough not to hammer a service
/// while someone types a password, fast enough that finishing feels immediate.
pub const LINK_POLL: Duration = Duration::from_secs(3);

/// How a service expects to be authenticated, from its `<Policy Auth=...>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Auth {
    /// No credential at all beyond `deviceProvider`. Usable as-is.
    Anonymous,
    /// A code typed into, or embedded in, a web page. `x2rock link` drives it.
    DeviceLink,
    /// Hands off to the service's own app. Not drivable from a Linux desktop.
    AppLink,
}

/// A `loginToken` for the SMAPI credentials header: what a completed device link
/// minted. Held in [`crate::credentials`], which is the only thing that persists
/// it; this type is just the shape the envelope wants.
#[derive(Debug, Clone)]
pub struct Token {
    pub token: String,
    pub key: String,
    /// The household the token was minted against. Optional because the
    /// envelope tolerates its absence, and a search run from a cached catalogue
    /// with no player on the LAN has no way to look one up.
    pub household: Option<String>,
}

/// What `getDeviceLinkCode` returns: where to send the person, and the code.
#[derive(Debug, Clone)]
pub struct LinkCode {
    /// The page to open. On Linux that is `xdg-open` and nothing else.
    pub reg_url: String,
    pub link_code: String,
    /// Whether the person has to *type* the code. False when it is already a
    /// query parameter of `reg_url`, which is the graceful case: open a link,
    /// log in, done.
    pub show_link_code: bool,
}

/// What `getDeviceAuthToken` returns once the browser half is finished.
#[derive(Debug, Clone)]
pub struct DeviceAuth {
    pub auth_token: String,
    pub private_key: String,
    /// Handed to the controller by design - it is the controller that later
    /// calls `musicServiceAccounts:1 match`. Not every service sends one.
    pub user_id_hash_code: Option<String>,
}

/// A SOAP fault, which during a device link is not necessarily a failure.
#[derive(Debug, Clone)]
pub struct Fault {
    /// `faultcode`, e.g. `Client.NOT_LINKED_RETRY`.
    pub code: String,
    /// `faultstring`, which carries the useful half of the story.
    pub message: String,
    /// `<SonosError>` from the fault detail. 5 means "not linked yet".
    pub sonos_error: Option<u32>,
}

impl Fault {
    /// The one fault that means *keep waiting*, not *this failed*.
    ///
    /// Both signals are checked because they are documented together and
    /// services are inconsistent about which they populate.
    pub fn is_pending(&self) -> bool {
        self.code.contains("NOT_LINKED_RETRY") || self.sonos_error == Some(5)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Service {
    pub id: String,
    pub name: String,
    /// The SMAPI endpoint. `SecureUri` where the descriptor gives one.
    pub uri: String,
    pub auth: Auth,
    /// Where the manifest lives, which is where the search endpoints and the
    /// presentation map are named.
    pub manifest_uri: Option<String>,
    /// `serviceId * 256 + type`, from `AvailableServiceTypeList`. The number a
    /// cdudn is built from: `SA_RINCON<type>_X_#Svc<type>-0-Token`, which is how
    /// enqueued content names the account the player should resolve it with.
    /// Absent for a service the type list does not mention.
    pub service_type: Option<u32>,
}

impl Service {
    /// The `<desc id="cdudn">` an enqueued item must carry for this service.
    ///
    /// Derived, not scavenged: the arithmetic reproduces the `SA_RINCON77575`
    /// that Sonos Radio's own favorites carry (303 * 256 + 7), which is what
    /// gives any confidence that it is right for services with no favorite to
    /// copy from.
    pub fn cdudn(&self) -> Option<String> {
        let t = self.service_type?;
        Some(format!("SA_RINCON{t}_X_#Svc{t}-0-Token"))
    }
}

/// One search hit, flattened from `mediaMetadata`.
#[derive(Debug, Clone)]
pub struct Item {
    pub id: String,
    pub title: String,
    /// `stream`, `track`, `album`, `artist`, `playlist`, ...
    pub item_type: String,
    pub summary: Option<String>,
    /// Whatever the service offers as a cover. Services put it in different
    /// places: `albumArtURI` for a track, a station's logo nested under
    /// `streamMetadata`.
    pub art_url: Option<String>,
}

/// A searchable category, from the service's presentation map.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Category {
    /// What a person picks: `stations`, `tracks`, `all`.
    pub id: String,
    /// What the service is actually sent: `search:station`, `SONGS`.
    pub mapped_id: String,
}

/// Parse the descriptor list `MusicServices ListAvailableServices` returns.
///
/// The list is every service Sonos knows about, not the household's - there is
/// no command that gives the second. Filtering to what is usable is the caller's
/// job and mostly means [`Auth::Anonymous`].
pub fn parse_services(descriptor_list: &str, type_list: &str) -> Result<Vec<Service>> {
    // serviceId -> serviceId * 256 + type. The list gives only the combined
    // number, so the id it belongs to is the number shifted back down.
    let types: std::collections::HashMap<u32, u32> = type_list
        .split(',')
        .filter_map(|t| t.trim().parse::<u32>().ok())
        .map(|t| (t / 256, t))
        .collect();
    let doc = Document::parse(descriptor_list).context("parsing service descriptors")?;
    let mut out = Vec::new();
    for node in doc.descendants().filter(|n| n.has_tag_name("Service")) {
        let Some(id) = node.attribute("Id") else {
            continue;
        };
        let uri = node
            .attribute("SecureUri")
            .or_else(|| node.attribute("Uri"))
            .unwrap_or_default();
        if uri.is_empty() {
            continue;
        }
        let policy = node.children().find(|n| n.has_tag_name("Policy"));
        // The household's own catalogue splits exactly three ways, so anything
        // unrecognised is treated as AppLink: the conservative choice, since it
        // is the tier x2rock cannot drive and mislabelling it that way costs a
        // clear error rather than a confusing failure mid-flow.
        let auth = match policy.and_then(|p| p.attribute("Auth")) {
            Some("Anonymous") => Auth::Anonymous,
            Some("DeviceLink") => Auth::DeviceLink,
            _ => Auth::AppLink,
        };
        out.push(Service {
            id: id.to_string(),
            name: node.attribute("Name").unwrap_or(id).to_string(),
            uri: uri.to_string(),
            auth,
            manifest_uri: node
                .children()
                .find(|n| n.has_tag_name("Manifest"))
                .and_then(|n| n.attribute("Uri"))
                .map(str::to_string),
            service_type: id.parse().ok().and_then(|n: u32| types.get(&n).copied()),
        });
    }
    if out.is_empty() {
        bail!("no services in the descriptor list");
    }
    Ok(out)
}

/// The categories a service will accept in `search`, from its presentation map.
///
/// The manifest names the presentation map; both are plain documents on Sonos's
/// CDN, fetched with no credential. A service with no `SearchCategories` cannot
/// be searched, and says so by returning an empty list rather than by failing -
/// that is a fact about the service, not an error.
pub async fn categories(service: &Service) -> Result<Vec<Category>> {
    let Some(manifest_uri) = &service.manifest_uri else {
        return Ok(Vec::new());
    };
    let (status, body) = http::get(manifest_uri, TIMEOUT).await?;
    if status != 200 {
        bail!("{} manifest: HTTP {status}", service.name);
    }
    let manifest: serde_json::Value =
        serde_json::from_str(&body).with_context(|| format!("{} manifest", service.name))?;
    let Some(map_uri) = manifest
        .get("presentationMap")
        .and_then(|m| m.get("uri"))
        .and_then(|u| u.as_str())
    else {
        return Ok(Vec::new());
    };

    let (status, body) = http::get(map_uri, TIMEOUT).await?;
    if status != 200 {
        bail!("{} presentation map: HTTP {status}", service.name);
    }
    let doc =
        Document::parse(&body).with_context(|| format!("{} presentation map", service.name))?;
    Ok(doc
        .descendants()
        .filter(|n| n.has_tag_name("Category"))
        .filter_map(|n| {
            let id = n.attribute("id")?;
            Some(Category {
                id: id.to_string(),
                mapped_id: n.attribute("mappedId").unwrap_or(id).to_string(),
            })
        })
        .collect())
}

/// `search`, returning the hits and the total the service claims.
pub async fn search(
    service: &Service,
    token: Option<&Token>,
    category: &str,
    term: &str,
    index: u32,
    count: u32,
) -> Result<(Vec<Item>, u32)> {
    let body = call(
        service,
        token,
        "search",
        &format!(
            "<id>{}</id><term>{}</term><index>{index}</index><count>{count}</count>",
            escape(category),
            escape(term)
        ),
    )
    .await?;
    let doc = Document::parse(&body).context("parsing search response")?;
    let total = doc
        .descendants()
        .find(|n| n.has_tag_name("total"))
        .and_then(|n| n.text())
        .and_then(|t| t.parse().ok())
        .unwrap_or(0);
    let items = doc
        .descendants()
        .filter(|n| n.has_tag_name("mediaMetadata") || n.has_tag_name("mediaCollection"))
        .filter_map(|n| {
            let child = |tag: &str| {
                n.children()
                    .find(|c| c.has_tag_name(tag))
                    .and_then(|c| c.text())
                    .map(str::to_string)
            };
            let nested = |parent: &str, tag: &str| {
                n.children()
                    .find(|c| c.has_tag_name(parent))
                    .and_then(|c| c.children().find(|g| g.has_tag_name(tag)))
                    .and_then(|g| g.text())
                    .map(str::to_string)
            };
            Some(Item {
                id: child("id")?,
                title: child("title").unwrap_or_default(),
                item_type: child("itemType").unwrap_or_default(),
                // Whatever the service offers as a second line; services differ
                // on which of these they populate, and most populate one.
                summary: child("summary")
                    .or_else(|| child("artist"))
                    .or_else(|| child("genre"))
                    .or_else(|| child("country")),
                art_url: child("albumArtURI").or_else(|| nested("streamMetadata", "logo")),
            })
        })
        .collect();
    Ok((items, total))
}

/// `getMediaURI`, turning a search hit into something a player can be handed.
pub async fn media_uri(service: &Service, token: Option<&Token>, id: &str) -> Result<String> {
    let body = call(
        service,
        token,
        "getMediaURI",
        &format!("<id>{}</id>", escape(id)),
    )
    .await?;
    let doc = Document::parse(&body).context("parsing getMediaURI response")?;
    doc.descendants()
        .find(|n| n.has_tag_name("getMediaURIResult"))
        .and_then(|n| n.text())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("{} returned no media URI for {id}", service.name))
}

/// Ask a device-link service where to send the person.
///
/// Sent with no credential at all beyond `deviceProvider` - the whole point of
/// the flow is that there is nothing to send yet.
pub async fn device_link_code(service: &Service, household: &str) -> Result<LinkCode> {
    ensure!(
        service.auth == Auth::DeviceLink,
        "{} does not use device linking ({:?}), so there is no link code to ask for",
        service.name,
        service.auth
    );
    let body = match call_soap(
        service,
        None,
        "getDeviceLinkCode",
        &format!("<householdId>{}</householdId>", escape(household)),
    )
    .await?
    {
        Ok(body) => body,
        Err(fault) => bail!(
            "{} refused getDeviceLinkCode: {}",
            service.name,
            fault.message
        ),
    };
    let doc = Document::parse(&body).context("parsing getDeviceLinkCode response")?;
    let field = |tag: &str| {
        doc.descendants()
            .find(|n| n.has_tag_name(tag))
            .and_then(|n| n.text())
            .map(str::to_string)
    };
    let link_code =
        field("linkCode").ok_or_else(|| anyhow!("{} returned no linkCode", service.name))?;
    Ok(LinkCode {
        reg_url: field("regUrl")
            .ok_or_else(|| anyhow!("{} returned no regUrl to open", service.name))?,
        link_code,
        // Absent means the code is not needed on screen. Defaulting to false
        // rather than true because the failure it causes is cosmetic, whereas
        // printing a code the person cannot use is confusing.
        show_link_code: field("showLinkCode").is_some_and(|v| v.trim() == "true"),
    })
}

/// Ask whether the browser half has finished.
///
/// `Ok(None)` is *pending*, not failure: the service answers a SOAP fault with
/// `Client.NOT_LINKED_RETRY` (`SonosError` 5) for as long as the person has not
/// finished logging in, and treating that as an error would abort the flow on
/// its normal first reply.
pub async fn device_auth_token(
    service: &Service,
    household: &str,
    link_code: &str,
) -> Result<Option<DeviceAuth>> {
    let body = match call_soap(
        service,
        None,
        "getDeviceAuthToken",
        &format!(
            "<householdId>{}</householdId><linkCode>{}</linkCode>",
            escape(household),
            escape(link_code)
        ),
    )
    .await?
    {
        Ok(body) => body,
        Err(fault) if fault.is_pending() => return Ok(None),
        Err(fault) => bail!(
            "{} refused getDeviceAuthToken: {}",
            service.name,
            fault.message
        ),
    };
    parse_device_auth(&service.name, &body).map(Some)
}

fn parse_device_auth(service_name: &str, body: &str) -> Result<DeviceAuth> {
    let doc = Document::parse(body).context("parsing getDeviceAuthToken response")?;
    let field = |tag: &str| {
        doc.descendants()
            .find(|n| n.has_tag_name(tag))
            .and_then(|n| n.text())
            .map(str::to_string)
    };
    Ok(DeviceAuth {
        auth_token: field("authToken")
            .ok_or_else(|| anyhow!("{service_name} linked but returned no authToken"))?,
        // A service that sends a token and no key is taken at its word rather
        // than refused: the header carries an empty key fine, and the search
        // that follows is the honest test of whether it was enough.
        private_key: field("privateKey").unwrap_or_default(),
        user_id_hash_code: field("userIdHashCode"),
    })
}

/// The credentials header and the body around one action.
///
/// No XML declaration and no BOM: services answer one with
/// `s:Client / Expecting state 'Element'`, which reads like a malformed request
/// and is really just the prologue.
fn envelope(action: &str, params: &str, token: Option<&Token>) -> String {
    let login = match token {
        None => String::new(),
        Some(t) => {
            let household = t
                .household
                .as_deref()
                .map(|h| format!("<householdId>{}</householdId>", escape(h)))
                .unwrap_or_default();
            format!(
                "<loginToken><token>{}</token><key>{}</key>{household}</loginToken>",
                escape(&t.token),
                escape(&t.key)
            )
        }
    };
    format!(
        concat!(
            r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">"#,
            r#"<s:Header><credentials xmlns="{ns}">"#,
            r#"<deviceProvider>Sonos</deviceProvider>{login}"#,
            r#"</credentials></s:Header>"#,
            r#"<s:Body><{action} xmlns="{ns}">{params}</{action}></s:Body></s:Envelope>"#
        ),
        ns = NS,
        login = login,
        action = action,
        params = params
    )
}

/// One SMAPI call, with a fault returned rather than raised.
///
/// The split exists for the link flow alone: `getDeviceAuthToken` answers a
/// fault on every poll but the last, so its caller needs the `faultcode`, not a
/// formatted error string.
async fn call_soap(
    service: &Service,
    token: Option<&Token>,
    action: &str,
    params: &str,
) -> Result<std::result::Result<String, Fault>> {
    let (endpoint, path, tls) = http::parse_url(&service.uri)?;
    let envelope = envelope(action, params, token);
    let soap_action = format!("\"{NS}#{action}\"");
    let (status, text) = http::post(
        &endpoint,
        tls,
        &path,
        &[
            ("Content-Type", "text/xml; charset=utf-8"),
            ("SOAPACTION", &soap_action),
        ],
        &envelope,
        TIMEOUT,
    )
    .await?;

    // **The body decides whether this is a fault, not the status.** SOAP 1.1
    // says a fault travels with HTTP 500, and most of them do - but Bandcamp
    // answers `getDeviceAuthToken` with HTTP 200 and a `<s:Fault>` for every
    // poll before the last one, which is the *normal* path through a device
    // link. Trusting the status there read the pending fault as a successful
    // reply and reported "linked but returned no authToken" seconds into a flow
    // that had not started yet. Verified against Bandcamp 2026-08-31.
    if std::env::var_os("X2ROCK_DUMP_SMAPI").is_some() {
        eprintln!(
            "--- {action} request ---\n{}\n--- reply HTTP {status} ---\n{text}\n---",
            without_credentials(&envelope)
        );
    }
    if let Some(fault) = fault_in(&text) {
        return Ok(Err(fault));
    }
    if status != 200 {
        return Ok(Err(parse_fault(&text, status)));
    }
    Ok(Ok(text))
}

/// An envelope with its whole credentials header removed, for `X2ROCK_DUMP_SMAPI`.
///
/// The *whole* header, not the token and key within it: a redaction that has to
/// enumerate which fields are secret is one field away from printing a token
/// into a terminal or a log, and the header has never been the interesting half
/// of a request that is behaving oddly.
fn without_credentials(envelope: &str) -> String {
    const CLOSE: &str = "</s:Header>";
    match (envelope.find("<s:Header>"), envelope.find(CLOSE)) {
        (Some(start), Some(end)) if start < end => format!(
            "{}<s:Header>(credentials omitted)</s:Header>{}",
            &envelope[..start],
            &envelope[end + CLOSE.len()..]
        ),
        _ => envelope.to_string(),
    }
}

/// The fault in a reply, if the reply is one.
fn fault_in(text: &str) -> Option<Fault> {
    let doc = Document::parse(text).ok()?;
    if !doc.descendants().any(|n| n.has_tag_name("Fault")) {
        return None;
    }
    let field = |tag: &str| {
        doc.descendants()
            .find(|n| n.has_tag_name(tag))
            .and_then(|n| n.text())
            .map(str::to_string)
    };
    Some(Fault {
        code: field("faultcode").unwrap_or_default(),
        message: field("faultstring").unwrap_or_else(|| "a fault with no faultstring".to_string()),
        sonos_error: field("SonosError").and_then(|v| v.trim().parse().ok()),
    })
}

/// A failure that did not arrive as a readable SOAP fault - a proxy's error
/// page, a truncated body, a bare 500.
///
/// The `NOT_LINKED_RETRY` substring check is not tidiness: seven minutes of
/// polling must not abort because one reply came back malformed, and the token
/// would be lost for a reason that had nothing to do with the person or the
/// service.
fn parse_fault(text: &str, status: u16) -> Fault {
    fault_in(text).unwrap_or_else(|| Fault {
        code: if text.contains("NOT_LINKED_RETRY") {
            "NOT_LINKED_RETRY".to_string()
        } else {
            String::new()
        },
        message: format!("HTTP {status}"),
        sonos_error: None,
    })
}

/// One SMAPI call for everything that is not the link flow.
///
/// A service that needs an account is refused here rather than sent and
/// rejected, and the error names the command that would fix it.
async fn call(
    service: &Service,
    token: Option<&Token>,
    action: &str,
    params: &str,
) -> Result<String> {
    if service.auth != Auth::Anonymous && token.is_none() {
        match service.auth {
            Auth::DeviceLink => bail!(
                "{} needs a linked account. Run `x2rock link {}` once and it will work from then on.",
                service.name,
                service.name
            ),
            _ => bail!(
                "{} authenticates by handing off to its own app, which a Linux \
                 desktop cannot do. x2rock cannot link it.",
                service.name
            ),
        }
    }
    match call_soap(service, token, action, params).await? {
        Ok(body) => Ok(body),
        Err(fault) => bail!("{} refused {action}: {}", service.name, fault.message),
    }
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    const DESCRIPTORS: &str = r#"<Services SchemaVersion="1">
        <Service Id="254" Name="TuneIn" Uri="http://legato/x" SecureUri="https://legato/x"
                 ContainerType="MService" Capabilities="29364801">
          <Policy Auth="Anonymous" PollInterval="0"/>
          <Manifest Version="259" Uri="https://cdn/m/tunein"/>
        </Service>
        <Service Id="284" Name="YouTube Music" Uri="https://ytm/x" ContainerType="MService">
          <Policy Auth="AppLink" PollInterval="60"/>
        </Service>
        <Service Id="200" Name="Bandcamp" Uri="https://bandcamp/smapi" ContainerType="MService">
          <Policy Auth="DeviceLink" PollInterval="0"/>
        </Service>
        <Service Id="999" Name="Broken"/>
      </Services>"#;

    #[test]
    fn a_cdudn_is_derived_from_the_service_type_list() {
        // 303 * 256 + 7 = 77575, which is the SA_RINCON this household's Sonos
        // Radio favorites actually carry - the check that the arithmetic is
        // right for services with no favorite to copy from.
        let services = parse_services(
            r#"<Services><Service Id="303" Name="Sonos Radio" Uri="https://x/y">
                 <Policy Auth="DeviceLink"/></Service>
               <Service Id="284" Name="YouTube Music" Uri="https://y/z">
                 <Policy Auth="AppLink"/></Service>
               <Service Id="999" Name="Unlisted" Uri="https://z/z">
                 <Policy Auth="Anonymous"/></Service></Services>"#,
            "77575,72711",
        )
        .unwrap();
        assert_eq!(services[0].auth, Auth::DeviceLink);
        assert_eq!(services[0].service_type, Some(77575));
        assert_eq!(
            services[0].cdudn().as_deref(),
            Some("SA_RINCON77575_X_#Svc77575-0-Token")
        );
        assert_eq!(services[1].service_type, Some(72711), "284 * 256 + 7");
        // A service the type list does not mention has no cdudn to offer, and
        // says so rather than inventing one.
        assert_eq!(services[2].service_type, None);
        assert!(services[2].cdudn().is_none());
    }

    #[test]
    fn descriptors_split_by_how_they_authenticate() {
        let services = parse_services(DESCRIPTORS, "").unwrap();
        // The one with no Uri at all is dropped rather than carried as unusable.
        assert_eq!(services.len(), 3);

        let tunein = &services[0];
        assert_eq!(tunein.auth, Auth::Anonymous);
        assert_eq!(tunein.uri, "https://legato/x", "SecureUri wins over Uri");
        assert_eq!(tunein.manifest_uri.as_deref(), Some("https://cdn/m/tunein"));

        let ytm = &services[1];
        assert_eq!(ytm.auth, Auth::AppLink, "not linkable from a desktop");
        assert_eq!(
            ytm.uri, "https://ytm/x",
            "Uri is used when there is no SecureUri"
        );
        assert!(ytm.manifest_uri.is_none());

        // The tier that separates this from the pre-linking version: a
        // credential x2rock can actually go and get.
        assert_eq!(services[2].auth, Auth::DeviceLink);
    }

    #[test]
    fn an_empty_descriptor_list_is_an_error_not_an_empty_catalogue() {
        assert!(parse_services(r#"<Services SchemaVersion="1"/>"#, "").is_err());
    }

    #[tokio::test]
    async fn an_unlinked_service_is_refused_before_the_network() {
        let services = parse_services(DESCRIPTORS, "").unwrap();

        // A device-link service with no token is refused with the command that
        // fixes it, not with "cannot be searched".
        let err = search(&services[2], None, "all", "jazz", 0, 5)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("x2rock link Bandcamp"), "{err}");

        // An app-link service is refused with no such promise, because there is
        // nothing to run.
        let err = search(&services[1], None, "all", "jazz", 0, 5)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("cannot link"), "{err}");
        assert!(!err.contains("x2rock link"), "{err}");
    }

    #[tokio::test]
    async fn only_a_device_link_service_has_a_link_code_to_ask_for() {
        let services = parse_services(DESCRIPTORS, "").unwrap();
        for unlinkable in [&services[0], &services[1]] {
            let err = device_link_code(unlinkable, "Sonos_house")
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains("does not use device linking"), "{err}");
        }
    }

    #[test]
    fn an_anonymous_envelope_carries_no_login_token() {
        let body = envelope("search", "<id>all</id>", None);
        assert!(body.contains("<deviceProvider>Sonos</deviceProvider>"));
        assert!(!body.contains("loginToken"), "{body}");
        // The prologue that services reject.
        assert!(!body.starts_with("<?xml"));
    }

    #[test]
    fn a_token_goes_into_the_credentials_header_escaped() {
        let body = envelope(
            "search",
            "<id>all</id>",
            Some(&Token {
                token: "a&b".into(),
                key: "k<1".into(),
                household: Some("Sonos_house".into()),
            }),
        );
        assert!(
            body.contains(concat!(
                "<loginToken><token>a&amp;b</token><key>k&lt;1</key>",
                "<householdId>Sonos_house</householdId></loginToken>"
            )),
            "{body}"
        );
        // Inside the credentials header, not the body.
        let header_end = body.find("</credentials>").unwrap();
        assert!(body.find("loginToken").unwrap() < header_end);
    }

    #[test]
    fn a_token_with_no_household_still_makes_an_envelope() {
        // Searching from a cached catalogue with no player on the LAN: there is
        // nothing to look a household up from, and the call should still go out.
        let body = envelope(
            "search",
            "",
            Some(&Token {
                token: "t".into(),
                key: "k".into(),
                household: None,
            }),
        );
        assert!(body.contains("<loginToken><token>t</token><key>k</key></loginToken>"));
    }

    #[test]
    fn the_pending_fault_is_told_apart_from_a_real_one() {
        let pending = r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body>
            <s:Fault><faultcode>s:Client.NOT_LINKED_RETRY</faultcode>
            <faultstring>Link code not yet claimed</faultstring>
            <detail><ExceptionInfo>NOT_LINKED_RETRY</ExceptionInfo>
            <SonosError>5</SonosError></detail></s:Fault></s:Body></s:Envelope>"#;
        let fault = parse_fault(pending, 500);
        assert!(fault.is_pending());
        assert_eq!(fault.sonos_error, Some(5));
        assert_eq!(fault.message, "Link code not yet claimed");

        let refused = r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body>
            <s:Fault><faultcode>s:Client.LOGIN_INVALID</faultcode>
            <faultstring>Invalid credentials</faultstring>
            <detail><SonosError>9</SonosError></detail></s:Fault></s:Body></s:Envelope>"#;
        assert!(!parse_fault(refused, 500).is_pending());

        // Either signal alone is enough: services are inconsistent about which
        // half they populate, and both are documented.
        let code_only = r#"<Fault><faultcode>Client.NOT_LINKED_RETRY</faultcode>
            <faultstring>wait</faultstring></Fault>"#;
        assert!(parse_fault(code_only, 500).is_pending());
        let error_only = r#"<Fault><faultcode>Client</faultcode>
            <faultstring>wait</faultstring><detail><SonosError>5</SonosError></detail></Fault>"#;
        assert!(parse_fault(error_only, 500).is_pending());
    }

    #[test]
    fn bandcamps_real_pending_reply_is_pending() {
        // Verbatim from Bandcamp, 2026-08-31, and it differs from the docs in
        // two ways that both mattered: the faultcode is `s:NOT_LINKED_RETRY`
        // rather than the documented `Client.NOT_LINKED_RETRY`, and the whole
        // thing arrives with **HTTP 200**.
        let body = "<s:Envelope xmlns:s='http://schemas.xmlsoap.org/soap/envelope/'>\
                    <s:Body><s:Fault><faultcode>s:NOT_LINKED_RETRY</faultcode>\
                    <faultstring>Link Code not found retry...</faultstring>\
                    <detail><ExceptionInfo>NOT_LINKED_RETRY</ExceptionInfo>\
                    <SonosError>5</SonosError></detail></s:Fault></s:Body></s:Envelope>";
        let fault = fault_in(body).expect("a fault at HTTP 200 is still a fault");
        assert!(fault.is_pending());
        assert_eq!(fault.message, "Link Code not found retry...");
    }

    #[test]
    fn a_dump_can_never_print_a_token() {
        let body = envelope(
            "search",
            "<id>albums</id><term>miles</term>",
            Some(&Token {
                token: "s3cret-token".into(),
                key: "s3cret-key".into(),
                household: Some("Sonos_house".into()),
            }),
        );
        let dumped = without_credentials(&body);
        assert!(!dumped.contains("s3cret-token"), "{dumped}");
        assert!(!dumped.contains("s3cret-key"), "{dumped}");
        // The half worth reading survives.
        assert!(
            dumped.contains("<id>albums</id><term>miles</term>"),
            "{dumped}"
        );
        assert!(dumped.contains("(credentials omitted)"), "{dumped}");
    }

    #[test]
    fn a_successful_reply_is_not_mistaken_for_a_fault() {
        let body = "<s:Envelope xmlns:s='http://schemas.xmlsoap.org/soap/envelope/'>\
                    <s:Body><searchResponse xmlns='http://www.sonos.com/Services/1.1'>\
                    <searchResult><index>0</index></searchResult>\
                    </searchResponse></s:Body></s:Envelope>";
        assert!(fault_in(body).is_none());
    }

    #[test]
    fn a_fault_that_is_not_xml_still_says_something() {
        let fault = parse_fault("<html>502 Bad Gateway</html>", 502);
        assert_eq!(fault.message, "HTTP 502");
        assert!(
            !fault.is_pending(),
            "an outage is not a person still typing"
        );
    }

    #[test]
    fn a_completed_link_is_read_out_of_the_reply() {
        let body = r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body>
            <getDeviceAuthTokenResponse xmlns="http://www.sonos.com/Services/1.1">
              <getDeviceAuthTokenResult>
                <authToken>tok-123</authToken>
                <privateKey>key-456</privateKey>
                <userIdHashCode>hash-789</userIdHashCode>
              </getDeviceAuthTokenResult>
            </getDeviceAuthTokenResponse></s:Body></s:Envelope>"#;
        let auth = parse_device_auth("Bandcamp", body).unwrap();
        assert_eq!(auth.auth_token, "tok-123");
        assert_eq!(auth.private_key, "key-456");
        assert_eq!(auth.user_id_hash_code.as_deref(), Some("hash-789"));
    }

    #[test]
    fn a_reply_with_no_token_is_an_error_but_a_missing_key_is_not() {
        let no_token = r#"<getDeviceAuthTokenResult>
            <privateKey>k</privateKey></getDeviceAuthTokenResult>"#;
        assert!(parse_device_auth("Bandcamp", no_token).is_err());

        // A service that sends a token and no key is taken at its word; the
        // search that follows is the honest test of whether it was enough.
        let no_key = r#"<getDeviceAuthTokenResult>
            <authToken>t</authToken></getDeviceAuthTokenResult>"#;
        let auth = parse_device_auth("Bandcamp", no_key).unwrap();
        assert_eq!(auth.auth_token, "t");
        assert!(auth.private_key.is_empty());
        assert!(auth.user_id_hash_code.is_none());
    }

    #[test]
    fn search_terms_are_escaped_into_the_envelope() {
        assert_eq!(escape(r#"rock & <roll>"#), "rock &amp; &lt;roll&gt;");
    }
}
