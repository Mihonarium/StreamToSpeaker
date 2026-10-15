//! Sonos zone-group topology.
//!
//! Sonos speakers can be grouped in the Sonos app; a group has exactly one
//! *coordinator* and the rest are members that mirror its audio. Two facts
//! drive everything here:
//!
//!  * AVTransport commands only make sense on the coordinator. Sending
//!    `SetAVTransportURI` to a non-coordinator member silently rips it out
//!    of its group (that is how Sonos "moves" a zone between groups).
//!  * Volume for the whole group lives on the coordinator's
//!    `GroupRenderingControl` service (`SetGroupVolume` scales every
//!    member proportionally, exactly like the group slider in the Sonos
//!    app); the plain `RenderingControl` volume only moves the
//!    coordinator's own speaker.
//!
//! Topology comes from the `ZoneGroupTopology` service every Sonos device
//! exposes on its root device: `GetZoneGroupState` returns an
//! entity-encoded XML document listing groups, coordinators, members and
//! their visibility. Bonded satellites (the right speaker of a stereo
//! pair, a Sub, surrounds) are marked `Invisible="1"` — they answer SSDP
//! but must never be offered as targets.
//!
//! Non-Sonos renderers (no ZoneGroupTopology service) pass through
//! untouched, and every network failure here fails open: a unit whose own
//! topology can't be read stays listed unless it said in a recent scan
//! that it is bonded or grouped, and without any topology the list is
//! left as discovery found it.
//!
//! Any device can answer discovery and serve a topology, so a unit is
//! hidden only on its own word (see [`apply_topology`]). Topology URLs and
//! the member Locations in a topology come from devices on the network;
//! discovery follows them only to device addresses on this PC's networks
//! (see `LanTargets`), and so does the check before streaming to a
//! speaker discovery found (see [`resolve_group_coordinator`]).

use crate::discovery_net::{shown, Reach, Reported};
use crate::ssdp::Renderer;
use crate::upnp;
use log::{debug, info};
use std::borrow::Cow;
use std::collections::HashMap;

/// GetZoneGroupState from a ZoneGroupTopology control URL (asked from
/// several threads at once at discovery time).
type TopologyFn<'a> = dyn Fn(&str) -> anyhow::Result<Vec<ZoneGroup>> + Sync + 'a;
/// A device description, by its URL.
type DescribeFn<'a> = dyn Fn(&str) -> Option<Renderer> + 'a;

/// A group coordinator's description, from the Location a topology gives.
/// `reported`: a discovery list's memory, so that service URLs ignored in
/// it are logged once.
fn describe_device(location: &str, reported: Option<&Reported>) -> Option<Renderer> {
    let urls = crate::ssdp::ServiceUrls::OnItsHost(reported);
    crate::ssdp::fetch_and_parse_device(location, std::time::Duration::from_secs(3), urls).ok()
}

/// The topology URL a renderer's description gives, for a log line.
fn topology_url_of(r: &Renderer) -> String {
    format!("the topology URL of {}", shown(&r.friendly_name))
}

/// A group coordinator's Location in a topology, for a log line.
fn coordinator_location_of(m: &ZoneMember) -> String {
    format!("the Location of group coordinator {} in a Sonos topology", shown(&m.zone_name))
}

/// One zone group as reported by ZoneGroupTopology.
#[derive(Debug, Clone)]
pub struct ZoneGroup {
    /// UUID (RINCON_...) of the coordinator zone.
    pub coordinator_uuid: String,
    pub members: Vec<ZoneMember>,
}

/// One member zone (or bonded satellite) of a group.
#[derive(Debug, Clone)]
pub struct ZoneMember {
    /// UUID without the `uuid:` prefix, e.g. `RINCON_949F3EC2E15801400`.
    pub uuid: String,
    /// Room name ("Living Room").
    pub zone_name: String,
    /// URL of this member's own device description.
    pub location: String,
    /// True for bonded units that must not be shown (stereo-pair slave,
    /// Sub, surrounds) — they can't be targeted independently.
    pub invisible: bool,
}

impl ZoneGroup {
    /// Zone names of the visible members, coordinator first.
    pub fn visible_member_names(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        if let Some(c) = self.members.iter().find(|m| m.uuid == self.coordinator_uuid) {
            names.push(c.zone_name.clone());
        }
        for m in &self.members {
            if m.uuid != self.coordinator_uuid && !m.invisible {
                names.push(m.zone_name.clone());
            }
        }
        names
    }
}

/// Fetch and parse the zone-group state from a device's ZoneGroupTopology
/// control URL.
pub fn fetch_zone_groups(control_url: &str) -> anyhow::Result<Vec<ZoneGroup>> {
    let body = upnp::get_zone_group_state(control_url)?;
    Ok(parse_zone_group_state(&body))
}

/// Parse the (already entity-decoded) ZoneGroupState XML document.
///
/// Shape (current firmware wraps it in `<ZoneGroupState><ZoneGroups>`,
/// older firmware returned `<ZoneGroups>` directly — we just walk to the
/// `ZoneGroup` elements wherever they are):
///
/// ```xml
/// <ZoneGroups>
///   <ZoneGroup Coordinator="RINCON_A" ID="RINCON_A:42">
///     <ZoneGroupMember UUID="RINCON_A" Location="http://..." ZoneName="Living Room">
///       <Satellite UUID="RINCON_S" Location="http://..." ZoneName="Living Room" Invisible="1"/>
///     </ZoneGroupMember>
///     <ZoneGroupMember UUID="RINCON_B" Location="http://..." ZoneName="Kitchen"/>
///   </ZoneGroup>
/// </ZoneGroups>
/// ```
pub fn parse_zone_group_state(xml: &str) -> Vec<ZoneGroup> {
    const MAX_TOPOLOGY_BYTES: usize = 1024 * 1024;
    const MAX_GROUPS: usize = 64;
    const MAX_MEMBERS: usize = 1024;
    const MAX_COPIED_ATTRIBUTE_BYTES: usize = 1024 * 1024;

    if xml.len() > MAX_TOPOLOGY_BYTES {
        debug!("ZoneGroupState exceeds {} bytes", MAX_TOPOLOGY_BYTES);
        return Vec::new();
    }
    let doc = match roxmltree::Document::parse(xml.trim_start()) {
        Ok(d) => d,
        Err(e) => {
            debug!("ZoneGroupState parse failed: {}", e);
            return Vec::new();
        }
    };

    let mut groups = Vec::new();
    let mut member_count = 0usize;
    let mut copied_attribute_bytes = 0usize;
    for g in doc
        .descendants()
        .filter(|n| n.tag_name().name() == "ZoneGroup")
    {
        // ZoneGroup elements are siblings in the protocol.  Reject nesting so
        // a member can never be attributed to several ancestor groups.
        if g.ancestors()
            .skip(1)
            .any(|n| n.is_element() && n.tag_name().name() == "ZoneGroup")
            || groups.len() == MAX_GROUPS
        {
            debug!("invalid or oversized ZoneGroupState");
            return Vec::new();
        }
        let Some(coordinator) = g.attribute("Coordinator") else {
            continue;
        };
        let mut members = Vec::new();
        // Only accept members directly owned by this group and satellites
        // directly owned by those members.  In particular, do not traverse
        // into a nested group.
        let members_and_satellites = g
            .children()
            .filter(|n| n.is_element() && n.tag_name().name() == "ZoneGroupMember")
            .flat_map(|member| {
                std::iter::once(member).chain(
                    member
                        .children()
                        .filter(|n| n.is_element() && n.tag_name().name() == "Satellite"),
                )
            });
        for m in members_and_satellites {
            let Some(uuid) = m.attribute("UUID") else {
                continue;
            };
            let zone_name = m.attribute("ZoneName").unwrap_or("");
            let location = m.attribute("Location").unwrap_or("");
            member_count += 1;
            copied_attribute_bytes = copied_attribute_bytes
                .saturating_add(uuid.len())
                .saturating_add(zone_name.len())
                .saturating_add(location.len());
            if member_count > MAX_MEMBERS || copied_attribute_bytes > MAX_COPIED_ATTRIBUTE_BYTES {
                debug!("ZoneGroupState member allocation budget exceeded");
                return Vec::new();
            }
            // IsZoneBridge covers BRIDGE/BOOST units — no audio output,
            // never a valid target (SoCo excludes them from visible
            // zones the same way, independently of Invisible).
            let invisible = m.tag_name().name() == "Satellite"
                || m.attribute("Invisible").map(|v| v == "1").unwrap_or(false)
                || m.attribute("IsZoneBridge").map(|v| v == "1").unwrap_or(false);
            members.push(ZoneMember {
                uuid: uuid.to_string(),
                zone_name: zone_name.to_string(),
                location: location.to_string(),
                invisible,
            });
        }
        groups.push(ZoneGroup {
            coordinator_uuid: coordinator.to_string(),
            members,
        });
    }
    groups
}

