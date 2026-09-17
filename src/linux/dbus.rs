//! D-Bus related
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use dbus::{
    MessageType, Path,
    arg::{Iter, OwnedFd, PropMap, ReadAll, RefArg, TypeMismatchError, Variant},
    blocking::Connection,
    message::MatchRule,
    strings::{BusName, Interface},
};
use parking_lot::Mutex;

use crate::error::{CaptureError, PortalCall, PortalError, Result};

pub const SOURCE_TYPE_MONITOR: u32 = 1;
/// `AvailableCursorModes` bit for a cursor drawn into the frames.
const CURSOR_MODE_EMBEDDED: u32 = 2;

/// A request in flight: the token that names it, the object path its `Response` signal was
/// predicted to arrive on, and the slot that signal lands in.
struct Request {
    token: String,
    path: Path<'static>,
    slot: Arc<Mutex<Option<Response>>>,
}

#[derive(Debug)]
pub struct Response {
    pub response: u32,
    pub results: PropMap,
}

impl ReadAll for Response {
    fn read(i: &mut Iter) -> std::result::Result<Self, TypeMismatchError> {
        Ok(Response {
            response: i.read()?,
            results: i.read()?,
        })
    }
}

/// What a granted ScreenCast session hands over: the video node, and the PipeWire remote
/// it lives on.
pub struct Session {
    pub node_id: u32,
    pub fd: OwnedFd,
}

const PORTAL_BUS_NAME: &str = "org.freedesktop.portal.Desktop";

pub struct DbusScreen {
    connection: Connection,
    /// Set once the portal that owned [`PORTAL_BUS_NAME`] has left the bus, taking every
    /// pending request with it.
    portal_gone: Arc<AtomicBool>,
}

impl DbusScreen {
    pub fn new() -> Result<Self> {
        let connection = Connection::new_session()?;
        let portal_gone = Arc::new(AtomicBool::new(false));
        let mut rule = MatchRule::new_signal("org.freedesktop.DBus", "NameOwnerChanged");
        rule.sender = Some(BusName::from("org.freedesktop.DBus"));
        connection.add_match(rule, {
            let portal_gone = Arc::clone(&portal_gone);
            move |(name, old, _new): (String, String, String), _c, _msg| {
                // An empty old owner is the portal being activated, not going away.
                if name == PORTAL_BUS_NAME && !old.is_empty() {
                    portal_gone.store(true, Ordering::Relaxed);
                }
                true
            }
        })?;
        Ok(Self {
            connection,
            portal_gone,
        })
    }

