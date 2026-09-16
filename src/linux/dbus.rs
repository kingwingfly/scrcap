//! D-Bus related
use std::{sync::Arc, time::Duration};

use dbus::{
    MessageType, Path,
    arg::{Iter, PropMap, ReadAll, RefArg, TypeMismatchError, Variant},
    blocking::Connection,
    message::MatchRule,
    strings::{BusName, Interface},
};
use parking_lot::Mutex;

use crate::error::{CaptureError, PortalCall, PortalError, Result};

const TOKEN: &str = "scrcap";

pub const SOURCE_TYPE_MONITOR: u32 = 1;

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

pub struct DbusScreen {
    connection: Connection,
}

impl DbusScreen {
    pub fn new() -> Result<Self> {
        Ok(Self {
            connection: Connection::new_session()?,
        })
    }

    pub fn start(&self, wanted_source_types: u32) -> Result<u32> {
        let proxy = self.connection.with_proxy(
            "org.freedesktop.portal.Desktop",
            "/org/freedesktop/portal/desktop",
            Duration::from_secs(3),
        );
        let token = TOKEN.to_string();

        // create session
        let mut map = PropMap::new();
        map.insert("handle_token".to_string(), Variant(Box::new(token.clone())));
        map.insert(
            "session_handle_token".to_string(),
            Variant(Box::new(token.clone())),
        );
        let path = proxy
            .method_call("org.freedesktop.portal.ScreenCast", "CreateSession", (map,))
            .map(|r: (Path<'static>,)| r.0)?;

        let resp = self.recv_resp(path, PortalCall::CreateSession)?;
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
        let mut map = PropMap::new();
        map.insert(
            String::from("handle_token"),
            Variant(Box::new(token.clone())),
        );
        map.insert(String::from("types"), Variant(Box::new(source_type)));
        map.insert(String::from("multiple"), Variant(Box::new(false)));
        map.insert(String::from("cursor_mode"), Variant(Box::new(2u32)));
        let path = proxy
            .method_call(
                "org.freedesktop.portal.ScreenCast",
                "SelectSources",
                (handle.clone(), map),
            )
            .map(|r: (Path<'static>,)| r.0)?;

        let resp = self.recv_resp(path, PortalCall::SelectSources)?;
        check_response(&resp, PortalCall::SelectSources)?;

        // start capturing
        let path = proxy
            .method_call(
                "org.freedesktop.portal.ScreenCast",
                "Start",
                (handle, "", PropMap::new()),
            )
            .map(|r: (Path<'static>,)| r.0)?;

        let resp = self.recv_resp(path, PortalCall::Start)?;
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
        Ok(pipewire_node_id as u32)
    }

    fn recv_resp(&self, path: Path<'static>, call: PortalCall) -> Result<Response> {
        let resp = Arc::new(Mutex::new(None));
        let mut resp_guard = resp.lock_arc();
        let mut rule = MatchRule::new();
        rule.path = Some(path);
        rule.msg_type = Some(MessageType::Signal);
        rule.sender = Some(BusName::from("org.freedesktop.portal.Desktop"));
        rule.interface = Some(Interface::from("org.freedesktop.portal.Request"));
        self.connection
            .add_match(rule, move |res: Response, _c, _msg| {
                *resp_guard = Some(res);
                false
            })?;

        loop {
            self.connection.process(Duration::from_millis(100))?;
            if let Some(mut guard) = resp.try_lock() {
                return guard.take().ok_or(PortalError::NoResponse { call }.into());
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
