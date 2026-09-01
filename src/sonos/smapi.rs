//! SMAPI: searching a music service directly, the way a player does.
//!
//! SMAPI is SOAP 1.1 over HTTPS in the namespace `http://www.sonos.com/Services/1.1`.
//! Sonos is the *client* and the music service is the server, so a controller that
//! wants search does not ask a player for it - it calls the service itself.
//!
//! **This is the only part of x2rock that leaves the LAN**, and it is confined to
//! the CLI on purpose - `search`, `browse` and `link` all come through here, and
//! nothing in the daemon does. See "Rule: talking to a service never enters the
//! daemon" in docs/architecture.md: the daemon publishes MPRIS and must never
//! acquire an internet timeout in front of play/pause.
//!
//! Two kinds of service can be searched. `Policy Auth="Anonymous"` needs nothing
//! but a `deviceProvider`, which is about a third of the catalogue and most of
//! the radio-shaped half of it. `DeviceLink` services need a `loginToken`, and
//! this module can now *mint* one: `getDeviceLinkCode` and `getDeviceAuthToken`
//! are the device-link flow, driven by the controller, with the browser step
//! handed to whatever browser the person already uses.
//!
//! `AppLink` is the tier that mostly stays out of reach. It expects the Sonos
//! app to launch the service's own mobile app, and there is no desktop app to
//! hand off to - but `getAppLink` nests the same browser link a device link
//! uses, so `x2rock link` asks anyway and lets the service answer. Some never
//! will: YouTube Music gates the endpoint on an API key before user auth is
//! even reached, and Plex's SMAPI link half is dead - Plex links through its
//! own published PIN flow instead, in [`super::plex`].

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
    /// Links through the service's own app. Often, but not always, a dead end
    /// here: the reply can still carry a browser page. Asking is how you know.
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

/// What `getDeviceLinkCode` or `getAppLink` returns: where to send the person,
/// and the code.
#[derive(Debug, Clone)]
pub struct LinkCode {
    /// The page to open. On Linux that is `xdg-open` and nothing else.
    pub reg_url: String,
    pub link_code: String,
    /// Whether the person has to *type* the code. False when it is already a
    /// query parameter of `reg_url`, which is the graceful case: open a link,
    /// log in, done.
    pub show_link_code: bool,
    /// Sent back verbatim in `getDeviceAuthToken` when the service handed one
    /// out. Only app-link replies have been seen to carry it.
    pub link_device_id: Option<String>,
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