/// Strip the `uuid:` prefix a UPnP UDN carries; topology UUIDs don't
/// have it.
fn bare_uuid(udn: &str) -> &str {
    udn.strip_prefix("uuid:").unwrap_or(udn)
}

/// What a unit says about itself in the topology it serves from its own
/// address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelfReport {
    /// Listed: a standalone zone, or the coordinator of its group.
    Listed,
    /// Not listed: a bonded unit (stereo-pair slave, Sub, surround), a
    /// bridge, or a member of a group another unit coordinates.
    Hidden,
}

/// Where `uuid` stands in `groups` (the first group naming it), or None
/// if no group does.
fn standing(groups: &[ZoneGroup], uuid: &str) -> Option<SelfReport> {
    groups.iter().find_map(|g| {
        let m = g.members.iter().find(|m| m.uuid == uuid)?;
        // Grouped under a different coordinator → hidden; the group
        // entry (the coordinator) is the way to reach it.
        Some(if m.invisible || g.coordinator_uuid != uuid {
            SelfReport::Hidden
        } else {
            SelfReport::Listed
        })
    })
}

/// What a unit says about itself in the topology it serves from its own
/// address.
#[derive(Clone, Copy, Debug)]
pub struct OwnWord<'g> {
    /// Listed or hidden: its answer in this scan or, if it didn't answer,
    /// its last one.
    pub report: SelfReport,
    /// That topology, when the unit answered in this scan.
    pub topology: Option<&'g [ZoneGroup]>,
}

/// What a unit with UUID `uuid` says about itself in `own`, the topology
/// it serves. Absent from it, it doesn't claim to be hidden.
fn self_report(own: &[ZoneGroup], uuid: &str) -> SelfReport {
    standing(own, uuid).unwrap_or(SelfReport::Listed)
}

/// Name `r` (UUID `uuid`) after the group it coordinates in `groups`, if
/// it coordinates one there: its zone name, and the names of the group's
/// other visible members.
fn label(r: &mut Renderer, uuid: &str, groups: &[ZoneGroup]) {
    for g in groups.iter().filter(|g| g.coordinator_uuid == uuid) {
        if let Some(m) = g.members.iter().find(|m| m.uuid == uuid) {
            r.zone_name = Some(m.zone_name.clone());
        }
        r.group_members = g
            .members
            .iter()
            .filter(|m| m.uuid != uuid && !m.invisible)
            .map(|m| m.zone_name.clone())
            .collect();
    }
}

/// Fold zone-group topology into a freshly-discovered renderer list:
///
///  * invisible zones (stereo-pair slaves, Subs, surrounds) are removed;
///  * non-coordinator members of a group are removed — selecting one
///    would ungroup it, and the group is reachable via its coordinator;
///  * coordinators of multi-member groups get `zone_name` +
///    `group_members` filled in so the UI can present the group, from the
///    renderer's own topology when it served one this scan;
///  * a group whose coordinator's SSDP response got lost gets the
///    coordinator synthesized from the topology's Location URL (SSDP is
///    lossy; the members would otherwise all vanish from the list), if
///    that Location is a device address `reach` allows.
///
/// `groups` may come from any device, and naming another speaker's UUID
/// is all it takes to call it bonded or grouped. So a renderer is removed
/// only when `own_word` — what that renderer says about itself in the
/// topology it serves from its own address (or said in an earlier scan)
/// — agrees; a renderer that says otherwise, or nothing, stays listed.
///
/// `fetch` resolves a device-description URL to a Renderer (injected so
/// tests can run without a network).
pub fn apply_topology<'g>(
    renderers: &mut Vec<Renderer>,
    groups: &[ZoneGroup],
    reach: Reach<'_>,
    fetch: &DescribeFn<'_>,
    own_word: &dyn Fn(&Renderer) -> Option<OwnWord<'g>>,
) {
    // Synthesize coordinators whose SSDP response we missed but whose
    // group has at least one discovered member.
    for g in groups {
        let coordinator_known = renderers
            .iter()
            .any(|r| r.udn.as_deref().map(bare_uuid) == Some(g.coordinator_uuid.as_str()));
        if coordinator_known {
            continue;
        }
        let any_member_discovered = renderers.iter().any(|r| {
            r.udn
                .as_deref()
                .map(|u| g.members.iter().any(|m| m.uuid == bare_uuid(u)))
                .unwrap_or(false)
        });
        let coord_member = g.members.iter().find(|m| m.uuid == g.coordinator_uuid);
        if let (true, Some(cm)) = (any_member_discovered, coord_member) {
            if !reach.allows(&cm.location, || coordinator_location_of(cm)) {
                continue;
            }
            let fetched = fetch(&cm.location)
                .filter(|r| r.udn.as_deref().map(bare_uuid) == Some(g.coordinator_uuid.as_str()));
            if let Some(r) = fetched {
                info!(
                    "Sonos topology: coordinator {} missed SSDP; recovered from topology",
                    cm.zone_name
                );
                renderers.push(r);
            }
        }
    }

    renderers.retain(|r| {
        let Some(udn) = r.udn.as_deref() else {
            return true; // non-Sonos / no UDN — leave it alone
        };
        // Not in the topology at all (non-Sonos renderer), or listed in
        // it: kept. Hidden by it: only if the renderer says so too.
        standing(groups, bare_uuid(udn)) != Some(SelfReport::Hidden)
            || own_word(r).map(|w| w.report) != Some(SelfReport::Hidden)
    });

    for r in renderers.iter_mut() {
        let Some(udn) = r.udn.as_deref() else { continue };
        let uuid = bare_uuid(udn).to_string();
        match own_word(r).and_then(|w| w.topology) {
            // It was asked this scan: its names are its own.
            Some(own) => label(r, &uuid, own),
            // Only this scan's one topology names it, and any device on
            // the network can serve one, so these names can come from
            // another device. They only label the row: whether it is
            // listed is decided on its own word (above), and selecting it
            // re-reads its group from the speaker itself.
            None => label(r, &uuid, groups),
        }
    }
}

/// Discovery-time entry point. One topology, from the first of up to three
/// Sonos in the list whose topology URL is a device address `reach` allows that
/// answers, says which renderers might be hidden. Each renderer it would hide
/// is then asked for its own topology, at its own address: it is hidden only if
/// that agrees, and named from that answer when it gave one (see
/// [`apply_topology`]). The other rows are named from the one topology, so
/// their labels can come from another device. `memory` keeps each unit's last
/// answer for the scans where its query fails. No-op for Sonos-free networks;
/// fails open on any error.
pub fn annotate_with_topology(renderers: &mut Vec<Renderer>, reach: Reach<'_>, memory: &SelfReports) {
    let describe = |location: &str| describe_device(location, Some(reach.reported));
    annotate_with(renderers, reach, memory, &|url| fetch_zone_groups(url), &describe);
}

/// Topology sources tried per scan before the list is left as it is.
const TOPOLOGY_SOURCES_TRIED: usize = 3;
/// Most renderers asked for their own topology at once.
const PARALLEL_ASKS: usize = 8;