    pub fn start(&self, wanted_source_types: u32) -> Result<Session> {
        let proxy = self.connection.with_proxy(
            PORTAL_BUS_NAME,
            "/org/freedesktop/portal/desktop",
            Duration::from_secs(3),
        );

        // create session
        let request = self.watch("create")?;
        let mut map = PropMap::new();
        map.insert(
            "handle_token".to_string(),
            Variant(Box::new(request.token.clone())),
        );
        map.insert(
            "session_handle_token".to_string(),
            Variant(Box::new(request.token.clone())),
        );
        let path = proxy
            .method_call("org.freedesktop.portal.ScreenCast", "CreateSession", (map,))
            .map(|r: (Path<'static>,)| r.0)?;

        let resp = self.recv_resp(&request, path, PortalCall::CreateSession)?;
        check_response(&resp, PortalCall::CreateSession)?;
        let handle = resp
            .results
            .get("session_handle")
            .and_then(|handle| handle.0.as_str())
            .ok_or(PortalError::MalformedReply {
                call: PortalCall::CreateSession,
            })?;
        let handle = Path::from(handle.to_string());

        // select source
        let available = proxy
            .method_call(
                "org.freedesktop.DBus.Properties",
                "Get",
                ("org.freedesktop.portal.ScreenCast", "AvailableSourceTypes"),
            )
            .map(|r: (Variant<u32>,)| (r.0).0)?;
        let source_type = if wanted_source_types == 0 {
            available
        } else {
            let wanted = available & wanted_source_types;
            if wanted == 0 {
                return Err(PortalError::NoMatchingSource {
                    wanted: wanted_source_types,
                    available,
                }
                .into());
            }
            wanted
        };
        let request = self.watch("select")?;
        let mut map = PropMap::new();
        map.insert(
            String::from("handle_token"),
            Variant(Box::new(request.token.clone())),
        );
        map.insert(String::from("types"), Variant(Box::new(source_type)));
        map.insert(String::from("multiple"), Variant(Box::new(false)));
        // The portal rejects a cursor mode its backend does not advertise, and a version 1
        // portal has no such property at all; the cursor is then left at the default.
        let cursor_modes = proxy
            .method_call(
                "org.freedesktop.DBus.Properties",
                "Get",
                ("org.freedesktop.portal.ScreenCast", "AvailableCursorModes"),
            )
            .map_or(0, |r: (Variant<u32>,)| (r.0).0);
        if cursor_modes & CURSOR_MODE_EMBEDDED != 0 {
            map.insert(
                String::from("cursor_mode"),
                Variant(Box::new(CURSOR_MODE_EMBEDDED)),
            );
        }
        let path = proxy
            .method_call(
                "org.freedesktop.portal.ScreenCast",
                "SelectSources",
                (handle.clone(), map),
            )
            .map(|r: (Path<'static>,)| r.0)?;

        let resp = self.recv_resp(&request, path, PortalCall::SelectSources)?;
        check_response(&resp, PortalCall::SelectSources)?;

        // start capturing
        let request = self.watch("start")?;
        let mut map = PropMap::new();
        map.insert(
            String::from("handle_token"),
            Variant(Box::new(request.token.clone())),
        );
        let path = proxy
            .method_call(
                "org.freedesktop.portal.ScreenCast",
                "Start",
                (handle.clone(), "", map),
            )
            .map(|r: (Path<'static>,)| r.0)?;

        let resp = self.recv_resp(&request, path, PortalCall::Start)?;
        check_response(&resp, PortalCall::Start)?;
        let pipewire_node_id = resp
            .results
            .get("streams")
            .and_then(|streams| streams.as_iter())
            .and_then(|mut iter| iter.next())
            .and_then(|item| item.as_iter())
            .and_then(|mut iter| iter.next())
            .and_then(|item| item.as_iter())
            .and_then(|mut iter| iter.next())
            .and_then(|item| item.as_u64())
            .ok_or(PortalError::MalformedReply {
                call: PortalCall::Start,
            })?;

        // The screencast nodes live on the remote the portal hands out, which in a sandbox
        // is the only PipeWire socket this process may open.
        let fd = proxy
            .method_call(
                "org.freedesktop.portal.ScreenCast",
                "OpenPipeWireRemote",
                (handle, PropMap::new()),
            )
            .map(|r: (OwnedFd,)| r.0)?;

        Ok(Session {
            node_id: pipewire_node_id as u32,
            fd,
        })
    }

    /// Install the `Response` match *before* the call that triggers it.
    ///
    /// `add_match` is itself a round trip to the bus and D-Bus drops signals no rule
    /// matches, so a portal that answers without asking the user -- a remembered
    /// permission, an auto-accepting portal -- would otherwise win the race and the wait
    /// below would never end. The request's object path is predictable from the spec, so
    /// the rule can go in first.
    fn watch(&self, name: &str) -> Result<Request> {
        let sender = self
            .connection
            .unique_name()
            .trim_start_matches(':')
            .replace('.', "_");
        let token = format!("scrcap_{name}");
        let path = Path::from(format!(
            "/org/freedesktop/portal/desktop/request/{sender}/{token}"
        ));
        let slot = Arc::new(Mutex::new(None));
        self.add_response_match(path.clone(), Arc::clone(&slot))?;
        Ok(Request { token, path, slot })
    }

    fn add_response_match(
        &self,
        path: Path<'static>,
        slot: Arc<Mutex<Option<Response>>>,
    ) -> Result<()> {
        let mut rule = MatchRule::new();
        rule.path = Some(path);
        rule.msg_type = Some(MessageType::Signal);
        rule.sender = Some(BusName::from(PORTAL_BUS_NAME));
        rule.interface = Some(Interface::from("org.freedesktop.portal.Request"));
        self.connection
            .add_match(rule, move |res: Response, _c, _msg| {
                *slot.lock() = Some(res);
                false
            })?;
        Ok(())
    }

    /// Pump the bus until the watched request answers.
    ///
    /// Deliberately without a deadline: `SelectSources` and `Start` raise the portal's own
    /// picker, so the wait is as long as the user takes. `process` only fails when this
    /// connection breaks, so a portal that leaves the bus mid-request is caught through
    /// `portal_gone` instead -- its requests go with it and would never answer.
    fn recv_resp(
        &self,
        request: &Request,
        actual: Path<'static>,
        call: PortalCall,
    ) -> Result<Response> {
        // The path is only predicted; watch the one the portal actually returned too if a
        // portal ever disagrees, so a mismatch costs the race rather than the whole wait.
        if actual != request.path {
            self.add_response_match(actual, Arc::clone(&request.slot))?;
        }
        loop {
            self.connection.process(Duration::from_millis(100))?;
            if let Some(resp) = request.slot.lock().take() {
                return Ok(resp);
            }
            if self.portal_gone.load(Ordering::Relaxed) {
                return Err(PortalError::Vanished { call }.into());
            }
        }
    }
}

fn check_response(resp: &Response, call: PortalCall) -> Result<()> {
    match resp.response {
        0 => Ok(()),
        // The spec's "the user cancelled the interaction".
        1 => Err(CaptureError::Cancelled),
        response => Err(PortalError::Refused { call, response }.into()),
    }
}
