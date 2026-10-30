//! Music services: which this household can use, one search across all of
//! them, a service's pages, and playing what they list. On x2rock's own
//! catalogue and credentials (the CLI's: one store per machine, so a service
//! linked with `x2rock link` is linked here too), and its play routes.
//!
//! Opened once a session and shared by the runtime's tasks, behind a lock that
//! is held only to read what a request needs and to write back what it learned,
//! never across the request (see [`Open`]).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use futures_util::StreamExt;
use x2rock::catalogue::Catalogue;
use x2rock::credentials::Credentials;
use x2rock::session::Session;
use x2rock::sonos::smapi::{self, Category, Item, Service};
use x2rock::sonos::upnp::Upnp;

/// The order categories are worth asking for, most wanted first (the TV app's
/// SearchViewModel).
const PREFERRED: [&str; 8] = [
    "tracks",
    "albums",
    "artists",
    "playlists",
    "stations",
    "live_stations",
    "podcasts",
    "audiobooks",
];
/// Each category's results, and the most one service shows.
pub const PER_CATEGORY: u32 = 6;
const SERVICE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CONCURRENT: usize = 8;

/// A service as the screens list it.
#[derive(Clone, Debug)]
pub struct ServiceInfo {
    pub id: String,
    pub name: String,
    /// Has an account here, rather than answering anyone.
    pub linked: bool,
}

/// One service's answer to a search: its results, or why there are none.
#[derive(Clone, Debug)]
pub struct Section {
    pub service: ServiceInfo,
    pub result: Result<Vec<Item>, String>,
}

pub struct Services {
    catalogue: Catalogue,
    linked: Credentials,
    household: String,
    /// The last read of the household's stored sign-ins: how many were kept,
    /// or why none could be. For Send a report, as the TV app's beta report says it.
    last_read: Option<Result<usize, String>>,
}

/// Two categories per service: its two most wanted, or whatever it has first.
/// Two, because every one more is another request to every service.
pub fn pick_categories(available: &[Category]) -> Vec<Category> {
    let preferred: Vec<Category> = PREFERRED
        .iter()
        .filter_map(|id| available.iter().find(|c| c.id == *id).cloned())
        .collect();
    let chosen = if preferred.is_empty() {
        available.to_vec()
    } else {
        preferred
    };
    chosen.into_iter().take(2).collect()
}

/// What one request to a service needs, taken under the lock and used after it.
struct Ask {
    service: Service,
    token: Option<smapi::Token>,
}

impl Services {
    /// The catalogue as the speakers list it now, and this machine's accounts.
    /// With none for this household yet and a speaker salt to read them with,
    /// the household's own sign-ins are read first, as the TV app reads them.
    async fn open(session: &Session) -> Result<Self> {
        let household = session.connection.household_id().await?;
        let mut catalogue = Catalogue::load();
        catalogue
            .refresh_and_save(&Upnp::new(session.connection.ip()), false)
            .await?;
        let linked = Credentials::load()?;
        let mut services = Services {
            catalogue,
            linked,
            household,
            last_read: None,
        };
        let none_here = services
            .catalogue
            .services()
            .iter()
            .all(|s| services.linked.get(&services.household, &s.id).is_none());
        if none_here && x2rock::salt::require().is_ok() {
            // Not shared yet: nothing else waits on this one.
            let (last_read, imported) =
                import(session, &services.catalogue, &services.household).await;
            services.last_read = Some(last_read);
            match imported {
                Ok(_) => services.linked = Credentials::load()?,
                Err(e) => eprintln!("x2rock-deck: the household's sign-ins were not read ({e:#})"),
            }
        }
        Ok(services)
    }

    fn info(&self, s: &Service) -> ServiceInfo {
        ServiceInfo {
            id: s.id.clone(),
            name: s.name.clone(),
            linked: self.linked.get(&self.household, &s.id).is_some(),
        }
    }

