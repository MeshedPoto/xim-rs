//! Provides an implementation of XIM using [`x11rb`] as a transport.
//!
//! Wrap your `Connection` in an [`X11rbClient`] or [`X11rbServer`] and use it as a
//! client or server.
//!
//! [`x11rb`]: https://crates.io/crates/x11rb

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use std::{convert::TryInto, rc::Rc, sync::Arc};
use x11rb::protocol::xproto::EventMask;

#[cfg(feature = "x11rb-client")]
use crate::client::{
    handle_request as client_handle_request, ClientCore, ClientError, ClientHandler,
};
#[cfg(feature = "x11rb-server")]
use crate::server::{ServerCore, ServerError, ServerHandler, XimConnection, XimConnections};
use crate::AHashMap;
#[cfg(feature = "x11rb-client")]
use xim_parser::{Attr, AttributeName};

use crate::Atoms;

#[cfg(feature = "x11rb-xcb")]
use x11rb::xcb_ffi::XCBConnection;

#[allow(unused_imports)]
use x11rb::{
    connection::Connection,
    errors::{ConnectError, ConnectionError, ParseError, ReplyError, ReplyOrIdError},
    protocol::{
        xproto::{
            Atom, AtomEnum, ClientMessageEvent, ConnectionExt, KeyPressEvent, PropMode, Screen,
            SelectionNotifyEvent, SelectionRequestEvent, Window, WindowClass, CLIENT_MESSAGE_EVENT,
            SELECTION_NOTIFY_EVENT,
        },
        Event,
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
    COPY_DEPTH_FROM_PARENT, CURRENT_TIME,
};

use xim_parser::{Request, XimWrite};

macro_rules! convert_error {
    ($($ty:ty,)+) => {
        $(
            #[cfg(feature = "x11rb-client")]
            impl From<$ty> for ClientError {
                fn from(err: $ty) -> Self {
                    ClientError::Other(err.into())
                }
            }

            #[cfg(feature = "x11rb-server")]
            impl From<$ty> for ServerError {
                fn from(err: $ty) -> Self {
                    ServerError::Other(err.into())
                }
            }
        )+
    };
}

convert_error!(
    ConnectError,
    ConnectionError,
    ReplyError,
    ReplyOrIdError,
    ParseError,
);

pub trait HasConnection {
    type Connection: Connection + ConnectionExt;

    fn conn(&self) -> &Self::Connection;
}

#[cfg(feature = "x11rb-xcb")]
impl HasConnection for XCBConnection {
    type Connection = Self;

    #[inline(always)]
    fn conn(&self) -> &Self::Connection {
        self
    }
}

impl HasConnection for RustConnection {
    type Connection = Self;

    #[inline(always)]
    fn conn(&self) -> &Self::Connection {
        self
    }
}

#[cfg(feature = "x11rb-client")]
impl<C: HasConnection> HasConnection for X11rbClient<C> {
    type Connection = C::Connection;

    #[inline(always)]
    fn conn(&self) -> &Self::Connection {
        self.has_conn.conn()
    }
}

#[cfg(feature = "x11rb-server")]
impl<C: HasConnection> HasConnection for X11rbServer<C> {
    type Connection = C::Connection;

    #[inline(always)]
    fn conn(&self) -> &Self::Connection {
        self.has_conn.conn()
    }
}

impl<'x, C: HasConnection> HasConnection for &'x C {
    type Connection = C::Connection;

    #[inline(always)]
    fn conn(&self) -> &Self::Connection {
        (**self).conn()
    }
}

impl<C: HasConnection> HasConnection for Rc<C> {
    type Connection = C::Connection;

    #[inline(always)]
    fn conn(&self) -> &Self::Connection {
        (**self).conn()
    }
}

impl<C: HasConnection> HasConnection for Arc<C> {
    type Connection = C::Connection;

    #[inline(always)]
    fn conn(&self) -> &Self::Connection {
        (**self).conn()
    }
}

#[cfg(feature = "x11rb-server")]
pub struct X11rbServer<C: HasConnection> {
    has_conn: C,
    locale_data: String,
    im_win: Window,
    atoms: Atoms<Atom>,
    buf: Vec<u8>,
    sequence: u16,
    property_offsets: PropertyOffsets,
}

#[cfg(feature = "x11rb-server")]
impl<C: HasConnection> X11rbServer<C> {
    pub fn init(
        has_conn: C,
        screen_num: usize,
        im_name: &str,
        locales: &str,
    ) -> Result<Self, ServerError> {
        let im_name = format!("@server={}", im_name);
        let conn = has_conn.conn();
        let screen = &conn.setup().roots[screen_num];
        let im_win = conn.generate_id()?;
        conn.create_window(
            COPY_DEPTH_FROM_PARENT,
            im_win,
            screen.root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            screen.root_visual,
            &Default::default(),
        )?;
        let atoms = Atoms::new::<ServerError, _>(|name| {
            Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
        })?;

        let reply = conn
            .get_property(
                false,
                screen.root,
                atoms.XIM_SERVERS,
                AtomEnum::ATOM,
                0,
                u32::MAX,
            )?
            .reply()?;

        if reply.type_ != x11rb::NONE && (reply.type_ != u32::from(AtomEnum::ATOM)) {
            return Err(ServerError::InvalidReply);
        }

        let server_name = conn.intern_atom(false, im_name.as_bytes())?.reply()?.atom;

        let mut found = false;

        if reply.type_ != x11rb::NONE {
            for prop in reply.value32().ok_or(ServerError::InvalidReply)? {
                if prop == server_name {
                    log::info!("Found previous XIM_SERVER it will overrided");
                    found = true;
                }
            }
        }

        // override owner
        conn.set_selection_owner(im_win, server_name, x11rb::CURRENT_TIME)?;

        if !found {
            conn.change_property32(
                PropMode::PREPEND,
                screen.root,
                atoms.XIM_SERVERS,
                AtomEnum::ATOM,
                &[server_name],
            )?;
        }

        conn.flush()?;

        log::info!("Start server win: {}", im_win);

        Ok(Self {
            has_conn,
            locale_data: format!("@locale={}", locales),
            im_win,
            atoms,
            buf: Vec::with_capacity(1024),
            sequence: 0,
            property_offsets: PropertyOffsets::with_hasher(Default::default()),
        })
    }

    pub fn filter_event<T>(
        &mut self,
        e: &Event,
        connections: &mut XimConnections<T>,
        handler: &mut impl ServerHandler<Self, InputContextData = T>,
    ) -> Result<bool, ServerError> {
        match e {
            Event::SelectionRequest(req) if req.owner == self.im_win => {
                if req.property == self.atoms.LOCALES {
                    log::trace!("Selection notify locale");
                    self.send_selection_notify(req, &self.locale_data)?;
                } else if req.property == self.atoms.TRANSPORT {
                    log::trace!("Selection notify transport");
                    self.send_selection_notify(req, "@transport=X/")?;
                }
                Ok(true)
            }
            Event::ClientMessage(msg) => {
                if msg.type_ == self.atoms.XIM_XCONNECT {
                    let com_win = self.conn().generate_id()?;
                    self.conn().create_window(
                        COPY_DEPTH_FROM_PARENT,
                        com_win,
                        self.im_win,
                        0,
                        0,
                        1,
                        1,
                        0,
                        WindowClass::INPUT_ONLY,
                        0,
                        &Default::default(),
                    )?;
                    let client_win = msg.data.as_data32()[0];
                    log::info!("XConnected with {}", client_win);
                    self.conn().send_event(
                        false,
                        client_win,
                        EventMask::NO_EVENT,
                        ClientMessageEvent {
                            format: 32,
                            type_: self.atoms.XIM_XCONNECT,
                            data: [com_win, 0, 0, 0, 0].into(),
                            response_type: CLIENT_MESSAGE_EVENT,
                            sequence: 0,
                            window: client_win,
                        },
                    )?;
                    self.conn().flush()?;
                    connections.new_connection(com_win, client_win);
                } else if msg.type_ == self.atoms.XIM_PROTOCOL {
                    if let Some(connection) = connections.get_connection(msg.window) {
                        self.handle_xim_protocol(msg, connection, handler)?;
                        if connection.disconnected {
                            connections.remove_connection(msg.window);
                            self.property_offsets
                                .retain(|(window, _), _| *window != msg.window);
                        }
                    } else {
                        log::warn!("Unknown connection");
                    }
                }

                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn handle_xim_protocol<T>(
        &mut self,
        msg: &ClientMessageEvent,
        connection: &mut XimConnection<T>,
        handler: &mut impl ServerHandler<Self, InputContextData = T>,
    ) -> Result<(), ServerError> {
        if msg.format == 32 {
            let data = read_xim_property(&self.has_conn, &mut self.property_offsets, msg)?
                .ok_or(ServerError::InvalidReply)?;
            let req = xim_parser::read(&data)?;
            connection.handle_request(self, req, handler)
        } else {
            let req = xim_parser::read(&msg.data.as_data8())?;
            connection.handle_request(self, req, handler)
        }
    }

    fn send_selection_notify(
        &self,
        req: &SelectionRequestEvent,
        data: &str,
    ) -> Result<(), ServerError> {
        let e = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            property: req.property,
            time: req.time,
            target: req.target,
            selection: req.selection,
            requestor: req.requestor,
            sequence: 0,
        };

        self.conn().change_property8(
            PropMode::REPLACE,
            req.requestor,
            req.property,
            req.target,
            data.as_bytes(),
        )?;
        self.conn()
            .send_event(false, req.requestor, EventMask::NO_EVENT, e)?;
        self.conn().flush()?;

        Ok(())
    }
}

#[cfg(feature = "x11rb-server")]
impl<C: HasConnection> ServerCore for X11rbServer<C> {
    type XEvent = KeyPressEvent;

    fn send_req(&mut self, client_win: u32, req: Request) -> Result<(), ServerError> {
        send_req_impl(
            &self.has_conn,
            &self.atoms,
            client_win,
            &mut self.buf,
            &mut self.sequence,
            PropertyAtoms::LegacyServer,
            20,
            &req,
        )
    }

    #[inline]
    fn deserialize_event(&self, ev: &xim_parser::XEvent) -> Self::XEvent {
        deserialize_event_impl(ev)
    }
}

#[cfg(feature = "x11rb-client")]
pub struct X11rbClient<C: HasConnection> {
    has_conn: C,
    server_owner_window: Window,
    im_window: Window,
    server_atom: Atom,
    atoms: Atoms<Atom>,
    transport_max: usize,
    client_window: u32,
    im_attributes: AHashMap<AttributeName, u16>,
    ic_attributes: AHashMap<AttributeName, u16>,
    sequence: u16,
    property_offsets: PropertyOffsets,
    buf: Vec<u8>,
}

#[cfg(feature = "x11rb-client")]
impl<C: HasConnection> X11rbClient<C> {
    pub fn init(
        has_conn: C,
        screen_num: usize,
        im_name: Option<&str>,
    ) -> Result<Self, ClientError> {
        let conn = has_conn.conn();
        let screen = &conn.setup().roots[screen_num];
        let client_window = conn.generate_id()?;

        conn.create_window(
            COPY_DEPTH_FROM_PARENT,
            client_window,
            screen.root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            screen.root_visual,
            &Default::default(),
        )?;

        let var = std::env::var("XMODIFIERS").ok();
        let var = var.as_ref().and_then(|n| n.strip_prefix("@im="));
        let im_name = im_name.or(var).ok_or(ClientError::NoXimServer)?;

        log::info!("Try connect {}", im_name);

        let atoms = Atoms::new::<ClientError, _>(|name| {
            Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
        })?;
        let server_reply = conn
            .get_property(
                false,
                screen.root,
                atoms.XIM_SERVERS,
                AtomEnum::ATOM,
                0,
                u32::MAX,
            )?
            .reply()?;

        if server_reply.type_ != u32::from(AtomEnum::ATOM) || server_reply.format != 32 {
            Err(ClientError::InvalidReply)
        } else {
            for server_atom in server_reply.value32().ok_or(ClientError::InvalidReply)? {
                let server_owner = conn.get_selection_owner(server_atom)?.reply()?.owner;
                let name = conn.get_atom_name(server_atom)?.reply()?.name;

                let name = match String::from_utf8(name) {
                    Ok(name) => name,
                    _ => continue,
                };

                if let Some(name) = name.strip_prefix("@server=") {
                    if name == im_name {
                        conn.convert_selection(
                            client_window,
                            server_atom,
                            atoms.TRANSPORT,
                            atoms.TRANSPORT,
                            CURRENT_TIME,
                        )?;

                        conn.flush()?;

                        return Ok(Self {
                            has_conn,
                            atoms,
                            server_atom,
                            server_owner_window: server_owner,
                            im_attributes: AHashMap::with_hasher(Default::default()),
                            ic_attributes: AHashMap::with_hasher(Default::default()),
                            im_window: x11rb::NONE,
                            transport_max: 20,
                            client_window,
                            sequence: 0,
                            property_offsets: PropertyOffsets::with_hasher(Default::default()),
                            buf: Vec::with_capacity(1024),
                        });
                    }
                }
            }

            Err(ClientError::NoXimServer)
        }
    }

    pub fn filter_event(
        &mut self,
        e: &Event,
        handler: &mut impl ClientHandler<Self>,
    ) -> Result<bool, ClientError> {
        match e {
            Event::SelectionNotify(e) if e.requestor == self.client_window => {
                if e.property == self.atoms.LOCALES {
                    // TODO: set locale
                    let _locale = self
                        .conn()
                        .get_property(
                            true,
                            self.client_window,
                            self.atoms.LOCALES,
                            self.atoms.LOCALES,
                            0,
                            u32::MAX,
                        )?
                        .reply()?;

                    self.xconnect()?;

                    Ok(true)
                } else if e.property == self.atoms.TRANSPORT {
                    let transport = self
                        .conn()
                        .get_property(
                            true,
                            self.client_window,
                            self.atoms.TRANSPORT,
                            self.atoms.TRANSPORT,
                            0,
                            u32::MAX,
                        )?
                        .reply()?;

                    if !transport.value.starts_with(b"@transport=X/") {
                        return Err(ClientError::UnsupportedTransport);
                    }

                    self.conn().convert_selection(
                        self.client_window,
                        self.server_atom,
                        self.atoms.LOCALES,
                        self.atoms.LOCALES,
                        CURRENT_TIME,
                    )?;

                    self.conn().flush()?;

                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            Event::ClientMessage(msg) if msg.window == self.client_window => {
                if msg.type_ == self.atoms.XIM_XCONNECT {
                    let [im_window, major, minor, max, _] = msg.data.as_data32();
                    log::info!(
                        "XConnected server on {}, transport version: {}.{}, TRANSPORT_MAX: {}",
                        im_window,
                        major,
                        minor,
                        max
                    );
                    self.im_window = im_window;
                    self.transport_max = max as usize;
                    self.send_req(Request::Connect {
                        client_major_protocol_version: 1,
                        client_minor_protocol_version: 0,
                        endian: xim_parser::Endian::Native,
                        client_auth_protocol_names: Vec::new(),
                    })?;
                    Ok(true)
                } else if msg.type_ == self.atoms.XIM_PROTOCOL {
                    self.handle_xim_protocol(msg, handler)?;
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            Event::Error(ref err)
                if err.error_kind == x11rb::protocol::ErrorKind::Window
                    && err.bad_value == self.im_window =>
            {
                Err(ClientError::NoXimServer)
            }
            _ => Ok(false),
        }
    }

    fn handle_xim_protocol(
        &mut self,
        msg: &ClientMessageEvent,
        handler: &mut impl ClientHandler<Self>,
    ) -> Result<(), ClientError> {
        if msg.format == 32 {
            let data = read_xim_property(&self.has_conn, &mut self.property_offsets, msg)?
                .ok_or(ClientError::InvalidReply)?;
            let req = xim_parser::read(&data)?;
            if matches!(&req, Request::DisconnectReply {}) {
                self.property_offsets.clear();
            }
            client_handle_request(self, handler, req)?;
        } else if msg.format == 8 {
            let data = msg.data.as_data8();
            let req: xim_parser::Request = xim_parser::read(&data)?;
            if matches!(&req, Request::DisconnectReply {}) {
                self.property_offsets.clear();
            }
            client_handle_request(self, handler, req)?;
        }

        Ok(())
    }

    fn xconnect(&mut self) -> Result<(), ClientError> {
        self.conn().send_event(
            false,
            self.server_owner_window,
            EventMask::NO_EVENT,
            ClientMessageEvent {
                data: [self.client_window, 0, 0, 0, 0].into(),
                format: 32,
                response_type: CLIENT_MESSAGE_EVENT,
                sequence: 0,
                type_: self.atoms.XIM_XCONNECT,
                window: self.server_owner_window,
            },
        )?;

        self.conn().flush()?;

        Ok(())
    }
}

#[cfg(feature = "x11rb-client")]
impl<C: HasConnection> ClientCore for X11rbClient<C> {
    type XEvent = KeyPressEvent;
    fn set_attrs(&mut self, im_attrs: Vec<Attr>, ic_attrs: Vec<Attr>) {
        for im_attr in im_attrs {
            self.im_attributes.insert(im_attr.name, im_attr.id);
        }

        for ic_attr in ic_attrs {
            self.ic_attributes.insert(ic_attr.name, ic_attr.id);
        }
    }

    #[inline]
    fn ic_attributes(&self) -> &AHashMap<AttributeName, u16> {
        &self.ic_attributes
    }

    #[inline]
    fn im_attributes(&self) -> &AHashMap<AttributeName, u16> {
        &self.im_attributes
    }

    #[inline]
    fn serialize_event(&self, xev: &Self::XEvent) -> xim_parser::XEvent {
        xim_parser::XEvent {
            response_type: xev.response_type,
            detail: xev.detail,
            sequence: xev.sequence,
            time: xev.time,
            root: xev.root,
            event: xev.event,
            child: xev.child,
            root_x: xev.root_x,
            root_y: xev.root_y,
            event_x: xev.event_x,
            event_y: xev.event_y,
            state: xev.state.into(),
            same_screen: xev.same_screen,
        }
    }

    #[inline]
    fn deserialize_event(&self, xev: &xim_parser::XEvent) -> Self::XEvent {
        deserialize_event_impl(xev)
    }

    #[inline]
    fn send_req(&mut self, req: Request) -> Result<(), ClientError> {
        send_req_impl(
            &self.has_conn,
            &self.atoms,
            self.im_window,
            &mut self.buf,
            &mut self.sequence,
            PropertyAtoms::BoundedClient,
            self.transport_max,
            &req,
        )
    }
}

// Atom names live for the lifetime of the X server. Reuse a small, global
// namespace rather than allocating one name per packet or per connection.
const PROPERTY_ATOM_COUNT: u16 = 32;

// Keep legacy server allocation until all supported client transports can
// consume several packets appended to a reused property (including xlib).
#[derive(Clone, Copy)]
enum PropertyAtoms {
    BoundedClient,
    LegacyServer,
}
type PropertyOffsets = AHashMap<(Window, Atom), u32>;

// APPEND may place several outstanding packets in one property. Consume only
// the packet named by this ClientMessage, preserving ordering with inline CMs.
// X deletes the property atomically only when this read reaches its end.
fn read_xim_property<C: HasConnection>(
    c: &C,
    offsets: &mut PropertyOffsets,
    msg: &ClientMessageEvent,
) -> Result<Option<Vec<u8>>, ReplyError> {
    let [length, atom, ..] = msg.data.as_data32();
    // XIM packets have a four-byte header and a body measured in four-byte units.
    if length < 4 || length % 4 != 0 || length > 4 + u16::MAX as u32 * 4 {
        return Ok(None);
    }
    let key = (msg.window, atom);
    let offset = offsets.get(&key).copied().unwrap_or(0);
    let end = match offset.checked_add(length) {
        Some(end) => end,
        None => return Ok(None),
    };
    let reply = c
        .conn()
        .get_property(
            true,
            msg.window,
            atom,
            AtomEnum::ANY,
            offset / 4,
            length / 4,
        )?
        .reply()?;
    if reply.bytes_after == 0 {
        offsets.remove(&key);
    } else {
        offsets.insert(key, end);
    }
    if reply.format != 8 || reply.value.len() != length as usize {
        return Ok(None);
    }
    Ok(Some(reply.value))
}

fn send_req_impl<C: HasConnection, E: From<ConnectionError> + From<ReplyError>>(
    c: &C,
    atoms: &Atoms<Atom>,
    target: Window,
    buf: &mut Vec<u8>,
    sequence: &mut u16,
    property_atoms: PropertyAtoms,
    transport_max: usize,
    req: &Request,
) -> Result<(), E> {
    if log::log_enabled!(log::Level::Trace) {
        log::trace!("->: {:?}", req);
    } else {
        log::debug!("->: {}", req.name());
    }
    buf.resize(req.size(), 0);
    xim_parser::write(req, buf);

    if buf.len() < transport_max {
        if buf.len() > 20 {
            todo!("multi-CM");
        }
        buf.resize(20, 0);
        let buf: [u8; 20] = buf.as_slice().try_into().unwrap();
        c.conn().send_event(
            false,
            target,
            EventMask::NO_EVENT,
            ClientMessageEvent {
                response_type: CLIENT_MESSAGE_EVENT,
                data: buf.into(),
                format: 8,
                sequence: 0,
                type_: atoms.XIM_PROTOCOL,
                window: target,
            },
        )?;
    } else {
        let prop = c
            .conn()
            .intern_atom(false, format!("_XIM_DATA_{}", sequence).as_bytes())?
            .reply()?
            .atom;
        *sequence = match property_atoms {
            PropertyAtoms::BoundedClient => (*sequence + 1) % PROPERTY_ATOM_COUNT,
            PropertyAtoms::LegacyServer => sequence.wrapping_add(1),
        };
        c.conn().change_property(
            PropMode::APPEND,
            target,
            prop,
            AtomEnum::STRING,
            8,
            buf.len() as u32,
            buf,
        )?;
        c.conn().send_event(
            false,
            target,
            EventMask::NO_EVENT,
            ClientMessageEvent {
                data: [buf.len() as u32, prop, 0, 0, 0].into(),
                format: 32,
                sequence: 0,
                response_type: CLIENT_MESSAGE_EVENT,
                type_: atoms.XIM_PROTOCOL,
                window: target,
            },
        )?;
    }
    buf.clear();
    c.conn().flush()?;
    Ok(())
}

#[inline]
fn deserialize_event_impl(xev: &xim_parser::XEvent) -> KeyPressEvent {
    KeyPressEvent {
        response_type: xev.response_type,
        detail: xev.detail,
        sequence: xev.sequence,
        time: xev.time,
        root: xev.root,
        event: xev.event,
        child: xev.child,
        root_x: xev.root_x,
        root_y: xev.root_y,
        event_x: xev.event_x,
        event_y: xev.event_y,
        state: xev.state.into(),
        same_screen: xev.same_screen,
    }
}

#[cfg(test)]
mod property_transport_tests {
    use super::*;
    use std::collections::HashSet;
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};

    struct TestDisplay(Child);
    impl Drop for TestDisplay {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    // Never connects to DISPLAY. Xvfb chooses a free, private display, and this
    // guard owns and terminates only the server created by this test.
    fn private_display() -> (TestDisplay, String) {
        let binary = std::env::var_os("XIM_TEST_XVFB").unwrap_or_else(|| "Xvfb".into());
        let mut child = Command::new(binary)
            .args([
                "-displayfd",
                "1",
                "-screen",
                "0",
                "640x480x24",
                "-nolisten",
                "tcp",
            ])
            .stdout(Stdio::piped())
            .spawn()
            .expect("install Xvfb or set XIM_TEST_XVFB to run isolated transport tests");
        let stdout = child.stdout.take().unwrap();
        let guard = TestDisplay(child);
        let mut number = String::new();
        BufReader::new(stdout).read_line(&mut number).unwrap();
        let number: u32 = number
            .trim()
            .parse()
            .expect("Xvfb did not report a display");
        (guard, format!(":{}", number))
    }

    fn window(conn: &RustConnection, screen: usize) -> Window {
        let id = conn.generate_id().unwrap();
        conn.create_window(
            COPY_DEPTH_FROM_PARENT,
            id,
            conn.setup().roots[screen].root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &Default::default(),
        )
        .unwrap()
        .check()
        .unwrap();
        id
    }

    #[test]
    fn bounded_atoms_preserve_queued_packets_and_inline_order() {
        let (_display, address) = private_display();
        let (receiver, screen) = x11rb::connect(Some(&address)).unwrap();
        let windows = [window(&receiver, screen), window(&receiver, screen)];
        let mut offsets = PropertyOffsets::with_hasher(Default::default());
        let mut all_atoms = HashSet::new();
        // Reconnect the sender and reuse the same names/windows. Each round
        // queues far more than the pool size before the receiver reads anything.
        for round in 0..3 {
            let (sender, _) = x11rb::connect(Some(&address)).unwrap();
            let atoms = Atoms::new::<ReplyError, _>(|name| {
                Ok(sender.intern_atom(false, name.as_bytes())?.reply()?.atom)
            })
            .unwrap();
            let mut sequence = 0;
            let mut buf = Vec::new();
            let mut expected = Vec::new();
            for i in 0..2048 {
                let req = Request::Open {
                    locale: if i % 7 == 0 {
                        "en".into()
                    } else {
                        format!("中文输入保持完整-{}-{}-{}", round, i, "x".repeat(i % 31))
                    },
                };
                let mut bytes = alloc::vec![0; req.size()];
                xim_parser::write(&req, &mut bytes);
                let target = windows[i % windows.len()];
                send_req_impl::<_, ReplyError>(
                    &sender,
                    &atoms,
                    target,
                    &mut buf,
                    &mut sequence,
                    PropertyAtoms::BoundedClient,
                    20,
                    &req,
                )
                .unwrap();
                expected.push((target, bytes));
            }
            // Server-side barrier: every append/event has been processed before
            // we drain, so the test cannot accidentally avoid pool collisions.
            sender.get_input_focus().unwrap().reply().unwrap();
            for (target, bytes) in expected {
                let msg = loop {
                    if let Event::ClientMessage(msg) = receiver.wait_for_event().unwrap() {
                        break msg;
                    }
                };
                assert_eq!(msg.window, target);
                let actual = if msg.format == 32 {
                    all_atoms.insert(msg.data.as_data32()[1]);
                    read_xim_property(&receiver, &mut offsets, &msg)
                        .unwrap()
                        .unwrap()
                } else {
                    msg.data.as_data8()[..bytes.len()].to_vec()
                };
                assert_eq!(
                    actual, bytes,
                    "queued XIM packet changed or arrived out of order"
                );
                xim_parser::read::<Request>(&actual).unwrap();
            }
            assert!(
                offsets.is_empty(),
                "fully drained properties must reset offsets"
            );
            assert!(all_atoms.len() <= PROPERTY_ATOM_COUNT as usize);
            for target in windows {
                for atom in &all_atoms {
                    let reply = receiver
                        .get_property(false, target, *atom, AtomEnum::ANY, 0, 1)
                        .unwrap()
                        .reply()
                        .unwrap();
                    assert_eq!(reply.type_, x11rb::NONE, "drained property was not deleted");
                }
            }
        }
        assert_eq!(all_atoms.len(), PROPERTY_ATOM_COUNT as usize);
    }

    #[test]
    fn property_reads_reject_bad_lengths_and_preserve_pending_data() {
        let (_display, address) = private_display();
        let (conn, screen) = x11rb::connect(Some(&address)).unwrap();
        let target = window(&conn, screen);
        let atom = conn
            .intern_atom(false, b"_XIM_DATA_0")
            .unwrap()
            .reply()
            .unwrap()
            .atom;
        let mut offsets = PropertyOffsets::with_hasher(Default::default());
        let message = |length| ClientMessageEvent::new(32, target, atom, [length, atom, 0, 0, 0]);
        conn.change_property8(PropMode::APPEND, target, atom, AtomEnum::STRING, &[1; 8])
            .unwrap();
        for length in [0, 3, 5, u32::MAX] {
            assert!(read_xim_property(&conn, &mut offsets, &message(length))
                .unwrap()
                .is_none());
        }
        assert_eq!(
            read_xim_property(&conn, &mut offsets, &message(4))
                .unwrap()
                .unwrap(),
            [1; 4]
        );
        assert_eq!(offsets.get(&(target, atom)), Some(&4));
        // Append while an earlier frame remains unconsumed.
        conn.change_property8(PropMode::APPEND, target, atom, AtomEnum::STRING, &[2; 4])
            .unwrap();
        assert_eq!(
            read_xim_property(&conn, &mut offsets, &message(4))
                .unwrap()
                .unwrap(),
            [1; 4]
        );
        assert_eq!(
            read_xim_property(&conn, &mut offsets, &message(4))
                .unwrap()
                .unwrap(),
            [2; 4]
        );
        assert!(offsets.is_empty());
        conn.change_property8(PropMode::APPEND, target, atom, AtomEnum::STRING, &[3; 4])
            .unwrap();
        assert_eq!(
            read_xim_property(&conn, &mut offsets, &message(4))
                .unwrap()
                .unwrap(),
            [3; 4]
        );
        assert!(read_xim_property(&conn, &mut offsets, &message(4))
            .unwrap()
            .is_none());
        conn.change_property8(PropMode::APPEND, target, atom, AtomEnum::STRING, &[4; 4])
            .unwrap();
        assert!(read_xim_property(&conn, &mut offsets, &message(8))
            .unwrap()
            .is_none());
        conn.change_property32(PropMode::REPLACE, target, atom, AtomEnum::CARDINAL, &[1])
            .unwrap();
        assert!(read_xim_property(&conn, &mut offsets, &message(4))
            .unwrap()
            .is_none());
    }
}