fn annotate_with(
    renderers: &mut Vec<Renderer>,
    reach: Reach<'_>,
    memory: &SelfReports,
    topology: &TopologyFn<'_>,
    describe: &DescribeFn<'_>,
) {
    let sources = renderers
        .iter()
        .filter_map(|r| {
            let url = r.zone_group_topology_control_url.as_deref()?;
            reach.allows(url, || topology_url_of(r)).then_some(url)
        })
        .take(TOPOLOGY_SOURCES_TRIED);
    let mut found = None;
    for url in sources {
        match topology(url) {
            Ok(groups) if !groups.is_empty() => {
                found = Some((url.to_string(), groups));
                break;
            }
            Ok(_) => debug!("Sonos topology from {} is empty or unreadable", shown(url)),
            Err(e) => debug!("Sonos topology query to {} failed: {:#}", shown(url), e),
        }
    }
    let Some((source, groups)) = found else {
        debug!("no Sonos topology this scan (list left as-is)");
        return;
    };
    let (source, groups) = (source.as_str(), groups.as_slice());

    // The renderers this topology would hide, and the topologies they
    // serve themselves.
    let doubted: Vec<&Renderer> = renderers
        .iter()
        .filter(|r| {
            r.udn
                .as_deref()
                .is_some_and(|udn| standing(groups, bare_uuid(udn)) == Some(SelfReport::Hidden))
        })
        .collect();
    let mut answers: Vec<(Option<UnitKey>, Option<Cow<'_, [ZoneGroup]>>)> = Vec::with_capacity(doubted.len());
    for chunk in doubted.chunks(PARALLEL_ASKS) {
        std::thread::scope(|s| {
            // A unit whose thread can't be started is asked right here.
            let asks: Vec<_> = chunk
                .iter()
                .map(|&r| {
                    std::thread::Builder::new()
                        .name("stream-to-speaker-sonos-ask".into())
                        .spawn_scoped(s, move || own_answer(r, reach, source, groups, topology))
                        .map_err(|_| own_answer(r, reach, source, groups, topology))
                })
                .collect();
            for (&r, ask) in chunk.iter().zip(asks) {
                let answer = match ask {
                    Ok(thread) => thread.join().unwrap_or_else(|e| std::panic::resume_unwind(e)),
                    Err(answer) => answer,
                };
                answers.push((unit_key(r), answer));
            }
        });
    }
    let present: Vec<UnitKey> = renderers.iter().filter_map(unit_key).collect();
    let reports = answers
        .iter()
        .map(|(key, own)| {
            let report = key.as_ref().zip(own.as_deref()).map(|((uuid, _), own)| self_report(own, uuid));
            (key.clone(), report)
        })
        .collect();
    let words = memory.settle(reports, &present);
    let fresh: HashMap<UnitKey, Cow<'_, [ZoneGroup]>> =
        answers.into_iter().filter_map(|(key, own)| Some((key?, own?))).collect();
    apply_topology(renderers, groups, reach, describe, &|r| {
        let key = unit_key(r)?;
        Some(OwnWord { report: *words.get(&key)?, topology: fresh.get(&key).map(|own| &**own) })
    });
}

/// The topology `r` serves at its own topology URL; None if it can't be
/// asked or doesn't answer. `source` and `groups` are this scan's
/// topology and where it came from (a renderer's own answer when the URL
/// is its own).
fn own_answer<'g>(
    r: &Renderer,
    reach: Reach<'_>,
    source: &str,
    groups: &'g [ZoneGroup],
    topology: &TopologyFn<'_>,
) -> Option<Cow<'g, [ZoneGroup]>> {
    let url = r.zone_group_topology_control_url.as_deref()?;
    if url == source {
        return Some(Cow::Borrowed(groups));
    }
    if !reach.allows(url, || topology_url_of(r)) {
        return None;
    }
    match topology(url) {
        Ok(own) if !own.is_empty() => Some(Cow::Owned(own)),
        Ok(_) => {
            debug!("{}: its own topology is empty or unreadable", shown(&r.friendly_name));
            None
        }
        Err(e) => {
            debug!("{}: its own topology query failed: {:#}", shown(&r.friendly_name), e);
            None
        }
    }
}

/// A unit as remembered across scans: its UUID at an address. Another
/// device claiming the same UUID elsewhere is another unit.
type UnitKey = (String, std::net::IpAddr);

fn unit_key(r: &Renderer) -> Option<UnitKey> {
    Some((bare_uuid(r.udn.as_deref()?).to_string(), r.ip))
}

/// Each unit's last good self-report, kept across scans so that a unit
/// whose own query fails once keeps its place: a bonded Sub or a grouped
/// member doesn't flicker into the list for a scan.
#[derive(Default)]
pub struct SelfReports {
    inner: std::sync::Mutex<SelfReportsInner>,
}

#[derive(Default)]
struct SelfReportsInner {
    scan: u64,
    /// Unit → (its last answer, the last scan it was present in).
    units: std::collections::HashMap<UnitKey, (SelfReport, u64)>,
}

/// A unit's report is forgotten once the unit has been missing from this
/// many scans in a row.
const REMEMBERED_SCANS: u64 = 3;
/// Most units remembered: far above any household, a bound for a network
/// full of fake ones.
const MAX_REMEMBERED: usize = 1024;

impl SelfReports {
    /// Fold one scan in. `answers` holds each asked unit's fresh answer
    /// (None: no answer); `present` lists every unit in the scan. Returns
    /// what each asked unit is taken to say: its fresh answer, otherwise
    /// its last one.
    fn settle(
        &self,
        answers: Vec<(Option<UnitKey>, Option<SelfReport>)>,
        present: &[UnitKey],
    ) -> std::collections::HashMap<UnitKey, SelfReport> {
        let mut inner = self.inner.lock().unwrap();
        inner.scan += 1;
        let scan = inner.scan;
        for k in present {
            if let Some(entry) = inner.units.get_mut(k) {
                entry.1 = scan;
            }
        }
        let mut words = std::collections::HashMap::new();
        for (key, answer) in answers {
            let Some(key) = key else { continue };
            match answer {
                Some(report) => {
                    if inner.units.len() < MAX_REMEMBERED || inner.units.contains_key(&key) {
                        inner.units.insert(key.clone(), (report, scan));
                    }
                    words.insert(key, report);
                }
                None => {
                    if let Some((report, _)) = inner.units.get(&key) {
                        words.insert(key, *report);
                    }
                }
            }
        }
        inner.units.retain(|_, (_, seen)| scan - *seen < REMEMBERED_SCANS);
        words
    }

    /// Forget every unit (discovery moved to another network).
    pub fn clear(&self) {
        *self.inner.lock().unwrap() = SelfReportsInner::default();
    }
}

/// Select-time safety net: the discovery list can be minutes stale, so
/// re-check the target's group membership right before streaming. If it
/// is now a non-coordinator member, redirect to the coordinator —
/// streaming to the member would silently ungroup it, and the coordinator
/// plays on the whole group (which is what a fresh list would have
/// offered). Also refreshes `group_members` on the (possibly unchanged)
/// coordinator so volume routing and the status banner are current.
///
/// `lookup` resolves a stable id (`uuid:RINCON_...`) against the current
/// discovery list; the coordinator falls back to a device-description
/// fetch via the topology's Location when it isn't in the list.
///
/// The speaker's own topology is asked wherever its topology URL points:
/// the user picked the speaker, and one added by address may be on
/// another subnet, a VLAN or this PC. The coordinator is another device,
/// named by that topology. For a speaker discovery found (`reach` given)
/// the topology is whatever that device serves, so the coordinator is
/// followed only to a device address `reach` allows, as discovery's own
/// requests are; otherwise the speaker is used as it is. For a speaker
/// added by address (`reach` None) the coordinator is followed wherever
/// the topology puts it, since its group may be wherever it is.
pub fn resolve_group_coordinator(
    renderer: Renderer,
    lookup: &dyn Fn(&str) -> Option<Renderer>,
    reach: Option<Reach<'_>>,
) -> Renderer {
    let describe = |location: &str| describe_device(location, reach.map(|r| r.reported));
    resolve_with(renderer, lookup, reach, &|url| fetch_zone_groups(url), &describe)
}

