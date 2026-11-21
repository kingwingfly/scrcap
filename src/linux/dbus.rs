//! D-Bus related

use std::time::Duration;

use dbus::{
    Message, Path,
    arg::{AppendAll, Arg, IterAppend, OwnedFd, PropMap, Variant, cast},
    blocking::{BlockingSender, Connection},
    message::MatchRule,
};

use crate::error::{CaptureError, PortalCall, PortalError, Result};

pub const SOURCE_TYPE_MONITOR: u32 = 1;
const CURSOR_MODE_EMBEDDED: u32 = 2;
const PORTAL_BUS_NAME: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const SCREENCAST: &str = "org.freedesktop.portal.ScreenCast";
const DBUS: &str = "org.freedesktop.DBus";
const CALL_TIMEOUT: Duration = Duration::from_secs(3);

/// What a granted ScreenCast session hands over.
pub struct Session {
    pub target: StreamTarget,
    pub fd: OwnedFd,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamTarget {
    NodeId(u32),
    Serial(u64),
}

fn stream_target(results: &PropMap) -> Option<StreamTarget> {
    let streams = &results.get("streams")?.0;
    if streams.signature() != Vec::<(u32, PropMap)>::signature()
        || streams.as_static_inner(1).is_some()
    {
        return None;
    }
    let stream = streams.as_static_inner(0)?;
    let props = cast::<PropMap>(stream.as_static_inner(1)?)?;
    if let Some(serial) = props.get("pipewire-serial") {
        return cast::<u64>(&serial.0).copied().map(StreamTarget::Serial);
    }
    // Older portals supply only a node ID. Zero is the core and u32::MAX means any node.
    let id = *cast::<u32>(stream.as_static_inner(0)?)?;
    (id != 0 && id != u32::MAX).then_some(StreamTarget::NodeId(id))
}

pub struct DbusScreen {
    connection: Connection,
    response: MatchRule<'static>,
    owner_changed: MatchRule<'static>,
}

impl DbusScreen {
    pub fn new() -> Result<Self> {
        let connection = Connection::new_session()?;
        let response = MatchRule::new_signal("org.freedesktop.portal.Request", "Response")
            .with_sender(PORTAL_BUS_NAME);
        let owner_changed =
            MatchRule::new_signal(DBUS, "NameOwnerChanged").with_strict_sender(DBUS);
        // This private connection issues one request at a time. Subscribe before the first
        // call so an immediate response is queued even if the portal chooses its own path.
        connection.add_match_no_cb(&response.match_str())?;
        connection.add_match_no_cb(&owner_changed.match_str())?;
        Ok(Self {
            connection,
            response,
            owner_changed,
        })
    }

    pub fn start(&self, wanted_source_types: u32) -> Result<Session> {
        let proxy = self
            .connection
            .with_proxy(PORTAL_BUS_NAME, PORTAL_PATH, CALL_TIMEOUT);
        let available = proxy
            .method_call(
                "org.freedesktop.DBus.Properties",
                "Get",
                (SCREENCAST, "AvailableSourceTypes"),
            )
            .map(|r: (Variant<u32>,)| r.0.0)?;
        let source_type = if wanted_source_types == 0 {
            available
        } else {
            available & wanted_source_types
        };
        if source_type == 0 {
            return Err(PortalError::NoMatchingSource {
                wanted: wanted_source_types,
                available,
            }
            .into());
        }

        let mut options = PropMap::new();
        options.insert(
            "session_handle_token".into(),
            Variant(Box::new(String::from("scrcap_create"))),
        );
        let results = self.request(PortalCall::CreateSession, (options,))?;
        let handle = results
            .get("session_handle")
            .and_then(|handle| handle.0.as_str())
            .and_then(|path| Path::new(path.to_owned()).ok())
            .ok_or(PortalError::MalformedReply {
                call: PortalCall::CreateSession,
            })?;

        let mut options = PropMap::new();
        options.insert("types".into(), Variant(Box::new(source_type)));
        options.insert("multiple".into(), Variant(Box::new(false)));
        // Version 1 portals have no cursor-mode property.
        let cursor_modes = proxy
            .method_call(
                "org.freedesktop.DBus.Properties",
                "Get",
                (SCREENCAST, "AvailableCursorModes"),
            )
            .map_or(0, |r: (Variant<u32>,)| r.0.0);
        if cursor_modes & CURSOR_MODE_EMBEDDED != 0 {
            options.insert(
                "cursor_mode".into(),
                Variant(Box::new(CURSOR_MODE_EMBEDDED)),
            );
        }
        self.request(PortalCall::SelectSources, (handle.clone(), options))?;
        let results = self.request(PortalCall::Start, (handle.clone(), "", PropMap::new()))?;
        let target = stream_target(&results).ok_or(PortalError::MalformedReply {
            call: PortalCall::Start,
        })?;

        let (fd,): (OwnedFd,) =
            proxy.method_call(SCREENCAST, "OpenPipeWireRemote", (handle, PropMap::new()))?;
        // Keep the connection alive for the session without queueing further setup signals.
        self.connection
            .remove_match_no_cb(&self.response.match_str())?;
        self.connection
            .remove_match_no_cb(&self.owner_changed.match_str())?;
        Ok(Session { target, fd })
    }

