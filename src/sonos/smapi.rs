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
//! Only services whose `Policy Auth` is `Anonymous` are supported, which is about
//! a third of the catalogue and most of the radio-shaped half of it. The rest need
//! the household's `loginToken`, and nothing documented reads one back.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use roxmltree::Document;
use serde::{Deserialize, Serialize};

use super::http;

/// Short, and deliberately shorter than the LAN's. Search is invoked from a bar
/// widget as a subprocess; a call that never returns is worse than one that fails.
pub const TIMEOUT: Duration = Duration::from_secs(6);

const NS: &str = "http://www.sonos.com/Services/1.1";

/// How a service expects to be authenticated, from its `<Policy Auth=...>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Auth {
    /// No credential at all beyond `deviceProvider`. The ones we can use.
    Anonymous,
    /// Needs the household's `loginToken`, which we have no way to read.
    Linked,
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
        let auth = match policy.and_then(|p| p.attribute("Auth")) {
            Some("Anonymous") => Auth::Anonymous,
            _ => Auth::Linked,
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
    category: &str,
    term: &str,
    index: u32,
    count: u32,
) -> Result<(Vec<Item>, u32)> {
    let body = call(
        service,
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
pub async fn media_uri(service: &Service, id: &str) -> Result<String> {
    let body = call(service, "getMediaURI", &format!("<id>{}</id>", escape(id))).await?;
    let doc = Document::parse(&body).context("parsing getMediaURI response")?;
    doc.descendants()
        .find(|n| n.has_tag_name("getMediaURIResult"))
        .and_then(|n| n.text())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("{} returned no media URI for {id}", service.name))
}

/// One SMAPI call. Anonymous services need nothing in the credentials header but
/// `deviceProvider`; anything else is refused here rather than sent and rejected.
async fn call(service: &Service, action: &str, params: &str) -> Result<String> {
    if service.auth != Auth::Anonymous {
        bail!(
            "{} needs a linked account, which x2rock cannot supply. \
             Only services with anonymous access can be searched.",
            service.name
        );
    }
    let (endpoint, path, tls) = http::parse_url(&service.uri)?;
    // No XML declaration and no BOM: services answer one with
    // `s:Client / Expecting state 'Element'`, which reads like a malformed
    // request and is really just the prologue.
    let envelope = format!(
        concat!(
            r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">"#,
            r#"<s:Header><credentials xmlns="{ns}">"#,
            r#"<deviceProvider>Sonos</deviceProvider>"#,
            r#"</credentials></s:Header>"#,
            r#"<s:Body><{action} xmlns="{ns}">{params}</{action}></s:Body></s:Envelope>"#
        ),
        ns = NS,
        action = action,
        params = params
    );
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

    if status != 200 {
        // A SOAP fault carries the useful half of the story; the status alone
        // is almost always a bare 500.
        let reason = Document::parse(&text)
            .ok()
            .and_then(|d| {
                d.descendants()
                    .find(|n| n.has_tag_name("faultstring"))
                    .and_then(|n| n.text())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| format!("HTTP {status}"));
        bail!("{} refused {action}: {reason}", service.name);
    }
    Ok(text)
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
        assert_eq!(services.len(), 2);

        let tunein = &services[0];
        assert_eq!(tunein.auth, Auth::Anonymous);
        assert_eq!(tunein.uri, "https://legato/x", "SecureUri wins over Uri");
        assert_eq!(tunein.manifest_uri.as_deref(), Some("https://cdn/m/tunein"));

        let ytm = &services[1];
        assert_eq!(ytm.auth, Auth::Linked, "AppLink is not usable");
        assert_eq!(
            ytm.uri, "https://ytm/x",
            "Uri is used when there is no SecureUri"
        );
        assert!(ytm.manifest_uri.is_none());
    }

    #[test]
    fn an_empty_descriptor_list_is_an_error_not_an_empty_catalogue() {
        assert!(parse_services(r#"<Services SchemaVersion="1"/>"#, "").is_err());
    }

    #[tokio::test]
    async fn a_linked_service_is_refused_before_the_network() {
        let ytm = &parse_services(DESCRIPTORS, "").unwrap()[1];
        let err = search(ytm, "all", "jazz", 0, 5)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("linked account"), "{err}");
    }

    #[test]
    fn search_terms_are_escaped_into_the_envelope() {
        assert_eq!(escape(r#"rock & <roll>"#), "rock &amp; &lt;roll&gt;");
    }
}