fn resolve_with(
    renderer: Renderer,
    lookup: &dyn Fn(&str) -> Option<Renderer>,
    reach: Option<Reach<'_>>,
    topology: &TopologyFn<'_>,
    describe: &DescribeFn<'_>,
) -> Renderer {
    let Some(zgt_url) = renderer.zone_group_topology_control_url.clone() else {
        return renderer; // not a Sonos
    };
    let Some(udn) = renderer.udn.clone() else {
        return renderer;
    };
    let uuid = bare_uuid(&udn).to_string();

    let groups = match topology(&zgt_url) {
        Ok(g) => g,
        Err(e) => {
            debug!("pre-stream topology check failed (using speaker as-is): {:#}", e);
            return renderer;
        }
    };
    let Some(group) = groups.iter().find(|g| g.members.iter().any(|m| m.uuid == uuid)) else {
        return renderer;
    };

    let mut target = if group.coordinator_uuid == uuid {
        renderer
    } else {
        let coord_id = format!("uuid:{}", group.coordinator_uuid);
        let coord_member = group.members.iter().find(|m| m.uuid == group.coordinator_uuid);
        let coord = verified_coordinator(group, &coord_id, reach, lookup, describe);
        match coord {
            Some(c) => {
                info!(
                    "Sonos: {} is grouped under {}; streaming to the group coordinator",
                    renderer.friendly_name,
                    coord_member.map(|m| m.zone_name.as_str()).unwrap_or("its coordinator"),
                );
                c
            }
            None => {
                debug!(
                    "Sonos: couldn't resolve coordinator {} — using {} directly",
                    group.coordinator_uuid, renderer.friendly_name
                );
                return renderer;
            }
        }
    };

    // Refresh group annotations from the just-fetched topology.
    if let Some(m) = group.members.iter().find(|m| m.uuid == group.coordinator_uuid) {
        target.zone_name = Some(m.zone_name.clone());
    }
    target.group_members = group
        .members
        .iter()
        .filter(|m| m.uuid != group.coordinator_uuid && !m.invisible)
        .map(|m| m.zone_name.clone())
        .collect();
    // Every other member's address (invisible ones too — a stereo-pair
    // slave or Sub is still a device on the LAN that may fetch), so a
    // privacy-mode grant for this session covers the whole group.
    target.group_member_addrs = group
        .members
        .iter()
        .filter(|m| m.uuid != group.coordinator_uuid)
        .filter_map(|m| location_host_addr(&m.location))
        .collect();
    target
}

/// The group's coordinator as a Renderer. The topology came from the
/// speaker the user picked, so the coordinator is trusted only at the
/// address that topology gives for it: a discovery entry claiming the
/// coordinator's UDN from another address is ignored in favour of a
/// fetch from the topology `Location`, whose description must carry the
/// coordinator's UUID. With `reach` (a speaker discovery found), that
/// address must be one `reach` allows, and a host name in the `Location`
/// is not looked up.
fn verified_coordinator(
    group: &ZoneGroup,
    coord_id: &str,
    reach: Option<Reach<'_>>,
    lookup: &dyn Fn(&str) -> Option<Renderer>,
    fetch: &DescribeFn<'_>,
) -> Option<Renderer> {
    let m = group.members.iter().find(|m| m.uuid == group.coordinator_uuid)?;
    let expected = match reach {
        Some(reach) => reach.address(&m.location, || coordinator_location_of(m))?,
        None => location_host_addr(&m.location)?,
    };
    lookup(coord_id)
        .filter(|c| c.ip == expected || c.host_addrs.contains(&expected))
        .or_else(|| {
            fetch(&m.location)
                .filter(|c| c.udn.as_deref().map(bare_uuid) == Some(group.coordinator_uuid.as_str()))
        })
}

/// The address behind a topology `Location` URL: a literal IP, or the
/// first address a hostname resolves to. `None` if unparseable.
pub(crate) fn location_host_addr(location: &str) -> Option<std::net::IpAddr> {
    let url = url::Url::parse(location).ok()?;
    let host = url.host_str()?;
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return Some(ip);
    }
    use std::net::ToSocketAddrs;
    (host, url.port().unwrap_or(1400))
        .to_socket_addrs()
        .ok()?
        .next()
        .map(|sa| sa.ip())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn location_host_addr_parses_literal_ips_and_rejects_garbage() {
        assert_eq!(
            location_host_addr("http://192.168.1.51:1400/xml/device_description.xml"),
            Some("192.168.1.51".parse().unwrap())
        );
        assert_eq!(location_host_addr("not a url"), None);
        assert_eq!(location_host_addr("http:///nohost"), None);
    }
    use std::net::{IpAddr, Ipv4Addr};

    use crate::discovery_net::LanTargets;

    /// The PC is 192.168.1.5/24, the network the test speakers are on.
    fn home_lan() -> LanTargets {
        LanTargets::from_networks([(Ipv4Addr::new(192, 168, 1, 5), 24)])
    }

    /// Discovery on that network, with nothing reported yet.
    struct Home {
        lan: LanTargets,
        reported: Reported,
    }

    impl Home {
        fn reach(&self) -> Reach<'_> {
            Reach { lan: &self.lan, reported: &self.reported }
        }
    }

    fn home() -> Home {
        Home { lan: home_lan(), reported: Reported::default() }
    }

    /// A genuine household: every renderer says about itself what
    /// `groups` says about it (in an earlier scan: no topology of its own
    /// to label it with).
    fn agreeing(groups: &[ZoneGroup]) -> impl Fn(&Renderer) -> Option<OwnWord<'static>> + '_ {
        move |r| {
            let report = standing(groups, bare_uuid(r.udn.as_deref()?))?;
            Some(OwnWord { report, topology: None })
        }
    }

    fn renderer(udn: &str, name: &str) -> Renderer {
        Renderer {
            friendly_name: name.to_string(),
            ip: IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)),
            base_url: "http://192.168.1.10:1400".into(),
            av_transport_control_url: "http://192.168.1.10:1400/MediaRenderer/AVTransport/Control"
                .into(),
            rendering_control_control_url:
                "http://192.168.1.10:1400/MediaRenderer/RenderingControl/Control".into(),
            rendering_control_event_url:
                "http://192.168.1.10:1400/MediaRenderer/RenderingControl/Event".into(),
            av_transport_event_url: None,
            udn: Some(udn.to_string()),
            zone_group_topology_control_url: Some(
                "http://192.168.1.10:1400/ZoneGroupTopology/Control".into(),
            ),
            group_rendering_control_control_url: Some(
                "http://192.168.1.10:1400/MediaRenderer/GroupRenderingControl/Control".into(),
            ),
            group_rendering_control_event_url: Some(
                "http://192.168.1.10:1400/MediaRenderer/GroupRenderingControl/Event".into(),
            ),
            zone_name: None,
            group_members: Vec::new(),
            host_addrs: Vec::new(),
            group_member_addrs: Vec::new(),
            model_name: None,
            udn_conflict: false,
        }
    }

    const SAMPLE: &str = r#"<ZoneGroupState><ZoneGroups>
<ZoneGroup Coordinator="RINCON_AAA" ID="RINCON_AAA:42">
  <ZoneGroupMember UUID="RINCON_AAA" Location="http://192.168.1.10:1400/xml/device_description.xml" ZoneName="Living Room">
    <Satellite UUID="RINCON_SAT" Location="http://192.168.1.13:1400/xml/device_description.xml" ZoneName="Living Room" Invisible="1"/>
  </ZoneGroupMember>
  <ZoneGroupMember UUID="RINCON_BBB" Location="http://192.168.1.11:1400/xml/device_description.xml" ZoneName="Kitchen"/>
</ZoneGroup>
<ZoneGroup Coordinator="RINCON_CCC" ID="RINCON_CCC:7">
  <ZoneGroupMember UUID="RINCON_CCC" Location="http://192.168.1.12:1400/xml/device_description.xml" ZoneName="Bedroom"/>
