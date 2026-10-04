//! `x2rock scene`: save how the household is arranged, and put it back.
//!
//! A scene is the groups - which rooms play together, and which of them
//! coordinates - each room's own volume, each group's mute, and optionally a
//! soundtrack for a group: a Sonos favorite or a kept bookmark, started once
//! the rooms are in place. "Beach": the patio and the kitchen together at
//! 30 and 25, playing an ocean-waves favorite.
//!
//! Kept in `$XDG_STATE_HOME/x2rock/scenes.json`, per household, by room id
//! with the name beside it, so a renamed room still matches and a scene from
//! another household is never applied here.
//!
//! **Applying never removes a coordinator from its group.** Doing that hands
//! the leaving room's queue to whoever stays and costs them theirs (see
//! "Grouping over the local API" in docs/architecture.md). A room that has to
//! coordinate is first taken out of the group it is a member of - a member
//! leaving is harmless - and its group is then built around it. Only what
//! differs is changed: regrouping stalls playback in every room it touches.
//!
//! Rooms the scene does not name are left where they are, unless they sit in
//! a group the scene reshapes, which they then leave to play on their own. A
//! room the scene names that is not in the household now is skipped and said.

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use super::content::{find_favorite, start_bookmark};
use super::household::change_group;
use super::{Report, emit};
use crate::bookmarks;
use crate::session::{self, Session};
use crate::sonos::proto::{Group, Groups};
use crate::store;

/// One room in a scene, by id with its name beside it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SceneRoom {
    pub id: String,
    pub name: String,
    /// The room's own level. `None` for a fixed-volume room (line-level out
    /// into an amplifier), which has none to set.
    #[serde(default)]
    pub volume: Option<u8>,
}

/// What a group starts playing once it is in place.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Soundtrack {
    Favorite { name: String },
    Bookmark { name: String },
}

impl Soundtrack {
    fn name(&self) -> &str {
        match self {
            Self::Favorite { name } | Self::Bookmark { name } => name,
        }
    }

    /// Which kind of soundtrack, as the `--json` output names it.
    fn kind(&self) -> &'static str {
        match self {
            Self::Favorite { .. } => "favorite",
            Self::Bookmark { .. } => "bookmark",
        }
    }
}

