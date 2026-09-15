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

const TOKEN: &str = "scrcap";

#[derive(Debug)]
pub struct Response {
    pub response: u32,
    pub results: PropMap,
}

impl ReadAll for Response {
    fn read(i: &mut Iter) -> Result<Self, TypeMismatchError> {
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
    pub fn new() -> Self {
        Self {
            connection: Connection::new_session().unwrap(),
        }
    }

    pub fn start(&self) -> u32 {
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
            .map(|r: (Path<'static>,)| r.0)
            .unwrap();

        let resp = self.recv_resp(path).unwrap();
        let handle = resp.results.get("session_handle").unwrap();
        let handle = Path::from(handle.0.as_str().unwrap().to_string());

        // select source
        let source_type = proxy
            .method_call(
                "org.freedesktop.DBus.Properties",
                "Get",
                ("org.freedesktop.portal.ScreenCast", "AvailableSourceTypes"),
            )
            .map(|r: (Variant<u32>,)| (r.0).0)
            .unwrap();
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
            .map(|r: (Path<'static>,)| r.0)
            .unwrap();

        let resp = self.recv_resp(path).unwrap();
        if resp.response != 0 {
            panic!("Failed to select source");
        }

        // start capturing
        let path = proxy
            .method_call(
                "org.freedesktop.portal.ScreenCast",
                "Start",
                (handle, "", PropMap::new()),
            )
            .map(|r: (Path<'static>,)| r.0)
            .unwrap();

        let resp = self.recv_resp(path).unwrap();
        let streams = resp.results.get("streams").unwrap();
        let pipewire_node_id = streams
            .as_iter()
            .and_then(|mut iter| iter.next())
            .and_then(|item| item.as_iter())
            .and_then(|mut iter| iter.next())
            .and_then(|item| item.as_iter())
            .and_then(|mut iter| iter.next())
            .and_then(|item| item.as_u64())
            .unwrap();
        pipewire_node_id as u32
    }

    fn recv_resp(&self, path: Path<'static>) -> Option<Response> {
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
            })
            .unwrap();

        loop {
            self.connection.process(Duration::from_millis(100)).unwrap();
            if let Some(mut guard) = resp.try_lock() {
                return guard.take();
            }
        }
    }
}