</ZoneGroup>
</ZoneGroups></ZoneGroupState>"#;

    #[test]
    fn parses_groups_members_satellites() {
        let groups = parse_zone_group_state(SAMPLE);
        assert_eq!(groups.len(), 2);
        let g = &groups[0];
        assert_eq!(g.coordinator_uuid, "RINCON_AAA");
        assert_eq!(g.members.len(), 3); // coordinator + satellite + kitchen
        assert!(g.members.iter().any(|m| m.uuid == "RINCON_SAT" && m.invisible));
        assert_eq!(g.visible_member_names(), vec!["Living Room", "Kitchen"]);
        assert_eq!(groups[1].members.len(), 1);
    }

    #[test]
    fn hides_members_and_satellites_annotates_coordinator() {
        let groups = parse_zone_group_state(SAMPLE);
        let mut renderers = vec![
            renderer("uuid:RINCON_AAA", "Living Room - Sonos One"),
            renderer("uuid:RINCON_BBB", "Kitchen - Sonos One"),
            renderer("uuid:RINCON_SAT", "Living Room - Sonos One (R)"),
            renderer("uuid:RINCON_CCC", "Bedroom - Sonos One"),
        ];
        apply_topology(&mut renderers, &groups, home().reach(), &|_| None, &agreeing(&groups));
        let names: Vec<&str> = renderers.iter().map(|r| r.friendly_name.as_str()).collect();
        assert_eq!(names, vec!["Living Room - Sonos One", "Bedroom - Sonos One"]);
        let coord = &renderers[0];
        assert_eq!(coord.zone_name.as_deref(), Some("Living Room"));
        assert_eq!(coord.group_members, vec!["Kitchen"]);
        assert!(coord.is_group());
        // Standalone zone: no group annotations.
        let solo = &renderers[1];
        assert!(solo.group_members.is_empty());
        assert!(!solo.is_group());
    }

    #[test]
    fn bridge_units_are_invisible() {
        // BOOST/BRIDGE: its own single-member group, flagged
        // IsZoneBridge="1" (SoCo excludes these from visible zones too).
        let xml = r#"<ZoneGroupState><ZoneGroups>
<ZoneGroup Coordinator="RINCON_BRIDGE" ID="RINCON_BRIDGE:1">
  <ZoneGroupMember UUID="RINCON_BRIDGE" Location="http://192.168.1.20:1400/xml/device_description.xml" ZoneName="BOOST" IsZoneBridge="1"/>
</ZoneGroup>
</ZoneGroups></ZoneGroupState>"#;
        let groups = parse_zone_group_state(xml);
        assert!(groups[0].members[0].invisible);
        let mut renderers = vec![renderer("uuid:RINCON_BRIDGE", "BOOST")];
        apply_topology(&mut renderers, &groups, home().reach(), &|_| None, &agreeing(&groups));
        assert!(renderers.is_empty());
    }

    #[test]
    fn parses_pre_10_1_shape_without_zonegroups_wrapper() {
        // Pre-10.1 S1 firmware: ZoneGroup elements sit directly under
        // ZoneGroupState, no <ZoneGroups> (SoCo's compatibility case).
        let xml = r#"<ZoneGroupState>
<ZoneGroup Coordinator="RINCON_AAA" ID="RINCON_AAA:1">
  <ZoneGroupMember UUID="RINCON_AAA" Location="http://192.168.1.10:1400/xml/device_description.xml" ZoneName="Den"/>
</ZoneGroup>
</ZoneGroupState>"#;
        let groups = parse_zone_group_state(xml);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].members[0].zone_name, "Den");
    }

    #[test]
    fn rejects_nested_groups_without_copying_descendant_members() {
        let xml = r#"<ZoneGroupState><ZoneGroups>
<ZoneGroup Coordinator="OUTER">
  <ZoneGroup Coordinator="INNER">
    <ZoneGroupMember UUID="INNER" ZoneName="Kitchen" Location="http://speaker/desc.xml"/>
  </ZoneGroup>
</ZoneGroup>
</ZoneGroups></ZoneGroupState>"#;
        assert!(parse_zone_group_state(xml).is_empty());
    }

    #[test]
    fn rejects_topologies_over_the_member_budget() {
        let mut xml = String::from(
            "<ZoneGroupState><ZoneGroups><ZoneGroup Coordinator=\"RINCON_A\">",
        );
        for i in 0..=1024 {
            xml.push_str(&format!("<ZoneGroupMember UUID=\"RINCON_{i}\"/>"));
        }
        xml.push_str("</ZoneGroup></ZoneGroups></ZoneGroupState>");
        assert!(parse_zone_group_state(&xml).is_empty());
    }

    #[test]
    fn non_sonos_renderers_pass_through() {
        let groups = parse_zone_group_state(SAMPLE);
        let mut plain = renderer("uuid:other-device", "AVR");
        plain.zone_group_topology_control_url = None;
        let mut renderers = vec![plain];
        apply_topology(&mut renderers, &groups, home().reach(), &|_| None, &agreeing(&groups));
        assert_eq!(renderers.len(), 1);
    }

    #[test]
    fn synthesizes_missing_coordinator() {
        let groups = parse_zone_group_state(SAMPLE);
        // Only the Kitchen member answered SSDP; the coordinator's
        // response got lost.
        let mut renderers = vec![renderer("uuid:RINCON_BBB", "Kitchen - Sonos One")];
        let fetched = std::cell::RefCell::new(Vec::<String>::new());
        let fetch = |loc: &str| {
            fetched.borrow_mut().push(loc.to_string());
            Some(renderer("uuid:RINCON_AAA", "Living Room - Sonos One"))
        };
        apply_topology(&mut renderers, &groups, home().reach(), &fetch, &agreeing(&groups));
        assert_eq!(
            fetched.borrow().as_slice(),
            &["http://192.168.1.10:1400/xml/device_description.xml"]
        );
        assert_eq!(renderers.len(), 1);
        assert_eq!(renderers[0].udn.as_deref(), Some("uuid:RINCON_AAA"));
        assert_eq!(renderers[0].group_members, vec!["Kitchen"]);
    }

    #[test]
    fn synthesized_coordinator_must_carry_the_coordinator_uuid() {
        let groups = parse_zone_group_state(SAMPLE);
        let mut renderers = vec![renderer("uuid:RINCON_BBB", "Kitchen - Sonos One")];
        let impostor = |_: &str| Some(renderer("uuid:RINCON_ZZZ", "Impostor"));
        apply_topology(&mut renderers, &groups, home().reach(), &impostor, &agreeing(&groups));
        assert!(renderers.iter().all(|r| r.udn.as_deref() != Some("uuid:RINCON_ZZZ")));
    }

    /// A topology whose coordinator (missed by SSDP) sits at `location`,
    /// with the discovered Kitchen speaker as its member.
    fn group_with_coordinator_at(location: &str) -> Vec<ZoneGroup> {
        parse_zone_group_state(&format!(
            r#"<ZoneGroups><ZoneGroup Coordinator="RINCON_AAA">
<ZoneGroupMember UUID="RINCON_AAA" Location="{location}" ZoneName="Living Room"/>
<ZoneGroupMember UUID="RINCON_BBB" Location="http://192.168.1.11:1400/xml/device_description.xml" ZoneName="Kitchen"/>
</ZoneGroup></ZoneGroups>"#
        ))
    }

    #[test]
    fn coordinator_is_synthesized_only_from_an_address_on_our_networks() {
        let home = home();
        for (i, location) in [
            "http://127.0.0.1:8080/admin",
            // This PC's own address.
            "http://192.168.1.5:1400/xml/device_description.xml",
            "http://10.9.9.9:1400/xml/device_description.xml",
            "http://router.local/xml/device_description.xml",
        ]
        .into_iter()
        .enumerate()
        {
            let groups = group_with_coordinator_at(location);
            let fetched = std::cell::RefCell::new(Vec::<String>::new());
            let fetch = |loc: &str| {
                fetched.borrow_mut().push(loc.to_string());
                Some(renderer("uuid:RINCON_AAA", "Living Room - Sonos One"))
            };
            // Every scan: never fetched, and in the log once.
            for _ in 0..2 {
                let mut renderers = vec![renderer("uuid:RINCON_BBB", "Kitchen - Sonos One")];
                apply_topology(&mut renderers, &groups, home.reach(), &fetch, &agreeing(&groups));
                assert!(renderers.iter().all(|r| r.udn.as_deref() != Some("uuid:RINCON_AAA")));
            }
            assert!(fetched.borrow().is_empty(), "{location} was fetched");
            assert_eq!(home.reported.len(), i + 1, "{location}");
        }
        // On our network: fetched.
        let groups = group_with_coordinator_at("http://192.168.1.10:1400/xml/device_description.xml");
        let mut renderers = vec![renderer("uuid:RINCON_BBB", "Kitchen - Sonos One")];
        let fetch = |_: &str| Some(renderer("uuid:RINCON_AAA", "Living Room - Sonos One"));
        apply_topology(&mut renderers, &groups, home.reach(), &fetch, &agreeing(&groups));
        assert_eq!(renderers[0].udn.as_deref(), Some("uuid:RINCON_AAA"));
    }

    #[test]
    fn topology_is_queried_only_at_an_address_on_our_networks() {
        let mut stray = renderer("uuid:RINCON_ZZZ", "Stray");
        stray.zone_group_topology_control_url = Some("http://127.0.0.1:1400/ZoneGroupTopology/Control".into());
        let mut on_this_pc = renderer("uuid:RINCON_YYY", "On this PC");
        on_this_pc.zone_group_topology_control_url = Some("http://192.168.1.5:1400/ZoneGroupTopology/Control".into());
        let mut renderers = vec![stray, on_this_pc, renderer("uuid:RINCON_CCC", "Bedroom - Sonos One")];
        let asked = std::sync::Mutex::new(Vec::<String>::new());
        let topology = |url: &str| {
            asked.lock().unwrap().push(url.to_string());
            Ok(parse_zone_group_state(SAMPLE))
        };
        let home = home();
        annotate_with(&mut renderers, home.reach(), &SelfReports::default(), &topology, &|_| None);
        assert_eq!(*asked.lock().unwrap(), ["http://192.168.1.10:1400/ZoneGroupTopology/Control"]);
        // Both refusals are in the log.
        assert_eq!(home.reported.len(), 2);
        // Nothing on our networks to ask: nothing asked, list left alone.
        let mut stray = renderer("uuid:RINCON_ZZZ", "Stray");
        stray.zone_group_topology_control_url = Some("http://8.8.8.8/ZoneGroupTopology/Control".into());
        let mut renderers = vec![stray];
        annotate_with(&mut renderers, home.reach(), &SelfReports::default(), &|_| panic!("queried"), &|_| None);
        assert_eq!(renderers.len(), 1);
    }

    /// A speaker at `ip` with every service URL on that address.
    fn renderer_at(udn: &str, name: &str, ip: &str) -> Renderer {
        let mut r = renderer(udn, name);
        let base = format!("http://{ip}:1400");
        r.ip = ip.parse().unwrap();
        r.host_addrs = vec![r.ip];
        r.av_transport_control_url = format!("{base}/MediaRenderer/AVTransport/Control");
        r.rendering_control_control_url = format!("{base}/MediaRenderer/RenderingControl/Control");
        r.rendering_control_event_url = format!("{base}/MediaRenderer/RenderingControl/Event");
        r.zone_group_topology_control_url = Some(format!("{base}/ZoneGroupTopology/Control"));
        r.group_rendering_control_control_url = Some(format!("{base}/MediaRenderer/GroupRenderingControl/Control"));
        r.group_rendering_control_event_url = Some(format!("{base}/MediaRenderer/GroupRenderingControl/Event"));
        r.base_url = base;
        r
    }

    /// What a hostile device serves: the two real speakers named as a
    /// bonded unit and a member of a group it coordinates.
    const HOSTILE: &str = r#"<ZoneGroups><ZoneGroup Coordinator="RINCON_GHOST">
