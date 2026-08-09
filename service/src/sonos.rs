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
//! untouched, and every network failure here fails open: worst case the
//! list looks like it did before this module existed.

use crate::ssdp::Renderer;
use crate::upnp;
use log::{debug, info};

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
    let doc = match roxmltree::Document::parse(xml.trim_start()) {
        Ok(d) => d,
        Err(e) => {
            debug!("ZoneGroupState parse failed: {}", e);
            return Vec::new();
        }
    };

    let mut groups = Vec::new();
    for g in doc
        .descendants()
        .filter(|n| n.tag_name().name() == "ZoneGroup")
    {
        let Some(coordinator) = g.attribute("Coordinator") else {
            continue;
        };
        let mut members = Vec::new();
        for m in g.descendants().filter(|n| {
            let name = n.tag_name().name();
            // Satellites are bonded sub-units nested inside a member;
            // fold them in as invisible members so their SSDP entries
            // get filtered like any other invisible zone.
            name == "ZoneGroupMember" || name == "Satellite"
        }) {
            let Some(uuid) = m.attribute("UUID") else {
                continue;
            };
            let invisible = m.tag_name().name() == "Satellite"
                || m.attribute("Invisible").map(|v| v == "1").unwrap_or(false);
            members.push(ZoneMember {
                uuid: uuid.to_string(),
                zone_name: m.attribute("ZoneName").unwrap_or("").to_string(),
                location: m.attribute("Location").unwrap_or("").to_string(),
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

/// Fold zone-group topology into a freshly-discovered renderer list:
///
///  * invisible zones (stereo-pair slaves, Subs, surrounds) are removed;
///  * non-coordinator members of a group are removed — selecting one
///    would ungroup it, and the group is reachable via its coordinator;
///  * coordinators of multi-member groups get `zone_name` +
///    `group_members` filled in so the UI can present the group;
///  * a group whose coordinator's SSDP response got lost gets the
///    coordinator synthesized from the topology's Location URL (SSDP is
///    lossy; the members would otherwise all vanish from the list).
///
/// `fetch` resolves a device-description URL to a Renderer (injected so
/// tests can run without a network).
pub fn apply_topology(
    renderers: &mut Vec<Renderer>,
    groups: &[ZoneGroup],
    fetch: &dyn Fn(&str) -> Option<Renderer>,
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
            if let Some(r) = fetch(&cm.location) {
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
        let uuid = bare_uuid(udn);
        for g in groups {
            if let Some(m) = g.members.iter().find(|m| m.uuid == uuid) {
                if m.invisible {
                    return false;
                }
                // Grouped under a different coordinator → hidden; the
                // group entry (the coordinator) is the way to reach it.
                return g.coordinator_uuid == uuid;
            }
        }
        true // not in the topology at all (non-Sonos renderer)
    });

    for r in renderers.iter_mut() {
        let Some(udn) = r.udn.as_deref() else { continue };
        let uuid = bare_uuid(udn).to_string();
        for g in groups {
            if g.coordinator_uuid != uuid {
                continue;
            }
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
}

/// Discovery-time entry point: query topology from the first Sonos in the
/// list and fold it in. No-op for Sonos-free networks; fails open on any
/// error.
pub fn annotate_with_topology(renderers: &mut Vec<Renderer>) {
    let Some(zgt_url) = renderers
        .iter()
        .find_map(|r| r.zone_group_topology_control_url.clone())
    else {
        return;
    };
    match fetch_zone_groups(&zgt_url) {
        Ok(groups) => {
            apply_topology(renderers, &groups, &|location| {
                crate::ssdp::fetch_and_parse_device(location, std::time::Duration::from_secs(3))
                    .ok()
            });
        }
        Err(e) => debug!("Sonos topology query failed (list left as-is): {:#}", e),
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
pub fn resolve_group_coordinator(
    renderer: Renderer,
    lookup: &dyn Fn(&str) -> Option<Renderer>,
) -> Renderer {
    let Some(zgt_url) = renderer.zone_group_topology_control_url.clone() else {
        return renderer; // not a Sonos
    };
    let Some(udn) = renderer.udn.clone() else {
        return renderer;
    };
    let uuid = bare_uuid(&udn).to_string();

    let groups = match fetch_zone_groups(&zgt_url) {
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
        let coord = lookup(&coord_id).or_else(|| {
            coord_member.and_then(|m| {
                crate::ssdp::fetch_and_parse_device(&m.location, std::time::Duration::from_secs(3))
                    .ok()
            })
        });
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
    target
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

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
        apply_topology(&mut renderers, &groups, &|_| None);
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
    fn non_sonos_renderers_pass_through() {
        let groups = parse_zone_group_state(SAMPLE);
        let mut plain = renderer("uuid:other-device", "AVR");
        plain.zone_group_topology_control_url = None;
        let mut renderers = vec![plain];
        apply_topology(&mut renderers, &groups, &|_| None);
        assert_eq!(renderers.len(), 1);
    }

    #[test]
    fn synthesizes_missing_coordinator() {
        let groups = parse_zone_group_state(SAMPLE);
        // Only the Kitchen member answered SSDP; the coordinator's
        // response got lost.
        let mut renderers = vec![renderer("uuid:RINCON_BBB", "Kitchen - Sonos One")];
        let fetched = std::cell::RefCell::new(Vec::<String>::new());
        apply_topology(&mut renderers, &groups, &|loc| {
            fetched.borrow_mut().push(loc.to_string());
            Some(renderer("uuid:RINCON_AAA", "Living Room - Sonos One"))
        });
        assert_eq!(
            fetched.borrow().as_slice(),
            &["http://192.168.1.10:1400/xml/device_description.xml"]
        );
        assert_eq!(renderers.len(), 1);
        assert_eq!(renderers[0].udn.as_deref(), Some("uuid:RINCON_AAA"));
        assert_eq!(renderers[0].group_members, vec!["Kitchen"]);
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