    fn request(&self, call: PortalCall, args: impl AppendAll) -> Result<PropMap> {
        let mut message =
            Message::new_method_call(PORTAL_BUS_NAME, PORTAL_PATH, SCREENCAST, call.to_string())
                .expect("valid ScreenCast method");
        args.append(&mut IterAppend::new(&mut message));
        let reply = self
            .connection
            .send_with_reply_and_block(message, CALL_TIMEOUT)?;
        let malformed = PortalError::MalformedReply { call };
        let path = reply.read1::<Path>().map_err(|_| malformed)?;
        let owner = reply.sender().ok_or(malformed)?;
        let response = self
            .response
            .clone()
            .with_strict_sender(owner.clone().into_static())
            .with_path(path.into_static());
        // The picker can stay open for as long as the user needs. The portal losing its
        // name ends the wait, including when the connection that owned it is still alive.
        loop {
            let Some(message) = self
                .connection
                .channel()
                .blocking_pop_message(Duration::from_secs(30))?
            else {
                continue;
            };
            if response.matches(&message) {
                let (status, results) = message.read2::<u32, PropMap>().map_err(|_| malformed)?;
                return match status {
                    0 => Ok(results),
                    1 => Err(CaptureError::Cancelled),
                    response => Err(PortalError::Refused { call, response }.into()),
                };
            }
            if self.owner_changed.matches(&message)
                && let Ok((name, old, _)) = message.read3::<String, String, String>()
                && name == PORTAL_BUS_NAME
                && old == *owner
            {
                return Err(PortalError::Vanished { call }.into());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbus::arg::RefArg;

    fn target_from_wire(streams: impl RefArg + 'static) -> Option<StreamTarget> {
        let mut results = PropMap::new();
        results.insert("streams".into(), Variant(Box::new(streams)));
        let message = Message::new_signal("/test", "org.example.Test", "Response")
            .unwrap()
            .append1(results);
        stream_target(&message.read1().unwrap())
    }

    #[test]
    fn prefers_the_full_serial_and_accepts_legacy_node_ids() {
        let serial = u64::from(u32::MAX) + 42;
        let mut props = PropMap::new();
        props.insert("pipewire-serial".into(), Variant(Box::new(serial)));
        assert_eq!(
            target_from_wire(vec![(7u32, props)]),
            Some(StreamTarget::Serial(serial))
        );
        assert_eq!(
            target_from_wire(vec![(7u32, PropMap::new())]),
            Some(StreamTarget::NodeId(7))
        );
    }

    #[test]
    fn rejects_malformed_or_ambiguous_streams() {
        let mut props = PropMap::new();
        props.insert("pipewire-serial".into(), Variant(Box::new(7u32)));
        assert_eq!(target_from_wire(vec![(7u32, props)]), None);
        assert_eq!(target_from_wire(vec![(7u32, 0u32)]), None);
        assert_eq!(target_from_wire(Vec::<(u32, PropMap)>::new()), None);
        assert_eq!(
            target_from_wire(vec![(1u32, PropMap::new()), (2, PropMap::new())]),
            None
        );
        for id in [0u32, u32::MAX] {
            assert_eq!(target_from_wire(vec![(id, PropMap::new())]), None);
        }
    }
}