<ZoneGroupMember UUID="RINCON_GHOST" Location="http://192.168.1.66:1400/xml/device_description.xml" ZoneName="Ghost">
  <Satellite UUID="RINCON_AAA" Location="http://192.168.1.10:1400/xml/device_description.xml" ZoneName="Living Room" Invisible="1"/>
</ZoneGroupMember>
<ZoneGroupMember UUID="RINCON_BBB" Location="http://192.168.1.11:1400/xml/device_description.xml" ZoneName="Kitchen"/>
</ZoneGroup></ZoneGroups>"#;

    /// What the two real speakers serve: two standalone zones.
    const TWO_ROOMS: &str = r#"<ZoneGroups>
<ZoneGroup Coordinator="RINCON_AAA"><ZoneGroupMember UUID="RINCON_AAA" Location="http://192.168.1.10:1400/xml/device_description.xml" ZoneName="Living Room"/></ZoneGroup>
<ZoneGroup Coordinator="RINCON_BBB"><ZoneGroupMember UUID="RINCON_BBB" Location="http://192.168.1.11:1400/xml/device_description.xml" ZoneName="Kitchen"/></ZoneGroup>
</ZoneGroups>"#;

    fn names(renderers: &[Renderer]) -> Vec<&str> {
        renderers.iter().map(|r| r.friendly_name.as_str()).collect()
    }

    #[test]
    fn another_devices_topology_cannot_hide_a_speaker() {
        let hostile = parse_zone_group_state(HOSTILE);
        let own = parse_zone_group_state(TWO_ROOMS);
        let real = || {
            vec![
                renderer_at("uuid:RINCON_AAA", "Living Room - Sonos One", "192.168.1.10"),
                renderer_at("uuid:RINCON_BBB", "Kitchen - Sonos One", "192.168.1.11"),
            ]
        };
        // Each speaker says it is a standalone zone: both stay.
        let mut renderers = real();
        apply_topology(&mut renderers, &hostile, home().reach(), &|_| None, &agreeing(&own));
        assert_eq!(names(&renderers), ["Living Room - Sonos One", "Kitchen - Sonos One"]);
        // Nothing heard from them: both stay.
        let mut renderers = real();
        apply_topology(&mut renderers, &hostile, home().reach(), &|_| None, &|_| None);
        assert_eq!(renderers.len(), 2);
        // Only their own word hides them.
        let mut renderers = real();
        apply_topology(&mut renderers, &hostile, home().reach(), &|_| None, &agreeing(&hostile));
        assert!(renderers.is_empty());
    }

    #[test]
    fn units_are_asked_at_their_own_address() {
        // The hostile device answers the search first, so its topology is
        // the one that labels the list; the real speakers are asked
        // themselves before anything is hidden.
        let mut renderers = vec![
            renderer_at("uuid:RINCON_GHOST", "Ghost", "192.168.1.66"),
            renderer_at("uuid:RINCON_AAA", "Living Room - Sonos One", "192.168.1.10"),
            renderer_at("uuid:RINCON_BBB", "Kitchen - Sonos One", "192.168.1.11"),
        ];
        let asked = std::sync::Mutex::new(Vec::<String>::new());
        let topology = |url: &str| {
            asked.lock().unwrap().push(url.to_string());
            Ok(parse_zone_group_state(if url.contains("192.168.1.66") { HOSTILE } else { TWO_ROOMS }))
        };
        annotate_with(&mut renderers, home().reach(), &SelfReports::default(), &topology, &|_| None);
        assert_eq!(names(&renderers), ["Ghost", "Living Room - Sonos One", "Kitchen - Sonos One"]);
        let mut asked = asked.into_inner().unwrap();
        assert_eq!(asked.remove(0), "http://192.168.1.66:1400/ZoneGroupTopology/Control");
        asked.sort();
        assert_eq!(
            asked,
            [
                "http://192.168.1.10:1400/ZoneGroupTopology/Control",
                "http://192.168.1.11:1400/ZoneGroupTopology/Control"
            ]
        );
    }

    /// What the two real speakers serve once grouped: Living Room
    /// coordinating Kitchen.
    const GROUPED: &str = r#"<ZoneGroups><ZoneGroup Coordinator="RINCON_AAA">