    fn ask(&self, service_id: &str) -> Result<Ask> {
        let service = self
            .catalogue
            .by_id(service_id)
            .cloned()
            .ok_or_else(|| anyhow!("that service is no longer listed"))?;
        let token = self.linked.token_for(&self.household, service_id);
        Ok(Ask { service, token })
    }
}

/// Read the household's stored sign-ins off a speaker into this machine's
/// store, from a catalogue and credentials of its own: how the read went, for
/// the report, and the names of the services kept.
async fn import(
    session: &Session,
    catalogue: &Catalogue,
    household: &str,
) -> (Result<usize, String>, Result<Vec<String>>) {
    let read = async {
        let mut linked = Credentials::load()?;
        x2rock::import::household_accounts(
            session,
            catalogue,
            &mut linked,
            household,
            None,
            None,
            0,
        )
        .await
    };
    match read.await {
        Ok(kept) => {
            let mut names: Vec<String> = kept.iter().map(|(name, _)| name.clone()).collect();
            names.dedup();
            (Ok(kept.len()), Ok(names))
        }
        Err(e) => (Err(format!("{e:#}")), Err(e)),
    }
}

/// The services, opened. The lock is held only to read what a request needs
/// and to write back what it learned, never across a request: one service
/// that does not answer must not hold up playing what another has listed.
pub struct Open(std::sync::Mutex<Services>);