    /// What to say when this service is asked for without a stored token.
    ///
    /// One sentence of what the service wants, one of what to do about it.
    /// Shared so the search path and the SOAP path cannot drift apart, which
    /// they had: two wordings of the same advice, in two files.
    ///
    /// App-link is deliberately not described as a phone-only flow. The
    /// hand-off target is the *controller's* choice - Sonos's own desktop
    /// controller does app-link without a mobile app anywhere - so the reply
    /// nests a browser page often enough to be worth asking for. Which
    /// services populate it is learned by asking; see `app_link_code`.
    ///
    /// Nor does it claim the service *said* app-link. `parse_services` sends
    /// every unrecognised or missing `Policy Auth` to `AppLink` on purpose,
    /// so this arm also covers a tier nobody here has seen - SMAPI's own
    /// username/password one among them. It says what x2rock knows (no code
    /// flow it can drive) rather than a mechanism it would be guessing at.
    ///
    /// Callers guard on `auth != Anonymous` before asking, because an
    /// anonymous service needs nothing and this is phrased as a refusal. The
    /// arm exists to keep the match total; a caller that reaches it has
    /// already asked the wrong question.
    pub fn needs_link(&self) -> String {
        match self.auth {
            Auth::Anonymous => format!("{} needs no account.", self.name),
            Auth::DeviceLink => format!(
                "{} needs a linked account. Run `x2rock link {}` once and it \
                 will work from then on.",
                self.name, self.name
            ),
            Auth::AppLink => format!(
                "{} needs a linked account, and offers no code flow x2rock \
                 can drive. Some services in this tier answer with a browser \
                 page anyway: `x2rock link {}` asks, and a refusal costs \
                 nothing.",
                self.name, self.name
            ),
        }
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
    /// A `mediaCollection` rather than a `mediaMetadata`: something to descend
    /// into rather than something to play.
    ///
    /// **`canPlay` is deliberately not what decides this.** iHeartRadio marks an
    /// `artist_radio` collection `canPlay`, and handing its id to `getMediaURI`
    /// is refused with the accepted grammar spelled out - `artist_radio_track`,
    /// `live_stations.`, `podcast_show`. So `canPlay` means "this collection can
    /// be played" in some sense the service understands, not "this id resolves",
    /// and the reliable question is which element the item arrived in.
    pub container: bool,
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
    parse_items(&body, "search")
}

/// `getMetadata`: what a container holds.
///
/// The other half of reaching a linked service, and the only way to reach the
/// parts of one that a search term cannot name - a personal library, a
/// "For You", a genre tree. `root` is where every service starts.
pub async fn metadata(
    service: &Service,
    token: Option<&Token>,
    id: &str,
    index: u32,
    count: u32,
) -> Result<(Vec<Item>, u32)> {
    let body = call(
        service,
        token,
        "getMetadata",
        &format!(
            "<id>{}</id><index>{index}</index><count>{count}</count>",
            escape(id)
        ),
    )
    .await?;
    parse_items(&body, "getMetadata")
}

/// The items in a `search` or `getMetadata` reply, and the total it claims.
///
/// One parser for both because the payload is the same: `mediaCollection` for
/// something to descend into, `mediaMetadata` for something to play, and the
/// services mix them freely in either call.
fn parse_items(body: &str, what: &str) -> Result<(Vec<Item>, u32)> {
    let doc = Document::parse(body).with_context(|| format!("parsing {what} response"))?;
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
            let item_type = child("itemType").unwrap_or_default();
            // The element decides - except when the declared type is itself a
            // playable leaf. Plex answers a *tracks* search with
            // `mediaCollection` elements whose `itemType` is `track`, and the
            // very id inside plays through the enqueue path (it is what the
            // household's own Plex playback reports as its object id). A type
            // that names a leaf outranks the wrapping, and only those two do:
            // `canPlay` stays untrusted for the reasons documented on `Item`.
            let container = n.has_tag_name("mediaCollection")
                && !matches!(item_type.as_str(), "track" | "stream");
            Some(Item {
                id: child("id")?,
                title: child("title").unwrap_or_default(),
                item_type,
                // Whatever the service offers as a second line; services differ
                // on which of these they populate, and most populate one.
                summary: child("summary")
                    .or_else(|| child("artist"))
                    .or_else(|| child("genre"))
                    .or_else(|| child("country")),
                art_url: child("albumArtURI").or_else(|| nested("streamMetadata", "logo")),
                container,
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
    parse_link_code(&service.name, "getDeviceLinkCode", &body)
}

/// Ask an app-link service where to send the person.
///
/// `getAppLink` is the newer flow, named for handing off to the service's own
/// app - but that is the controller's choice, not a requirement of the tier.
/// Sonos's own desktop controller does app-link with no mobile app anywhere,
/// so the reply nests a `deviceLink` (the same `regUrl`/`linkCode` pair) for
/// controllers with nothing to hand off to, and some services populate it
/// with a real browser page. Whether a given service does is learned by
/// asking: Plex answers, YouTube Music refuses the call outright wanting an
/// API key it seals beyond reach. A refusal costs nothing, so the CLI asks
/// rather than presuming.
pub async fn app_link_code(service: &Service, household: &str) -> Result<LinkCode> {
    ensure!(
        service.auth == Auth::AppLink,
        "{} does not use app linking ({:?}), so there is no app link to ask for",
        service.name,
        service.auth
    );
    // Unlike `getDeviceLinkCode`, this call wants a `loginToken` in the
    // credentials header even though there is no token yet: an *empty* token
    // and key alongside the household id. Leaving the block out entirely reads
    // to Plex as an expired credential (`Client.AuthTokenExpired`) rather than
    // a missing one.
    let empty = Token {
        token: String::new(),
        key: String::new(),
        household: Some(household.to_string()),
    };
    let body = match call_soap(
        service,
        Some(&empty),
        "getAppLink",
        &format!("<householdId>{}</householdId>", escape(household)),
    )
    .await?
    {
        Ok(body) => body,
        Err(fault) => bail!("{} refused getAppLink: {}", service.name, fault.message),
    };
    parse_link_code(&service.name, "getAppLink", &body)
}

/// The two link-code replies differ only in nesting - `getAppLink` wraps the
/// same fields in `authorizeAccount/deviceLink` - so one descendant scan reads
/// both.
fn parse_link_code(service_name: &str, action: &str, body: &str) -> Result<LinkCode> {
    let doc = Document::parse(body).with_context(|| format!("parsing {action} response"))?;
    let field = |tag: &str| {
        doc.descendants()
            .find(|n| n.has_tag_name(tag))
            .and_then(|n| n.text())
            .map(str::to_string)
    };
    let link_code =
        field("linkCode").ok_or_else(|| anyhow!("{service_name} returned no linkCode"))?;
    Ok(LinkCode {
        reg_url: field("regUrl")
            .ok_or_else(|| anyhow!("{service_name} returned no regUrl to open"))?,
        link_code,
        // Absent means the code is not needed on screen. Defaulting to false
        // rather than true because the failure it causes is cosmetic, whereas
        // printing a code the person cannot use is confusing.
        show_link_code: field("showLinkCode").is_some_and(|v| v.trim() == "true"),
        link_device_id: field("linkDeviceId"),
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
    link_device_id: Option<&str>,
) -> Result<Option<DeviceAuth>> {
    // `linkDeviceId` is echoed back only when the link-code reply carried one;
    // nothing device-linked so far has, and sending an empty element to a
    // service that never asked is a way to find new failure modes.
    let device = link_device_id
        .map(|d| format!("<linkDeviceId>{}</linkDeviceId>", escape(d)))
        .unwrap_or_default();
    let body = match call_soap(
        service,
        None,
        "getDeviceAuthToken",
        &format!(
            "<householdId>{}</householdId><linkCode>{}</linkCode>{device}",
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
    // An empty 200 is not a reply. Deezer answers `getDeviceLinkCode` with
    // exactly that, and letting it fall through to the XML reader reported
    // "parsing getDeviceLinkCode response" - blaming the parser for a service
    // that said nothing at all.
    if text.trim().is_empty() {
        return Ok(Err(Fault {
            code: String::new(),
            message: format!("answered HTTP {status} with an empty body"),
            sonos_error: None,
        }));
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
    // SMAPI is specified as SOAP 1.1, and services mostly oblige - but Sonos
    // Radio answers in **1.2**, where a fault has no `faultcode` or
    // `faultstring` at all: the code is `Code/Value` plus an optional
    // `Subcode/Value`, and the message is `Reason/Text`. Reading only the 1.1
    // names turned "TypeError: method is not a function" - a straight answer
    // about a service that is broken - into "a fault with no faultstring".
    let soap12_code = || {
        let values: Vec<String> = doc
            .descendants()
            .filter(|n| n.has_tag_name("Code"))
            .flat_map(|code| {
                code.descendants()
                    .filter(|n| n.has_tag_name("Value"))
                    .filter_map(|n| n.text())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .collect();
        (!values.is_empty()).then(|| values.join(" "))
    };
    let soap12_reason = || {
        doc.descendants()
            .find(|n| n.has_tag_name("Reason"))
            .and_then(|r| r.descendants().find(|n| n.has_tag_name("Text")))
            .and_then(|n| n.text())
            .map(str::to_string)
    };
    Some(Fault {
        // Every code value joined rather than just the first, so a pending
        // check works whichever half of a 1.2 code carries the word.
        code: field("faultcode").or_else(soap12_code).unwrap_or_default(),
        message: field("faultstring")
            .or_else(soap12_reason)
            .unwrap_or_else(|| "a fault with no message".to_string()),
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
        bail!("{}", service.needs_link());
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
    fn each_auth_tier_gets_the_advice_that_fits_it() {
        let services = parse_services(DESCRIPTORS, "").unwrap();
        let by_name = |n: &str| {
            services
                .iter()
                .find(|s| s.name == n)
                .unwrap_or_else(|| panic!("{n} missing from DESCRIPTORS"))
                .needs_link()
        };

        // Device link is a promise: x2rock can finish this one unaided.
        let bandcamp = by_name("Bandcamp");
        assert!(bandcamp.contains("`x2rock link Bandcamp`"));
        assert!(bandcamp.contains("will work from then on"));

        // App link is an invitation, not a promise - the service decides, and
        // the wording must not claim more than asking can deliver.
        let ytm = by_name("YouTube Music");
        assert!(ytm.contains("`x2rock link YouTube Music`"));
        assert!(ytm.contains("a refusal costs nothing"));
        assert!(
            !ytm.contains("will work"),
            "app-link advice must not promise a link it cannot make: {ytm}"
        );

        // Every tier that needs an account says so and names the one command
        // that might supply it; the anonymous tier says neither.
        for name in ["Bandcamp", "YouTube Music"] {
            assert!(by_name(name).contains("needs a linked account"));
        }
        let tunein = by_name("TuneIn");
        assert!(!tunein.contains("needs a linked account"));
        assert!(!tunein.contains("x2rock link"));
    }

    #[test]
    fn an_unrecognised_tier_is_not_told_it_links_through_an_app() {
        // parse_services sends anything it does not recognise to AppLink on
        // purpose. SMAPI's username/password tier lands there without ever
        // having claimed to be an app-link service, so the advice must not
        // describe a mechanism the descriptor never named.
        let services = parse_services(
            r#"<Services><Service Id="777" Name="Legacy" Uri="https://legacy/smapi">
                 <Policy Auth="UserId"/></Service></Services>"#,
            "",
        )
        .unwrap();
        let legacy = &services[0];
        assert_eq!(legacy.auth, Auth::AppLink, "the conservative fallback");

        let advice = legacy.needs_link();
        assert!(advice.contains("needs a linked account"));
        assert!(advice.contains("`x2rock link Legacy`"));
        assert!(
            !advice.contains("its own app"),
            "must not assert a mechanism the service never declared: {advice}"
        );
    }

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

        // An app-link service is refused with the same suggestion, hedged:
        // `x2rock link` now asks such a service for a browser page, and whether
        // one comes back is the service's answer to give.
        let err = search(&services[1], None, "all", "jazz", 0, 5)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("x2rock link"), "{err}");
        assert!(err.contains("refusal costs nothing"), "{err}");
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

    #[tokio::test]
    async fn only_an_app_link_service_has_an_app_link_to_ask_for() {
        let services = parse_services(DESCRIPTORS, "").unwrap();
        for unlinkable in [&services[0], &services[2]] {
            let err = app_link_code(unlinkable, "Sonos_house")
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains("does not use app linking"), "{err}");
        }
    }

    #[test]
    fn an_app_link_reply_is_read_through_its_nesting() {
        // The shape `getAppLink` documents: the same fields as a device link,
        // wrapped in `authorizeAccount/deviceLink`, plus a `linkDeviceId` that
        // must ride along into `getDeviceAuthToken`.
        let body = r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body>
            <getAppLinkResponse xmlns="http://www.sonos.com/Services/1.1">
              <getAppLinkResult><authorizeAccount>
                <appUrlStringId>PLEX_LINK</appUrlStringId>
                <deviceLink>
                  <regUrl>https://plex.tv/link?code=ABCD</regUrl>
                  <linkCode>ABCD</linkCode>
                  <showLinkCode>false</showLinkCode>
                  <linkDeviceId>dev-1</linkDeviceId>
                </deviceLink>
              </authorizeAccount></getAppLinkResult>
            </getAppLinkResponse></s:Body></s:Envelope>"#;
        let code = parse_link_code("Plex", "getAppLink", body).unwrap();
        assert_eq!(code.reg_url, "https://plex.tv/link?code=ABCD");
        assert_eq!(code.link_code, "ABCD");
        assert!(!code.show_link_code);
        assert_eq!(code.link_device_id.as_deref(), Some("dev-1"));
    }

    #[test]
    fn a_collection_declaring_itself_a_track_is_not_a_container() {
        // Verbatim shape from Plex, 2026-08-31: a *tracks* search answered in
        // `mediaCollection` elements whose `itemType` is `track`. The id inside
        // is playable - it is what the household's own Plex playback reports -
        // so the element wrapping must not outrank the declared leaf type.
        let body = r#"<soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/"><soap:Body>
            <searchResponse><searchResult><index>0</index><count>2</count><total>2</total>
              <mediaCollection>
                <id>c69ee188::68163:track</id><itemType>track</itemType>
                <title>Señorita</title><summary>Various Artists on Prime</summary>
                <canPlay>true</canPlay>
              </mediaCollection>
              <mediaCollection>
                <id>c69ee188::68162:album</id><itemType>album</itemType>
                <title>Now 103</title><canPlay>true</canPlay>
              </mediaCollection>
            </searchResult></searchResponse></soap:Body></soap:Envelope>"#;
        let (items, total) = parse_items(body, "search").unwrap();
        assert_eq!(total, 2);
        assert!(!items[0].container, "a track hit must be playable");
        assert!(items[1].container, "an album stays a place to open");
    }

    #[test]
    fn a_device_link_reply_still_parses_flat() {
        let body = r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body>
            <getDeviceLinkCodeResponse xmlns="http://www.sonos.com/Services/1.1">
              <getDeviceLinkCodeResult>
                <regUrl>https://bandcamp.com/login?sonos_link_code=7083</regUrl>
                <linkCode>7083</linkCode>
                <showLinkCode>false</showLinkCode>
              </getDeviceLinkCodeResult>
            </getDeviceLinkCodeResponse></s:Body></s:Envelope>"#;
        let code = parse_link_code("Bandcamp", "getDeviceLinkCode", body).unwrap();
        assert_eq!(code.link_code, "7083");
        assert_eq!(code.link_device_id, None);
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
    fn a_soap_12_fault_is_read_as_well_as_an_11_one() {
        // Verbatim from Sonos Radio, 2026-08-31. SMAPI says SOAP 1.1 and this
        // is 1.2, so there is no faultcode and no faultstring to find.
        let body = r#"<?xml version="1.0" encoding="utf-8"?>
            <soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/"
                           xmlns:tns="http://www.sonos.com/Services/1.1"><soap:Body>
            <soap:Fault><soap:Code><soap:Value>SOAP-ENV:Server</soap:Value>
            <soap:Subcode><soap:Value>InternalServerError</soap:Value></soap:Subcode>
            </soap:Code>
            <soap:Reason><soap:Text>TypeError: method is not a function</soap:Text></soap:Reason>
            </soap:Fault></soap:Body></soap:Envelope>"#;
        let fault = fault_in(body).expect("a 1.2 fault is still a fault");
        assert_eq!(fault.message, "TypeError: method is not a function");
        assert_eq!(fault.code, "SOAP-ENV:Server InternalServerError");
        assert!(
            !fault.is_pending(),
            "a crashing service is not a person typing"
        );
    }

    #[test]
    fn a_pending_fault_in_soap_12_shape_is_still_pending() {
        // No service has sent one, but the 1.1 assumption already cost a
        // readable error once; a device link is the worst place to repeat it.
        let body = r#"<soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/">
            <soap:Body><soap:Fault><soap:Code><soap:Value>soap:Sender</soap:Value>
            <soap:Subcode><soap:Value>NOT_LINKED_RETRY</soap:Value></soap:Subcode></soap:Code>
            <soap:Reason><soap:Text>keep waiting</soap:Text></soap:Reason>
            </soap:Fault></soap:Body></soap:Envelope>"#;
        assert!(fault_in(body).unwrap().is_pending());
    }

    #[test]
    fn an_empty_body_is_not_a_fault_shape_but_it_is_a_failure() {
        // fault_in has nothing to find in it, which is why the emptiness has to
        // be checked before the parser is asked.
        assert!(fault_in("").is_none());
        assert!(fault_in("   \n ").is_none());
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