<ZoneGroupMember UUID="RINCON_AAA" Location="http://192.168.1.10:1400/xml/device_description.xml" ZoneName="Living Room"/>
<ZoneGroupMember UUID="RINCON_BBB" Location="http://192.168.1.11:1400/xml/device_description.xml" ZoneName="Kitchen"/>
</ZoneGroup></ZoneGroups>"#;

    #[test]
    fn a_row_asked_this_scan_is_named_from_its_own_answer() {
        // The hostile device answers first and calls Living Room a bonded
        // unit of its own. Asked, Living Room says it coordinates Kitchen,
        // and Kitchen that it is Living Room's member.
        let mut renderers = vec![
            renderer_at("uuid:RINCON_GHOST", "Ghost", "192.168.1.66"),
            renderer_at("uuid:RINCON_AAA", "Living Room - Sonos One", "192.168.1.10"),
            renderer_at("uuid:RINCON_BBB", "Kitchen - Sonos One", "192.168.1.11"),
        ];
        let topology = |url: &str| Ok(parse_zone_group_state(if url.contains("192.168.1.66") { HOSTILE } else { GROUPED }));
        annotate_with(&mut renderers, home().reach(), &SelfReports::default(), &topology, &|_| None);
        let rows: Vec<String> = renderers.iter().map(|r| r.display_name()).collect();
        // The hostile device names only its own row.
        assert_eq!(rows, ["Ghost + Kitchen", "Living Room + Kitchen"]);
    }

    /// What a device claiming the Living Room speaker's ID serves: that ID
    /// as a bonded unit of a group it coordinates.
    const CLAIMANT: &str = r#"<ZoneGroups><ZoneGroup Coordinator="RINCON_GHOST">
<ZoneGroupMember UUID="RINCON_GHOST" Location="http://192.168.1.67:1400/xml/device_description.xml" ZoneName="Ghost">
  <Satellite UUID="RINCON_AAA" Location="http://192.168.1.66:1400/xml/device_description.xml" ZoneName="Living Room" Invisible="1"/>