/// One group: its rooms, the coordinator first.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SceneGroup {
    pub rooms: Vec<SceneRoom>,
    #[serde(default)]
    pub muted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub play: Option<Soundtrack>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Scene {
    pub name: String,
    pub household: String,
    pub groups: Vec<SceneGroup>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Scenes {
    #[serde(default)]
    scenes: Vec<Scene>,
}

fn path() -> Result<std::path::PathBuf> {
    store::path("scenes.json")
}

impl Scenes {
    fn load() -> Result<Self> {
        let path = path()?;
        match store::read_optional(&path)? {
            None => Ok(Self::default()),
            Some(text) => {
                serde_json::from_str(&text).with_context(|| format!("reading {}", path.display()))
            }
        }
    }

    /// Read, change and write under the store's lock, so two writers - a
    /// person and an agent, say - cannot lose each other's scene.
    fn update<T>(change: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        let path = path()?;
        let _lock = store::Lock::exclusive(&path)?;
        let mut scenes = Self::load()?;
        let answer = change(&mut scenes)?;
        let text = serde_json::to_string_pretty(&scenes)?;
        store::write_atomically(&path, &format!("{text}\n"), store::PLAIN)?;
        Ok(answer)
    }

    fn position(&self, household: &str, name: &str) -> Option<usize> {
        self.scenes
            .iter()
            .position(|s| s.household == household && s.name.eq_ignore_ascii_case(name))
    }

    fn find(&self, household: &str, name: &str) -> Result<&Scene> {
        self.position(household, name)
            .map(|i| &self.scenes[i])
            .ok_or_else(|| {
                let here: Vec<&str> = self
                    .scenes
                    .iter()
                    .filter(|s| s.household == household)
                    .map(|s| s.name.as_str())
                    .collect();
                if here.is_empty() {
                    anyhow!("no scene named {name:?}; this household has none saved yet")
                } else {
                    anyhow!("no scene named {name:?}. Saved: {}", here.join(", "))
                }
            })
    }
}

/// The next change that brings `now` closer to one group of `ids`, whose
/// first is the coordinator - or `None` once it is there.
///
/// One step at a time, re-read between them, because each change moves other
/// groups too: a room joining here leaves wherever it was.
#[derive(Debug)]
enum Step {
    /// Take the would-be coordinator out of the group it is a member of.
    Detach { group: Group, player: String },
    /// Add and remove members around a coordinator that already coordinates.
    Reshape {
        group: Group,
        add: Vec<String>,
        remove: Vec<String>,
    },
}

fn next_step(now: &Groups, ids: &[String]) -> Result<Option<Step>> {
    let Some(coordinator) = ids.first() else {
        return Ok(None);
    };
    let current = now
        .group_of(coordinator)
        .ok_or_else(|| anyhow!("{coordinator} is in no group"))?;
    if &current.coordinator_id != coordinator {
        return Ok(Some(Step::Detach {
            group: current.clone(),
            player: coordinator.clone(),
        }));
    }
    let add: Vec<String> = ids
        .iter()
        .filter(|id| !current.player_ids.contains(id))
        .cloned()
        .collect();
    let remove: Vec<String> = current
        .player_ids
        .iter()
        .filter(|id| !ids.contains(id))
        .cloned()
        .collect();
    if add.is_empty() && remove.is_empty() {
        return Ok(None);
    }
    Ok(Some(Step::Reshape {
        group: current.clone(),
        add,
        remove,
    }))
}

/// The ids of a scene group's rooms as the household has them now, the
/// coordinator first, with the ones it no longer has set aside by name.
fn present(now: &Groups, group: &SceneGroup) -> (Vec<String>, Vec<String>) {
    let mut ids = Vec::new();
    let mut missing = Vec::new();
    for room in &group.rooms {
        let found = now.player(&room.id).or_else(|| {
            now.players
                .iter()
                .find(|p| p.name.eq_ignore_ascii_case(&room.name))
        });
        match found {
            Some(p) if !ids.contains(&p.id) => ids.push(p.id.clone()),
            Some(_) => {}
            None => missing.push(room.name.clone()),
        }
    }
    (ids, missing)
}

fn room_names(scene_group: &SceneGroup) -> String {
    scene_group
        .rooms
        .iter()
        .map(|r| r.name.as_str())
        .collect::<Vec<_>>()
        .join(" + ")
}

#[derive(Serialize)]
pub struct SceneOutcome {
    scene: String,
    /// `saved`, `applied` or `deleted`.
    action: &'static str,
    /// Each group's rooms, the coordinator first.
    groups: Vec<Vec<String>>,
    /// Group changes made while applying.
    #[serde(skip_serializing_if = "Option::is_none")]
    regrouped: Option<usize>,
    /// What each group started, by its coordinator's room.
    playing: Vec<Playing>,
    #[serde(skip)]
    notes: Vec<String>,
}

#[derive(Serialize)]
struct Playing {
    room: String,
    title: String,
    kind: &'static str,
}

impl Report for SceneOutcome {
    fn text(&self) -> String {
        let groups: Vec<String> = self.groups.iter().map(|g| g.join(" + ")).collect();
        let mut lines = vec![format!(
            "{} {}: {}",
            match self.action {
                "saved" => "Saved",
                "deleted" => "Deleted",
                _ => "Applied",
            },
            self.scene,
            groups.join(", ")
        )];
        for p in &self.playing {
            let verb = if self.action == "applied" {
                "playing"
            } else {
                "plays"
            };
            lines.push(format!("  {:<22} {verb} {}", p.room, p.title));
        }
        lines.join("\n")
    }

    fn notes(&self) -> &[String] {
        &self.notes
    }
}

#[derive(Serialize)]
struct SceneList {
    scenes: Vec<Scene>,
}

impl Report for SceneList {
    fn text(&self) -> String {
        if self.scenes.is_empty() {
            return "No scenes saved for this household. Save one with `x2rock scene save <name>`."
                .into();
        }
        let mut lines = Vec::new();
        for scene in &self.scenes {
            lines.push(scene.name.clone());
            for group in &scene.groups {
                let levels: Vec<String> = group
                    .rooms
                    .iter()
                    .map(|r| match r.volume {
                        Some(v) => format!("{} {v}", r.name),
                        None => format!("{} fixed", r.name),
                    })
                    .collect();
                let mut line = format!("  {}", levels.join(", "));
                if group.muted {
                    line.push_str("  (muted)");
                }
                if let Some(play) = &group.play {
                    line.push_str(&format!("  plays {}", play.name()));
                }
                lines.push(line);
            }
        }
        lines.join("\n")
    }
}

/// `x2rock scene` / `scene list`.
pub async fn list(session: &Session, json: bool) -> Result<()> {
    let household = session.connection.household_id().await?;
    let scenes = Scenes::load()?
        .scenes
        .into_iter()
        .filter(|s| s.household == household)
        .collect();
    emit(&SceneList { scenes }, json)
}

/// What `--play` names: a Sonos favorite first, then a kept bookmark.
async fn soundtrack(session: &Session, household: &str, query: &str) -> Result<Soundtrack> {
    let favorites = session.connection.favorites(household).await?;
    if let Ok(favorite) = find_favorite(&favorites.items, query) {
        return Ok(Soundtrack::Favorite {
            name: favorite.name.clone(),
        });
    }
    let kept = bookmarks::Bookmarks::load()?;
    if let Ok(bookmark) = kept.find(query) {
        return Ok(Soundtrack::Bookmark {
            name: bookmark.name.clone(),
        });
    }
    bail!(
        "no Sonos favorite or kept bookmark named {query:?} - `x2rock favorites` and \
         `x2rock bookmarks` list what there is"
    )
}

/// `x2rock scene save`: the household as it is now, under `name`.
pub async fn save(
    session: &Session,
    room: Option<&str>,
    name: &str,
    play: Option<&str>,
    json: bool,
) -> Result<()> {
    let name = name.trim();
    if name.is_empty() {
        bail!("a scene needs a name");
    }
    let household = session.connection.household_id().await?;
    let groups = &session.groups;
    let mut notes = Vec::new();
    // Which group the soundtrack is for, settled before anything is read, so
    // a household with several groups and no --room fails at once.
    let playing_group = match play {
        Some(_) => Some(groups.resolve(room)?.id.clone()),
        None => None,
    };
    let track = match play {
        Some(query) => Some(soundtrack(session, &household, query).await?),
        None => None,
    };

    let mut scene_groups = Vec::new();
    for group in &groups.groups {
        // Coordinator first: it is who the group is built around.
        let mut members = groups.members(group);
        members.sort_by_key(|p| p.id != group.coordinator_id);
        let mut rooms = Vec::new();
        for player in members {
            let volume = match session.player(player.ip()).await {
                Ok(conn) => match conn.player_volume(&player.id).await {
                    Ok(v) if v.fixed => None,
                    Ok(v) => Some(v.volume),
                    Err(e) => {
                        notes.push(format!("note: {}: volume not read ({e:#})", player.name));
                        None
                    }
                },
                Err(e) => {
                    notes.push(format!("note: {}: not reached ({e:#})", player.name));
                    None
                }
            };
            rooms.push(SceneRoom {
                id: player.id.clone(),
                name: player.name.clone(),
                volume,
            });
        }
        let target = session::target_for(groups, group);
        let muted = match session::coordinator(session, &target).await {
            Ok(conn) => conn.group_volume(&group.id).await.is_ok_and(|v| v.muted),
            Err(_) => false,
        };
        let play = (playing_group.as_deref() == Some(group.id.as_str()))
            .then(|| track.clone())
            .flatten();
        scene_groups.push(SceneGroup { rooms, muted, play });
    }

    let scene = Scene {
        name: name.to_owned(),
        household: household.clone(),
        groups: scene_groups,
    };
    let replaced = Scenes::update(|all| {
        let at = all.position(&household, name);
        match at {
            Some(i) => all.scenes[i] = scene.clone(),
            None => all.scenes.push(scene.clone()),
        }
        Ok(at.is_some())
    })?;
    if replaced {
        notes.push(format!("note: replaced the scene already named {name:?}"));
    }
    emit(&outcome(&scene, "saved", None, played(&scene), notes), json)
}

/// Each soundtrack in a scene, as the report names it.
fn played(scene: &Scene) -> Vec<Playing> {
    scene
        .groups
        .iter()
        .filter_map(|g| {
            let play = g.play.as_ref()?;
            Some(Playing {
                room: g.rooms.first()?.name.clone(),
                title: play.name().to_owned(),
                kind: play.kind(),
            })
        })
        .collect()
}

fn outcome(
    scene: &Scene,
    action: &'static str,
    regrouped: Option<usize>,
    playing: Vec<Playing>,
    notes: Vec<String>,
) -> SceneOutcome {
    SceneOutcome {
        scene: scene.name.clone(),
        action,
        groups: scene
            .groups
            .iter()
            .map(|g| g.rooms.iter().map(|r| r.name.clone()).collect())
            .collect(),
        regrouped,
        playing,
        notes,
    }
}

/// `x2rock scene apply`: regroup, set each room's level and each group's
/// mute, then start the soundtracks - in that order, so music starts in the
/// right rooms at the right level.
pub async fn apply(session: &Session, name: &str, json: bool) -> Result<()> {
    let household = session.connection.household_id().await?;
    let scene = Scenes::load()?.find(&household, name)?.clone();
    let mut now = session.connection.groups().await?;
    let mut notes = Vec::new();
    let mut regrouped = 0;

    for group in &scene.groups {
        let (ids, missing) = present(&now, group);
        for name in missing {
            notes.push(format!(
                "note: {name} is not in the household now; {} applied without it",
                scene.name
            ));
        }
        // Bounded: each step settles one room's place, so a plan that has not
        // arrived after that many is a player refusing, not a longer plan.
        for _ in 0..=ids.len() + now.players.len() {
            let Some(step) = next_step(&now, &ids)? else {
                break;
            };
            let changed = match step {
                Step::Detach { group, player } => {
                    change_group(session, &group, &[], &[player]).await?
                }
                Step::Reshape { group, add, remove } => {
                    change_group(session, &group, &add, &remove).await?
                }
            };
            notes.extend(changed.note);
            regrouped += 1;
            now = session.connection.groups().await?;
        }
        if next_step(&now, &ids)?.is_some() {
            notes.push(format!(
                "note: {} did not settle into the scene's grouping",
                room_names(group)
            ));
        }
    }

    for group in &scene.groups {
        for room in &group.rooms {
            let Some(level) = room.volume else { continue };
            let Some(player) = now.player(&room.id).or_else(|| {
                now.players
                    .iter()
                    .find(|p| p.name.eq_ignore_ascii_case(&room.name))
            }) else {
                continue;
            };
            let set = match session.player(player.ip()).await {
                Ok(conn) => conn.set_player_volume(&player.id, level).await,
                Err(e) => Err(e),
            };
            if let Err(e) = set {
                notes.push(format!("note: {}: volume not set ({e:#})", room.name));
            }
        }
    }

    let mut playing = Vec::new();
    for group in &scene.groups {
        let (ids, _) = present(&now, group);
        let Some(live) = ids.first().and_then(|id| now.group_of(id)).cloned() else {
            continue;
        };
        let target = session::target_for(&now, &live);
        let conn = match session::coordinator(session, &target).await {
            Ok(conn) => conn,
            Err(e) => {
                notes.push(format!("note: {}: not reached ({e:#})", room_names(group)));
                continue;
            }
        };
        // Set either way: a level being set unmutes, and a group saved muted
        // should come back muted.
        if let Err(e) = conn.set_group_mute(&live.id, group.muted).await {
            notes.push(format!("note: {}: mute not set ({e:#})", room_names(group)));
        }
        let Some(play) = &group.play else { continue };
        let started = match play {
            Soundtrack::Favorite { name } => {
                let favorites = session.connection.favorites(&household).await?;
                match find_favorite(&favorites.items, name) {
                    Ok(favorite) => conn
                        .load_favorite(&live.id, &favorite.id)
                        .await
                        .map(|()| favorite.name.clone()),
                    Err(e) => Err(e),
                }
            }
            Soundtrack::Bookmark { name } => start_bookmark(session, &conn, &target, name).await,
        };
        match started {
            Ok(title) => playing.push(Playing {
                room: target.name.clone(),
                title,
                kind: play.kind(),
            }),
            Err(e) => notes.push(format!(
                "note: {}: {:?} did not start: {e:#}",
                room_names(group),
                play.name()
            )),
        }
    }

    emit(
        &outcome(&scene, "applied", Some(regrouped), playing, notes),
        json,
    )
}

/// `x2rock scene delete`.
pub async fn delete(session: &Session, name: &str, json: bool) -> Result<()> {
    let household = session.connection.household_id().await?;
    let gone = Scenes::update(|all| {
        let scene = all.find(&household, name)?.clone();
        all.scenes
            .retain(|s| !(s.household == household && s.name.eq_ignore_ascii_case(name)));
        Ok(scene)
    })?;
    emit(
        &outcome(&gone, "deleted", None, Vec::new(), Vec::new()),
        json,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sonos::proto::Player;

    fn player(id: &str) -> Player {
        Player {
            id: id.into(),
            name: id.into(),
            websocket_url: format!("wss://10.0.0.{}:1443/websocket/api", id.len()),
            capabilities: Vec::new(),
        }
    }

    fn household(groups: &[(&str, &[&str])]) -> Groups {
        let mut players = Vec::new();
        let groups = groups
            .iter()
            .map(|(coordinator, members)| {
                for m in *members {
                    players.push(player(m));
                }
                Group {
                    id: format!("{coordinator}:g"),
                    name: (*coordinator).into(),
                    coordinator_id: (*coordinator).into(),
                    playback_state: String::new(),
                    player_ids: members.iter().map(|m| (*m).into()).collect(),
                }
            })
            .collect();
        Groups { groups, players }
    }

    fn ids(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| (*n).into()).collect()
    }

    /// Apply a step the way the player does: a room added leaves wherever it
    /// was, a room removed becomes a group of its own.
    fn perform(now: &Groups, step: &Step) -> Groups {
        let (coordinator, add, remove) = match step {
            Step::Detach { group, player } => {
                (group.coordinator_id.clone(), vec![], vec![player.clone()])
            }
            Step::Reshape { group, add, remove } => {
                (group.coordinator_id.clone(), add.clone(), remove.clone())
            }
        };
        let mut groups: Vec<Group> = now
            .groups
            .iter()
            .map(|g| {
                let mut g = g.clone();
                if g.coordinator_id != coordinator {
                    g.player_ids.retain(|id| !add.contains(id));
                    if !g.player_ids.contains(&g.coordinator_id)
                        && let Some(first) = g.player_ids.first().cloned()
                    {
                        g.coordinator_id = first;
                    }
                } else {
                    g.player_ids.retain(|id| !remove.contains(id));
                    g.player_ids.extend(add.iter().cloned());
                }
                g
            })
            .filter(|g| !g.player_ids.is_empty())
            .collect();
        for id in &remove {
            groups.push(Group {
                id: format!("{id}:g"),
                name: id.clone(),
                coordinator_id: id.clone(),
                playback_state: String::new(),
                player_ids: vec![id.clone()],
            });
        }
        Groups {
            groups,
            players: now.players.clone(),
        }
    }

    fn settle(mut now: Groups, want: &[String]) -> (Groups, Vec<Step>) {
        let mut steps = Vec::new();
        while let Some(step) = next_step(&now, want).unwrap() {
            assert!(steps.len() < 10, "no convergence: {steps:?}");
            now = perform(&now, &step);
            steps.push(step);
        }
        (now, steps)
    }

    #[test]
    fn an_arrangement_already_in_place_changes_nothing() {
        let now = household(&[("Patio", &["Patio", "Kitchen"]), ("Den", &["Den"])]);
        assert!(
            next_step(&now, &ids(&["Patio", "Kitchen"]))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn rooms_on_their_own_are_gathered_in_one_change() {
        let now = household(&[("Patio", &["Patio"]), ("Kitchen", &["Kitchen"])]);
        let (after, steps) = settle(now, &ids(&["Patio", "Kitchen"]));
        assert_eq!(steps.len(), 1);
        let g = after.group_of("Patio").unwrap();
        assert_eq!(g.coordinator_id, "Patio");
        assert!(g.player_ids.contains(&"Kitchen".to_string()));
    }

    /// Kitchen coordinates Patio now, and the scene wants Patio coordinating.
    /// Kitchen must not be removed from its own group - that swaps queues -
    /// so Patio leaves first and then takes Kitchen in.
    #[test]
    fn a_member_that_must_coordinate_leaves_first_and_no_coordinator_is_removed() {
        let now = household(&[("Kitchen", &["Kitchen", "Patio"])]);
        let (after, steps) = settle(now, &ids(&["Patio", "Kitchen"]));
        assert!(matches!(&steps[0], Step::Detach { player, .. } if player == "Patio"));
        for step in &steps {
            let (group, removed) = match step {
                Step::Detach { group, player } => (group, std::slice::from_ref(player)),
                Step::Reshape { group, remove, .. } => (group, remove.as_slice()),
            };
            assert!(
                !removed.contains(&group.coordinator_id),
                "removed a coordinator from its own group: {step:?}"
            );
        }
        let g = after.group_of("Patio").unwrap();
        assert_eq!(g.coordinator_id, "Patio");
        assert_eq!(g.player_ids.len(), 2);
    }

    #[test]
    fn a_room_the_scene_leaves_out_is_let_go_from_its_group() {
        let now = household(&[("Patio", &["Patio", "Kitchen", "Den"])]);
        let (after, _) = settle(now, &ids(&["Patio", "Kitchen"]));
        assert_eq!(after.group_of("Den").unwrap().coordinator_id, "Den");
        assert_eq!(after.group_of("Patio").unwrap().player_ids.len(), 2);
    }

    #[test]
    fn a_room_that_has_gone_is_set_aside_and_a_renamed_one_still_found() {
        let now = household(&[("Patio", &["Patio"])]);
        let group = SceneGroup {
            rooms: vec![
                SceneRoom {
                    id: "Patio".into(),
                    name: "Old Name".into(),
                    volume: Some(30),
                },
                SceneRoom {
                    id: "RINCON_GONE".into(),
                    name: "Shed".into(),
                    volume: Some(20),
                },
            ],
            muted: false,
            play: None,
        };
        let (ids, missing) = present(&now, &group);
        assert_eq!(ids, vec!["Patio".to_string()]);
        assert_eq!(missing, vec!["Shed".to_string()]);
    }

    #[test]
    fn a_soundtrack_is_stored_with_its_kind() {
        let group = SceneGroup {
            rooms: vec![],
            muted: false,
            play: Some(Soundtrack::Favorite {
                name: "Ocean Waves".into(),
            }),
        };
        let json = serde_json::to_value(&group).unwrap();
        assert_eq!(json["play"]["kind"], "favorite");
        assert_eq!(json["play"]["name"], "Ocean Waves");
        let back: SceneGroup = serde_json::from_value(json).unwrap();
        assert_eq!(back, group);
    }
}