impl Open {
    fn lock(&self) -> std::sync::MutexGuard<'_, Services> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn ask(&self, service_id: &str) -> Result<Ask> {
        self.lock().ask(service_id)
    }

    /// The accounts as stored now. x2rock writes a refreshed token to the
    /// store and not to the copy held here, so after anything that may have
    /// refreshed one the copy is read again, or the next request would send
    /// the token the service has just replaced.
    fn follow_store(&self) {
        if let Ok(linked) = Credentials::load() {
            self.lock().linked = linked;
        }
    }

    /// Every service this household can browse: linked ones first, then those
    /// that answer anyone, each alphabetically.
    pub fn usable(&self) -> Vec<ServiceInfo> {
        let s = self.lock();
        let mut list: Vec<ServiceInfo> = s
            .catalogue
            .usable(&s.linked, &s.household)
            .into_iter()
            .map(|service| s.info(service))
            .collect();
        list.sort_by_key(|s| (!s.linked, s.name.to_lowercase()));
        list
    }

    /// How the last read of the household's sign-ins went, if one was made.
    pub fn last_read(&self) -> Option<Result<usize, String>> {
        self.lock().last_read.clone()
    }

    /// Read the household's stored sign-ins off a speaker into this machine's
    /// store: the names of the services kept.
    pub async fn import(&self, session: &Session) -> Result<Vec<String>> {
        // The catalogue as saved: the one held here is not to be lent across a read.
        let household = self.lock().household.clone();
        let (last_read, imported) = import(session, &Catalogue::load(), &household).await;
        self.lock().last_read = Some(last_read);
        let names = imported?;
        self.follow_store();
        Ok(names)
    }

    /// The searchable services' categories, learning any not yet asked. A
    /// failure is left unrecorded: recording it would drop the service from
    /// search for good.
    async fn warm_categories(&self) {
        let cold: Vec<Service> = {
            let s = self.lock();
            s.catalogue
                .searchable(&s.linked, &s.household)
                .into_iter()
                .filter(|service| !s.catalogue.categories_cached(&service.id))
                .cloned()
                .collect()
        };
        if cold.is_empty() {
            return;
        }
        let learned = futures_util::future::join_all(cold.iter().map(|s| async move {
            (
                s.id.clone(),
                tokio::time::timeout(SERVICE_TIMEOUT, smapi::categories(s)).await,
            )
        }))
        .await;
        let mut s = self.lock();
        let mut any = false;
        for (id, got) in learned {
            if let Ok(Ok(categories)) = got {
                s.catalogue.remember_categories(&id, categories);
                any = true;
            }
        }
        if any {
            let _ = s.catalogue.save();
        }
    }

    /// One search across every searchable service, each section handed to
    /// `answered` as its service answers, at most eight at once.
    pub async fn search(&self, term: &str, answered: impl Fn(Section)) {
        self.warm_categories().await;
        let plan: Vec<(ServiceInfo, Service, Option<smapi::Token>, Vec<Category>)> = {
            let s = self.lock();
            s.catalogue
                .searchable(&s.linked, &s.household)
                .into_iter()
                .filter_map(|service| {
                    let categories = pick_categories(s.catalogue.cached_categories(&service.id)?);
                    (!categories.is_empty()).then(|| {
                        (
                            s.info(service),
                            service.clone(),
                            s.linked.token_for(&s.household, &service.id),
                            categories,
                        )
                    })
                })
                .collect()
        };
        let term = term.to_owned();
        let mut answers = futures_util::stream::iter(plan.into_iter().map(
            |(info, service, token, categories)| {
                let term = term.clone();
                async move {
                    let asked = async {
                        let mut items = Vec::new();
                        let mut token = token;
                        for category in &categories {
                            let mut refreshed = None;
                            let got = smapi::search(
                                &service,
                                token.as_ref(),
                                &category.mapped_id,
                                &term,
                                0,
                                PER_CATEGORY,
                                &mut refreshed,
                            )
                            .await;
                            if refreshed.is_some() {
                                token = x2rock::play::after_refresh(
                                    &service.id,
                                    token.as_ref(),
                                    refreshed,
                                );
                                // Now, before the next category: a search ended while
                                // that one is asked must not leave the old token held.
                                self.follow_store();
                            }
                            items.extend(got?.0);
                        }
                        Ok::<_, anyhow::Error>(items)
                    };
                    let result = match tokio::time::timeout(SERVICE_TIMEOUT, asked).await {
                        Ok(Ok(items)) => Ok(items),
                        Ok(Err(e)) => Err(crate::household::plain(&e)),
                        Err(_) => Err("didn't answer".to_owned()),
                    };
                    Section {
                        service: info,
                        result,
                    }
                }
            },
        ))
        .buffer_unordered(MAX_CONCURRENT);
        while let Some(section) = answers.next().await {
            answered(section);
        }
    }

    /// A page of a service: `root` is its front page, where it publishes one.
    pub async fn browse(
        &self,
        service_id: &str,
        container: &str,
        index: u32,
        count: u32,
    ) -> Result<(Vec<Item>, u32)> {
        let Ask { service, token } = self.ask(service_id)?;
        let mut refreshed = None;
        let page = smapi::metadata(
            &service,
            token.as_ref(),
            container,
            index,
            count,
            &mut refreshed,
        )
        .await;
        if refreshed.is_some() {
            x2rock::play::after_refresh(&service.id, token.as_ref(), refreshed);
            self.follow_store();
        }
        page
    }

    /// Add an item to a room's queue: next after what plays, or at the end.
    pub async fn enqueue(
        &self,
        session: &Session,
        room: &str,
        service_id: &str,
        item: &Item,
        next: bool,
    ) -> Result<()> {
        let Ask { service, token } = self.ask(service_id)?;
        let naming = x2rock::play::Naming::of(&service, token.as_ref())
            .ok_or_else(|| anyhow!("{} has nothing here to name an account with", service.name))?;
        let how = if next {
            x2rock::play::Enqueue::Next
        } else {
            x2rock::play::Enqueue::End
        };
        let done = x2rock::play::enqueue_item(
            session,
            Some(room),
            &service,
            &naming,
            &item.id,
            &item.title,
            Some(&item.item_type),
            how,
        )
        .await;
        // x2rock's play routes refresh and store a token without saying so.
        self.follow_store();
        done
    }

    /// The URI and DIDL a queue would be given for a song, an album or a
    /// playlist, which is also what makes a Sonos favourite of one.
    fn source(ask: &Ask, item: &Item) -> Result<(String, String)> {
        use x2rock::bookmarks;
        let Ask { service, token } = ask;
        let naming = x2rock::play::Naming::of(service, token.as_ref())
            .ok_or_else(|| anyhow!("{} has nothing here to name an account with", service.name))?;
        let (cdudn, sn) = (naming.cdudn.as_str(), naming.serial.as_deref());
        let kind = item.item_type.as_str();
        let (uri, didl) = if bookmarks::container_holds_tracks(kind) {
            (
                bookmarks::container_uri(&item.id, &service.id, sn),
                bookmarks::container_didl(&item.id, &item.title, kind, cdudn),
            )
        } else if kind.eq_ignore_ascii_case("track") {
            (
                bookmarks::service_uri(&item.id, &service.id, sn),
                bookmarks::service_didl(&item.id, &item.title, cdudn),
            )
        } else {
            anyhow::bail!("Only a song, an album or a playlist can be a Sonos favorite from here");
        };
        Ok((uri, didl))
    }

    /// The household's Sonos favourites that are this item: by the URI a save
    /// gives it, or by the service's own id inside, which finds one the Sonos
    /// app saved as a shortcut, with no URI at all.
    fn favorites_of<'a>(
        favorites: &'a [x2rock::sonos::upnp::BrowseItem],
        service_id: &str,
        item: &Item,
        uri: Option<&str>,
    ) -> Vec<&'a x2rock::sonos::upnp::BrowseItem> {
        favorites
            .iter()
            .filter(|f| {
                (uri.is_some() && f.uri.as_deref() == uri)
                    || f.service_object()
                        .is_some_and(|(s, o)| s == service_id && o == item.id)
            })
            .collect()
    }

    /// Whether the item is among the household's Sonos favourites.
    pub async fn is_favorite(&self, upnp: &Upnp, service_id: &str, item: &Item) -> Result<bool> {
        let uri = self
            .ask(service_id)
            .and_then(|ask| Self::source(&ask, item))
            .ok()
            .map(|(uri, _)| uri);
        let favorites = upnp.browse_content("FV:2").await?;
        Ok(!Self::favorites_of(&favorites, service_id, item, uri.as_deref()).is_empty())
    }

    /// Save the item to the household's Sonos favourites, unless it is one
    /// already: false then. A duplicate is never made.
    pub async fn add_favorite(&self, upnp: &Upnp, service_id: &str, item: &Item) -> Result<bool> {
        let ask = self.ask(service_id)?;
        let (uri, didl) = Self::source(&ask, item)?;
        let favorites = upnp.browse_content("FV:2").await?;
        if !Self::favorites_of(&favorites, service_id, item, Some(&uri)).is_empty() {
            return Ok(false);
        }
        let saved = upnp
            .create_favorite(&x2rock::sonos::upnp::NewFavorite {
                title: &item.title,
                uri: &uri,
                metadata: &didl,
                art_url: item.art_url.as_deref(),
                service_name: Some(&ask.service.name),
            })
            .await?;
        // 803: a copy under another spelling of its URI is there already.
        Ok(saved.is_some())
    }

    /// Take every Sonos favourite that is this item out: how many went.
    pub async fn remove_favorite(
        &self,
        upnp: &Upnp,
        service_id: &str,
        item: &Item,
    ) -> Result<usize> {
        let uri = self
            .ask(service_id)
            .and_then(|ask| Self::source(&ask, item))
            .ok()
            .map(|(uri, _)| uri);
        let favorites = upnp.browse_content("FV:2").await?;
        let found = Self::favorites_of(&favorites, service_id, item, uri.as_deref());
        anyhow::ensure!(!found.is_empty(), "{} is not a Sonos favorite", item.title);
        for favorite in &found {
            upnp.destroy_favorite(&favorite.id).await?;
        }
        Ok(found.len())
    }

    /// The chapter a book resumes at, where its service keeps the place.
    pub async fn resume_chapter(&self, service_id: &str, book: &str) -> Result<Option<String>> {
        let Ask { service, token } = self.ask(service_id)?;
        let mut refreshed = None;
        let resume = smapi::resume_point(&service, token.as_ref(), book, &mut refreshed).await;
        if refreshed.is_some() {
            x2rock::play::after_refresh(&service.id, token.as_ref(), refreshed);
            self.follow_store();
        }
        Ok(resume?.0.map(|r| r.id))
    }

    /// Play an item in a room, by the CLI's own routes.
    pub async fn play(
        &self,
        session: &Session,
        room: &str,
        service_id: &str,
        item: &Item,
    ) -> Result<()> {
        let Ask { service, token } = self.ask(service_id)?;
        let done = x2rock::play::play_item(
            session,
            Some(room),
            &service,
            token.as_ref(),
            Some(&item.item_type),
            &item.id,
            &item.title,
            false,
        )
        .await;
        // x2rock's play routes refresh and store a token without saying so.
        self.follow_store();
        done
    }
}