</ZoneGroupMember>
</ZoneGroup></ZoneGroups>"#;

    #[test]
    fn a_device_claiming_a_speakers_id_never_decides_for_it() {
        let memory = SelfReports::default();
        let scan = |real_answers: bool| {
            // The claimant at .66 answers the search first.
            let mut renderers = vec![
                renderer_at("uuid:RINCON_AAA", "Impostor", "192.168.1.66"),
                renderer_at("uuid:RINCON_AAA", "Living Room - Sonos One", "192.168.1.10"),
            ];
            let topology = |url: &str| {
                if url.contains("192.168.1.66") {
                    Ok(parse_zone_group_state(CLAIMANT))
                } else if real_answers {
                    Ok(parse_zone_group_state(TWO_ROOMS))
                } else {
                    Err(anyhow::anyhow!("timed out"))
                }
            };
            annotate_with(&mut renderers, home().reach(), &memory, &topology, &|_| None);
            renderers.iter().map(|r| (r.friendly_name.clone(), r.ip.to_string())).collect::<Vec<_>>()
        };
        let real = vec![("Living Room - Sonos One".to_string(), "192.168.1.10".to_string())];
        // The real speaker says it is a standalone zone: it stays, and the
        // claimant, hidden on its own word, goes.
        assert_eq!(scan(true), real);
        // Next scan the real one doesn't answer. It is judged by what it
        // said itself, never by the claimant's remembered answer.
        assert_eq!(scan(false), real);
    }

    /// The SAMPLE household as discovered: coordinator, satellite, member
    /// and a standalone zone, each at its own address.
    fn sample_household() -> Vec<Renderer> {
        vec![
            renderer_at("uuid:RINCON_AAA", "Living Room - Sonos One", "192.168.1.10"),
            renderer_at("uuid:RINCON_SAT", "Living Room - Sonos One (R)", "192.168.1.13"),
            renderer_at("uuid:RINCON_BBB", "Kitchen - Sonos One", "192.168.1.11"),
            renderer_at("uuid:RINCON_CCC", "Bedroom - Sonos One", "192.168.1.12"),
        ]
    }

    #[test]
    fn a_genuine_household_still_collapses_to_its_groups() {
        let mut renderers = sample_household();
        let asked = std::sync::Mutex::new(Vec::<String>::new());
        let topology = |url: &str| {
            asked.lock().unwrap().push(url.to_string());
            Ok(parse_zone_group_state(SAMPLE))
        };
        annotate_with(&mut renderers, home().reach(), &SelfReports::default(), &topology, &|_| None);
        assert_eq!(names(&renderers), ["Living Room - Sonos One", "Bedroom - Sonos One"]);
        assert_eq!(renderers[0].display_name(), "Living Room + Kitchen");
        // The topology's source, then the two units it would hide.
        assert_eq!(asked.lock().unwrap().len(), 3);
    }

    #[test]
    fn a_unit_that_cannot_be_asked_keeps_its_last_answer() {
        let memory = SelfReports::default();
        let everyone = |_: &str| Ok(parse_zone_group_state(SAMPLE));
        let only_the_coordinator = |url: &str| {
            if url.contains("192.168.1.10:") {
                Ok(parse_zone_group_state(SAMPLE))
            } else {
                Err(anyhow::anyhow!("timed out"))
            }
        };
        let mut renderers = sample_household();
        annotate_with(&mut renderers, home().reach(), &memory, &everyone, &|_| None);
        assert_eq!(names(&renderers), ["Living Room - Sonos One", "Bedroom - Sonos One"]);
        // Next scan the satellite and the member don't answer: they stay
        // hidden on what they said before.
        let mut renderers = sample_household();
        annotate_with(&mut renderers, home().reach(), &memory, &only_the_coordinator, &|_| None);
        assert_eq!(names(&renderers), ["Living Room - Sonos One", "Bedroom - Sonos One"]);
        // Never heard from: listed.
        let mut renderers = sample_household();
        annotate_with(&mut renderers, home().reach(), &SelfReports::default(), &only_the_coordinator, &|_| None);
        assert_eq!(renderers.len(), 4);
    }

    #[test]
    fn self_reports_are_kept_per_unit_and_forgotten_after_absence() {
        use SelfReport::*;
        let memory = SelfReports::default();
        let real: UnitKey = ("RINCON_SAT".into(), "192.168.1.13".parse().unwrap());
        // Another device claiming the same UUID at another address.
        let other: UnitKey = ("RINCON_SAT".into(), "192.168.1.66".parse().unwrap());
        let both = [real.clone(), other.clone()];
        let w = memory.settle(vec![(Some(real.clone()), Some(Hidden)), (Some(other.clone()), Some(Listed))], &both);
        assert_eq!((w[&real], w[&other]), (Hidden, Listed));
        // Neither answers: each keeps its own.
        let w = memory.settle(vec![(Some(real.clone()), None), (Some(other.clone()), None)], &both);
        assert_eq!((w[&real], w[&other]), (Hidden, Listed));
        // A fresh answer replaces the remembered one.
        let w = memory.settle(vec![(Some(real.clone()), Some(Listed))], &[real.clone()]);
        assert_eq!(w[&real], Listed);
        // Missing from enough scans in a row: forgotten.
        for _ in 0..REMEMBERED_SCANS {
            memory.settle(Vec::new(), &[]);
        }
        let w = memory.settle(vec![(Some(real.clone()), None)], &[real.clone()]);
        assert!(w.is_empty());
        // Discovery moving to another network forgets everything.
        memory.settle(vec![(Some(real.clone()), Some(Hidden))], &[real.clone()]);
        memory.clear();
        assert!(memory.settle(vec![(Some(real.clone()), None)], &[real.clone()]).is_empty());
    }

    #[test]
    fn topology_comes_from_the_next_speaker_when_one_fails() {
        let mut renderers = vec![
            renderer_at("uuid:RINCON_AAA", "Living Room - Sonos One", "192.168.1.10"),
            renderer_at("uuid:RINCON_CCC", "Bedroom - Sonos One", "192.168.1.12"),
        ];
        let topology = |url: &str| {
            if url.contains("192.168.1.10:") {
                Err(anyhow::anyhow!("connection refused"))
            } else {
                Ok(parse_zone_group_state(SAMPLE))
            }
        };
        annotate_with(&mut renderers, home().reach(), &SelfReports::default(), &topology, &|_| None);
        assert_eq!(renderers[0].display_name(), "Living Room + Kitchen");
    }

    #[test]
    fn coordinator_is_trusted_only_at_the_topology_address() {
        let groups = parse_zone_group_state(SAMPLE);
        let g = &groups[0]; // coordinator RINCON_AAA at 192.168.1.10
        let mut impostor = renderer("uuid:RINCON_AAA", "Impostor");
        impostor.ip = "192.168.1.66".parse().unwrap();
        let real = renderer("uuid:RINCON_AAA", "Living Room");

        // Discovery entry at the wrong address: ignored, real one fetched.
        let imp = impostor.clone();
        let got = verified_coordinator(g, "uuid:RINCON_AAA", None, &move |_| Some(imp.clone()), &|_| {
            Some(real.clone())
        })
        .unwrap();
        assert_eq!(got.friendly_name, "Living Room");

        // Discovery entry at the topology address: used directly.
        let real2 = renderer("uuid:RINCON_AAA", "Living Room");
        let got = verified_coordinator(g, "uuid:RINCON_AAA", None, &move |_| Some(real2.clone()), &|_| None).unwrap();
        assert_eq!(got.ip, "192.168.1.10".parse::<IpAddr>().unwrap());

        // Fetched description with another UUID: rejected.
        let imp = impostor.clone();
        assert!(verified_coordinator(g, "uuid:RINCON_AAA", None, &|_| None, &move |_| {
            let mut r = imp.clone();
            r.udn = Some("uuid:RINCON_ZZZ".into());
            Some(r)
        })
        .is_none());
    }

    /// Select the Kitchen speaker, which answers that it is a member of
    /// the group the coordinator at `location` leads. What is streamed to,
    /// and the description URLs fetched on the way.
    fn select_kitchen_grouped_under(location: &str, reach: Option<Reach<'_>>) -> (Renderer, Vec<String>) {
        let groups = group_with_coordinator_at(location);
        let fetched = std::sync::Mutex::new(Vec::<String>::new());
        let describe = |loc: &str| {
            fetched.lock().unwrap().push(loc.to_string());
            Some(renderer("uuid:RINCON_AAA", "Living Room - Sonos One"))
        };
        let kitchen = renderer_at("uuid:RINCON_BBB", "Kitchen - Sonos One", "192.168.1.11");
        let target = resolve_with(kitchen, &|_| None, reach, &|_| Ok(groups.clone()), &describe);
        (target, fetched.into_inner().unwrap())
    }

    #[test]
    fn a_discovered_speaker_is_redirected_only_to_a_coordinator_on_our_networks() {
        let home = home();
        for (i, location) in [
            "http://127.0.0.1:8080/admin",
            // This PC's own address.
            "http://192.168.1.5:1400/xml/device_description.xml",
            "http://10.9.9.9:1400/xml/device_description.xml",
            "http://router.local/xml/device_description.xml",
            "http://[::ffff:127.0.0.2]:1400/xml/device_description.xml",
        ]
        .into_iter()
        .enumerate()
        {
            // Each selection: not fetched, the speaker used as it is, and
            // in the log once.
            for _ in 0..2 {
                let (target, fetched) = select_kitchen_grouped_under(location, Some(home.reach()));
                assert!(fetched.is_empty(), "{location} was fetched");
                assert_eq!(target.udn.as_deref(), Some("uuid:RINCON_BBB"), "{location}");
            }
            assert_eq!(home.reported.len(), i + 1, "{location}");
        }
        // On our network: streamed to the coordinator.
        let location = "http://192.168.1.10:1400/xml/device_description.xml";
        let (target, fetched) = select_kitchen_grouped_under(location, Some(home.reach()));
        assert_eq!(fetched, [location]);
        assert_eq!(target.udn.as_deref(), Some("uuid:RINCON_AAA"));
        assert_eq!(target.group_members, ["Kitchen"]);
    }

    #[test]
    fn a_speaker_added_by_address_is_redirected_to_its_coordinator_anywhere() {
        for location in [
            "http://10.9.9.9:1400/xml/device_description.xml",
            "http://192.168.1.5:1400/xml/device_description.xml",
            "http://127.0.0.1:1400/xml/device_description.xml",
        ] {
            let (target, fetched) = select_kitchen_grouped_under(location, None);
            assert_eq!(fetched, [location]);
            assert_eq!(target.udn.as_deref(), Some("uuid:RINCON_AAA"), "{location}");
        }
    }

    #[test]
    fn non_ascii_zone_names_reach_the_list_intact() {
        // From the wire format (escaped topology inside the SOAP body) to
        // the names the list row, banner, tray, web UI and endpoint show.
        let resp = concat!(
            r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body>"#,
            r#"<u:GetZoneGroupStateResponse xmlns:u="urn:schemas-upnp-org:service:ZoneGroupTopology:1">"#,
            r#"<ZoneGroupState>&lt;ZoneGroupState&gt;&lt;ZoneGroups&gt;"#,
            r#"&lt;ZoneGroup Coordinator="RINCON_AAA" ID="RINCON_AAA:1"&gt;"#,
            r#"&lt;ZoneGroupMember UUID="RINCON_AAA" Location="http://192.168.1.10:1400/xml/device_description.xml" ZoneName="Salón"/&gt;"#,
            r#"&lt;ZoneGroupMember UUID="RINCON_BBB" Location="http://192.168.1.11:1400/xml/device_description.xml" ZoneName="Küche"/&gt;"#,
            r#"&lt;/ZoneGroup&gt;"#,
            r#"&lt;ZoneGroup Coordinator="RINCON_CCC" ID="RINCON_CCC:7"&gt;"#,
            r#"&lt;ZoneGroupMember UUID="RINCON_CCC" Location="http://192.168.1.12:1400/xml/device_description.xml" ZoneName="Emma’s Room"/&gt;"#,
            r#"&lt;ZoneGroupMember UUID="RINCON_DDD" Location="http://192.168.1.13:1400/xml/device_description.xml" ZoneName="Детская"/&gt;"#,
            r#"&lt;ZoneGroupMember UUID="RINCON_EEE" Location="http://192.168.1.14:1400/xml/device_description.xml" ZoneName="寝室"/&gt;"#,
            r#"&lt;/ZoneGroup&gt;"#,
            r#"&lt;/ZoneGroups&gt;&lt;/ZoneGroupState&gt;</ZoneGroupState>"#,
            r#"</u:GetZoneGroupStateResponse></s:Body></s:Envelope>"#,
        );
        let groups = parse_zone_group_state(&upnp::zone_group_state_from_response(resp).unwrap());
        assert_eq!(groups[1].visible_member_names(), vec!["Emma’s Room", "Детская", "寝室"]);
        let mut renderers = vec![
            renderer("uuid:RINCON_AAA", "192.168.1.10 - Sonos One"),
            renderer("uuid:RINCON_BBB", "192.168.1.11 - Sonos One"),
            renderer("uuid:RINCON_CCC", "192.168.1.12 - Sonos Era 100"),
            renderer("uuid:RINCON_DDD", "192.168.1.13 - Sonos Era 100"),
            renderer("uuid:RINCON_EEE", "192.168.1.14 - Sonos Era 100"),
        ];
        apply_topology(&mut renderers, &groups, home().reach(), &|_| None, &agreeing(&groups));
        let rows: Vec<String> = renderers.iter().map(|r| r.display_name()).collect();
        assert_eq!(rows, vec!["Salón + Küche", "Emma’s Room + 2"]);
        assert_eq!(renderers[1].group_members, vec!["Детская", "寝室"]);
    }

    #[test]
    fn display_name_formats() {
        let mut r = renderer("uuid:RINCON_AAA", "Living Room - Sonos One");
        assert_eq!(r.display_name(), "Living Room - Sonos One");
        r.zone_name = Some("Living Room".into());
        r.group_members = vec!["Kitchen".into()];
        assert_eq!(r.display_name(), "Living Room + Kitchen");
        r.group_members = vec!["Kitchen".into(), "Bedroom".into()];
        assert_eq!(r.display_name(), "Living Room + 2");
    }

    #[test]
    fn group_volume_urls_only_for_real_groups() {
        let mut r = renderer("uuid:RINCON_AAA", "Living Room");
        // Standalone: plain RenderingControl even though the device
        // advertises GroupRenderingControl.
        assert!(r.group_volume_control_url().is_none());
        assert!(r.volume_event_url().contains("/RenderingControl/"));
        r.group_members = vec!["Kitchen".into()];
        assert!(r
            .group_volume_control_url()
            .unwrap()
            .contains("/GroupRenderingControl/"));
        assert!(r.volume_event_url().contains("/GroupRenderingControl/"));
    }
}