/// The services for a session: opened on first use, once. A second task that
/// wants them while the first is opening them waits for that open rather than
/// starting another; one that fails leaves them to be opened next time.
#[derive(Default)]
pub struct Lazy(tokio::sync::OnceCell<Open>);

impl Lazy {
    pub async fn open(&self, session: &Session) -> Result<&Open> {
        self.0
            .get_or_try_init(|| async {
                Services::open(session)
                    .await
                    .map(|s| Open(std::sync::Mutex::new(s)))
            })
            .await
    }
}

/// Shared by the runtime's tasks.
pub type Shared = Arc<Lazy>;

#[cfg(test)]
mod tests {
    use super::*;

    fn cat(id: &str) -> Category {
        Category {
            id: id.into(),
            mapped_id: id.into(),
            default: false,
        }
    }

    #[test]
    fn two_categories_are_asked_the_two_most_wanted() {
        let got = pick_categories(&[
            cat("artists"),
            cat("stations"),
            cat("albums"),
            cat("tracks"),
        ]);
        assert_eq!(
            got.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            ["tracks", "albums"]
        );
    }

    #[test]
    fn a_service_with_none_of_the_wanted_ones_is_asked_its_first_two() {
        let got = pick_categories(&[cat("genres"), cat("moods"), cat("hosts")]);
        assert_eq!(
            got.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            ["genres", "moods"]
        );
    }

    /// Services on local listeners: each id with its port, and the categories
    /// it is known to search (none: not searched).
    fn opened(services: &[(&str, u16, &[&str])]) -> Open {
        let catalogue: Catalogue = serde_json::from_value(serde_json::json!({
            "schema": 2,
            "version": "test",
            "services": services.iter().map(|(id, port, _)| serde_json::json!({
                "id": id, "name": format!("Service {id}"), "uri": format!("http://127.0.0.1:{port}/smapi"),
                "auth": "Anonymous", "manifest_uri": null, "service_type": null,
            })).collect::<Vec<_>>(),
            "categories": services.iter().map(|(id, _, categories)| {
                (id.to_string(), categories.iter().map(|c| serde_json::json!({ "id": c, "mapped_id": c })).collect::<Vec<_>>())
            }).collect::<std::collections::BTreeMap<_, _>>(),
        }))
        .expect("a catalogue");
        Open(std::sync::Mutex::new(Services {
            catalogue,
            linked: Credentials::default(),
            household: "Sonos_test".into(),
            last_read: None,
        }))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_service_that_does_not_answer_a_search_holds_up_no_other_request() {
        use tokio::net::TcpListener;
        // One service takes the search and never answers it; the other closes
        // every connection at once, so a request to it fails straight away.
        let stalled = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let open = Arc::new(opened(&[
            ("1", stalled.local_addr().unwrap().port(), &["tracks"]),
            ("2", closed.local_addr().unwrap().port(), &[]),
        ]));
        let (asked, being_asked) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (held, _) = stalled.accept().await.unwrap();
            let _ = asked.send(());
            // Held open, unanswered, for the test's life.
            std::future::pending::<()>().await;
            drop(held);
        });
        tokio::spawn(async move {
            while let Ok((connection, _)) = closed.accept().await {
                drop(connection);
            }
        });
        let search = tokio::spawn({
            let open = open.clone();
            async move { open.search("anything", |_| {}).await }
        });
        being_asked
            .await
            .expect("the search reached the stalled service");
        let browse = tokio::spawn(async move { open.browse("2", "root", 0, 10).await });
        tokio::select! {
            browsed = browse => assert!(browsed.unwrap().is_err(), "the closed service's page fails, and at once"),
            _ = search => panic!("the search ended first: the page waited for it"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_token_refreshed_by_a_search_ended_early_is_the_one_held() {
        // The refresh writes the credential store: only where that is a
        // throwaway, as `pl check` makes it, never a person's own accounts.
        let store = x2rock::store::path("credentials.json").unwrap();
        if !store.starts_with(std::env::temp_dir()) {
            eprintln!(
                "skipped: the credential store {} is not a temporary one",
                store.display()
            );
            return;
        }
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::{TcpListener, TcpStream};
        async fn request(socket: &mut TcpStream) -> String {
            let mut bytes = Vec::new();
            loop {
                let mut chunk = [0u8; 4096];
                let n = socket.read(&mut chunk).await.unwrap();
                assert!(n > 0, "the request ended early");
                bytes.extend_from_slice(&chunk[..n]);
                if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&bytes[..end]);
                    let length: usize = head
                        .lines()
                        .find_map(|l| {
                            l.split_once(':')
                                .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                                .map(|(_, v)| v.trim().parse().unwrap())
                        })
                        .unwrap();
                    if bytes.len() >= end + 4 + length {
                        return String::from_utf8(bytes).unwrap();
                    }
                }
            }
        }
        async fn reply(socket: &mut TcpStream, status: u16, body: &str) {
            socket.write_all(format!("HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
        // A service that refreshes the token on the first category, answers its
        // retry, and takes the second category without answering it. Synthetic
        // SOAP, after codex's reproduction in D3's first review.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let open = Arc::new(opened(&[(
            "1",
            listener.local_addr().unwrap().port(),
            &["tracks", "albums"],
        )]));
        let account = serde_json::from_value(serde_json::json!({
            "service_name": "Service 1", "auth_token": "synthetic-old", "private_key": "synthetic-key", "household": "Sonos_test", "linked": 0,
        }))
        .unwrap();
        {
            let mut s = open.lock();
            s.linked.remember("Sonos_test", "1", account);
            s.linked.save().unwrap();
        }
        let (asked_again, second_category) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            assert!(request(&mut socket).await.contains("synthetic-old"));
            reply(&mut socket, 500, r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><s:Fault><faultcode>s:Client.TokenRefreshRequired</faultcode><faultstring>tokenRefreshRequired</faultstring><detail><refreshAuthTokenResult><authToken>synthetic-new</authToken><privateKey>synthetic-new-key</privateKey></refreshAuthTokenResult></detail></s:Fault></s:Body></s:Envelope>"#).await;
            // Closed, as its header says: the client reads the body to the end.
            drop(socket);
            let (mut socket, _) = listener.accept().await.unwrap();
            assert!(request(&mut socket).await.contains("synthetic-new"));
            reply(&mut socket, 200, r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><searchResponse><searchResult><index>0</index><count>0</count><total>0</total></searchResult></searchResponse></s:Body></s:Envelope>"#).await;
            drop(socket);
            let (mut held, _) = listener.accept().await.unwrap();
            assert!(request(&mut held).await.contains("synthetic-new"));
            let _ = asked_again.send(());
            std::future::pending::<()>().await;
            drop(held);
        });
        let search = tokio::spawn({
            let open = open.clone();
            async move { open.search("anything", |_| {}).await }
        });
        // The timeout only turns a hang into a failure.
        tokio::time::timeout(Duration::from_secs(10), second_category)
            .await
            .expect("the second category was asked")
            .unwrap();
        search.abort();
        assert!(search.await.unwrap_err().is_cancelled());
        assert_eq!(
            Credentials::load()
                .unwrap()
                .token_for("Sonos_test", "1")
                .unwrap()
                .token,
            "synthetic-new",
            "stored"
        );
        assert_eq!(
            open.ask("1").unwrap().token.unwrap().token,
            "synthetic-new",
            "and held, though the search ended before its section"
        );
    }
}
