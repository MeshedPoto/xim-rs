//! Provides an implementation of XIM using [`x11rb`] as a transport.
//!
//! Wrap your `Connection` in an [`X11rbClient`] or [`X11rbServer`] and use it as a
//! client or server.
//!
//! [`x11rb`]: https://crates.io/crates/x11rb

#[cfg(feature = "x11rb-client")]
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use std::{rc::Rc, sync::Arc};
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
            Atom, AtomEnum, ChangeWindowAttributesAux, ClientMessageEvent, ConnectionExt,
            KeyPressEvent, PropMode, Property, Screen, SelectionNotifyEvent, SelectionRequestEvent,
            Window, WindowClass, CLIENT_MESSAGE_EVENT, SELECTION_NOTIFY_EVENT,
        },
        ErrorKind, Event,
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
    transport: PropertyTransport,
    property_offsets: PropertyOffsets,
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
                            transport: PropertyTransport::new(),
                            property_offsets: PropertyOffsets::with_hasher(Default::default()),
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
            Event::PropertyNotify(e) if e.window == self.im_window => {
                if e.state == Property::DELETE {
                    self.transport
                        .flush(&self.has_conn, &self.atoms, self.transport_max)?;
                }
                Ok(true)
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
                    self.transport.connect(&self.has_conn, im_window)?;
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
                self.transport.disconnect();
            }
            client_handle_request(self, handler, req)?;
        } else if msg.format == 8 {
            let data = msg.data.as_data8();
            let req: xim_parser::Request = xim_parser::read(&data)?;
            if matches!(&req, Request::DisconnectReply {}) {
                self.property_offsets.clear();
                self.transport.disconnect();
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
        log_req(&req);
        let mut packet = alloc::vec![0; req.size()];
        xim_parser::write(&req, &mut packet);
        self.transport
            .send(&self.has_conn, &self.atoms, self.transport_max, packet)
    }
}

// Atom names live for the lifetime of the X server. Reuse a small, global
// namespace rather than allocating one name per packet or per connection.
#[cfg(feature = "x11rb-client")]
const PROPERTY_ATOM_COUNT: usize = 32;
// Packets wait here while every property still holds an unread packet. A
// server that stops deleting properties gets an error instead of unbounded memory.
#[cfg(feature = "x11rb-client")]
const MAX_QUEUED_PACKETS: usize = 1024;

type PropertyOffsets = AHashMap<(Window, Atom), u32>;

// Receivers before this fork (and xlib-style receivers) read every property from
// offset zero and delete it, so a property must never hold more than one packet.
// Reuse a name only after the receiver deleted it; until then queue every packet,
// inline ones included, so ClientMessage order is unchanged.
#[cfg(feature = "x11rb-client")]
struct PropertyTransport {
    target: Window,
    slots: Vec<Atom>,
    next: usize,
    queue: VecDeque<Vec<u8>>,
}

#[cfg(feature = "x11rb-client")]
impl PropertyTransport {
    fn new() -> Self {
        Self {
            target: x11rb::NONE,
            slots: Vec::new(),
            next: 0,
            queue: VecDeque::new(),
        }
    }

    // Deletions by the receiver arrive as PropertyNotify events on its window.
    fn connect<C: HasConnection>(&mut self, c: &C, target: Window) -> Result<(), ClientError> {
        self.target = target;
        self.next = 0;
        self.queue.clear();
        c.conn().change_window_attributes(
            target,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )?;
        Ok(())
    }

    fn disconnect(&mut self) {
        self.target = x11rb::NONE;
        self.queue.clear();
    }

    fn send<C: HasConnection>(
        &mut self,
        c: &C,
        atoms: &Atoms<Atom>,
        transport_max: usize,
        packet: Vec<u8>,
    ) -> Result<(), ClientError> {
        self.flush(c, atoms, transport_max)?;
        if self.queue.is_empty() && self.try_send(c, atoms, transport_max, &packet)? {
            c.conn().flush()?;
            return Ok(());
        }
        if self.queue.len() >= MAX_QUEUED_PACKETS {
            return Err(ClientError::TransportBusy);
        }
        self.queue.push_back(packet);
        Ok(())
    }

    fn flush<C: HasConnection>(
        &mut self,
        c: &C,
        atoms: &Atoms<Atom>,
        transport_max: usize,
    ) -> Result<(), ClientError> {
        if self.queue.is_empty() {
            return Ok(());
        }
        while let Some(packet) = self.queue.pop_front() {
            if !self.try_send(c, atoms, transport_max, &packet)? {
                self.queue.push_front(packet);
                break;
            }
        }
        c.conn().flush()?;
        Ok(())
    }

    // Returns false while the next property still holds an unread packet.
    fn try_send<C: HasConnection>(
        &mut self,
        c: &C,
        atoms: &Atoms<Atom>,
        transport_max: usize,
        packet: &[u8],
    ) -> Result<bool, ClientError> {
        if packet.len() < transport_max {
            send_inline::<_, ClientError>(c, atoms, self.target, packet)?;
            return Ok(true);
        }
        if self.slots.is_empty() {
            let cookies = (0..PROPERTY_ATOM_COUNT)
                .map(|i| {
                    c.conn()
                        .intern_atom(false, format!("_XIM_DATA_{}", i).as_bytes())
                })
                .collect::<Result<Vec<_>, _>>()?;
            for cookie in cookies {
                self.slots.push(cookie.reply()?.atom);
            }
        }
        // Names are taken in order, so the next one is the oldest outstanding
        // packet. Only this client writes it; the receiver can only delete it.
        let prop = self.slots[self.next];
        let pending = match c
            .conn()
            .get_property(false, self.target, prop, AtomEnum::ANY, 0, 0)?
            .reply()
        {
            Ok(reply) => reply.type_ != x11rb::NONE,
            Err(ReplyError::X11Error(e)) if e.error_kind == ErrorKind::Window => {
                return Err(ClientError::NoXimServer)
            }
            Err(e) => return Err(e.into()),
        };
        if pending {
            return Ok(false);
        }
        self.next = (self.next + 1) % PROPERTY_ATOM_COUNT;
        send_property::<_, ClientError>(c, atoms, self.target, prop, packet)?;
        Ok(true)
    }
}

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

fn log_req(req: &Request) {
    if log::log_enabled!(log::Level::Trace) {
        log::trace!("->: {:?}", req);
    } else {
        log::debug!("->: {}", req.name());
    }
}

// The server keeps its original one-name-per-packet allocation: clients read
// each property from offset zero, and the server sends rarely enough that the
// names stay far below the client's per-keystroke rate.
#[cfg(feature = "x11rb-server")]
fn send_req_impl<C: HasConnection, E: From<ConnectionError> + From<ReplyError>>(
    c: &C,
    atoms: &Atoms<Atom>,
    target: Window,
    buf: &mut Vec<u8>,
    sequence: &mut u16,
    transport_max: usize,
    req: &Request,
) -> Result<(), E> {
    log_req(req);
    buf.resize(req.size(), 0);
    xim_parser::write(req, buf);

    if buf.len() < transport_max {
        send_inline::<_, E>(c, atoms, target, buf)?;
    } else {
        let prop = c
            .conn()
            .intern_atom(false, format!("_XIM_DATA_{}", sequence).as_bytes())?
            .reply()?
            .atom;
        *sequence = sequence.wrapping_add(1);
        send_property::<_, E>(c, atoms, target, prop, buf)?;
    }
    buf.clear();
    c.conn().flush()?;
    Ok(())
}

fn send_inline<C: HasConnection, E: From<ConnectionError>>(
    c: &C,
    atoms: &Atoms<Atom>,
    target: Window,
    packet: &[u8],
) -> Result<(), E> {
    if packet.len() > 20 {
        todo!("multi-CM");
    }
    let mut data = [0; 20];
    data[..packet.len()].copy_from_slice(packet);
    c.conn().send_event(
        false,
        target,
        EventMask::NO_EVENT,
        ClientMessageEvent {
            response_type: CLIENT_MESSAGE_EVENT,
            data: data.into(),
            format: 8,
            sequence: 0,
            type_: atoms.XIM_PROTOCOL,
            window: target,
        },
    )?;
    Ok(())
}

fn send_property<C: HasConnection, E: From<ConnectionError>>(
    c: &C,
    atoms: &Atoms<Atom>,
    target: Window,
    prop: Atom,
    packet: &[u8],
) -> Result<(), E> {
    c.conn().change_property(
        PropMode::APPEND,
        target,
        prop,
        AtomEnum::STRING,
        8,
        packet.len() as u32,
        packet,
    )?;
    c.conn().send_event(
        false,
        target,
        EventMask::NO_EVENT,
        ClientMessageEvent {
            data: [packet.len() as u32, prop, 0, 0, 0].into(),
            format: 32,
            sequence: 0,
            response_type: CLIENT_MESSAGE_EVENT,
            type_: atoms.XIM_PROTOCOL,
            window: target,
        },
    )?;
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

#[cfg(all(test, feature = "x11rb-client"))]
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

    // The receive path of the original revision (16f35a2, X11rbServer and
    // X11rbClient::handle_xim_protocol): read from offset zero, delete the
    // whole property, parse one request.
    fn legacy_read(conn: &RustConnection, msg: &ClientMessageEvent) -> Vec<u8> {
        let [length, atom, ..] = msg.data.as_data32();
        conn.get_property(true, msg.window, atom, AtomEnum::ANY, 0, length)
            .unwrap()
            .reply()
            .unwrap()
            .value
    }

    fn packet(round: usize, i: usize) -> Vec<u8> {
        // SetIcFocus fits in a ClientMessage; Open goes through a property.
        let req = if i % 5 == 0 {
            Request::SetIcFocus {
                input_method_id: round as u16,
                input_context_id: i as u16,
            }
        } else {
            Request::Open {
                locale: format!("中文输入保持完整-{}-{}-{}", round, i, "x".repeat(i % 31)),
            }
        };
        let mut bytes = alloc::vec![0; req.size()];
        xim_parser::write(&req, &mut bytes);
        bytes
    }

    struct NoopHandler;
    impl<C: crate::Client> ClientHandler<C> for NoopHandler {}

    fn client_message(window: Window, type_: Atom, data: [u32; 5]) -> ClientMessageEvent {
        ClientMessageEvent::new(32, window, type_, data)
    }

    // Drives the real X11rbClient (filter_event, PropertyNotify, DisconnectReply)
    // against a server that reads every property the way 16f35a2 does.
    #[test]
    fn client_interoperates_with_unchanged_receiver_across_reconnects() {
        let (_display, address) = private_display();
        let (server, screen) = x11rb::connect(Some(&address)).unwrap();
        let (conn, _) = x11rb::connect(Some(&address)).unwrap();
        let atoms = Atoms::new::<ReplyError, _>(|name| {
            Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
        })
        .unwrap();
        let mut client = X11rbClient {
            has_conn: &conn,
            server_owner_window: x11rb::NONE,
            im_window: x11rb::NONE,
            server_atom: x11rb::NONE,
            atoms: Atoms::new::<ReplyError, _>(|name| {
                Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
            })
            .unwrap(),
            transport_max: 20,
            client_window: window(&conn, screen),
            im_attributes: AHashMap::with_hasher(Default::default()),
            ic_attributes: AHashMap::with_hasher(Default::default()),
            transport: PropertyTransport::new(),
            property_offsets: PropertyOffsets::with_hasher(Default::default()),
        };
        let pump = |client: &mut X11rbClient<&RustConnection>| {
            conn.get_input_focus().unwrap().reply().unwrap();
            while let Some(event) = conn.poll_for_event().unwrap() {
                client.filter_event(&event, &mut NoopHandler).unwrap();
            }
        };
        let mut all_atoms = HashSet::new();
        for round in 0..3 {
            // A new server window per connection, as XIM servers do.
            let im_window = window(&server, screen);
            server
                .send_event(
                    false,
                    client.client_window,
                    EventMask::NO_EVENT,
                    client_message(
                        client.client_window,
                        atoms.XIM_XCONNECT,
                        [im_window, 0, 2, 20, 0],
                    ),
                )
                .unwrap();
            server.flush().unwrap();
            pump(&mut client);
            assert_eq!(client.im_window, im_window);

            let mut expected = alloc::vec![xim_parser::write_to_vec(Request::Connect {
                client_major_protocol_version: 1,
                client_minor_protocol_version: 0,
                endian: xim_parser::Endian::Native,
                client_auth_protocol_names: Vec::new(),
            })];
            for i in 0..300 {
                let bytes = packet(round, i);
                client.send_req(xim_parser::read(&bytes).unwrap()).unwrap();
                expected.push(bytes);
            }
            conn.get_input_focus().unwrap().reply().unwrap();
            assert!(client.transport.queue.len() > PROPERTY_ATOM_COUNT);

            let mut expected = expected.into_iter().peekable();
            while let Some(bytes) = expected.peek() {
                let msg = match server.poll_for_event().unwrap() {
                    Some(Event::ClientMessage(msg)) => msg,
                    Some(_) => continue,
                    None => {
                        pump(&mut client);
                        continue;
                    }
                };
                assert_eq!(msg.window, im_window);
                let actual = if msg.format == 32 {
                    all_atoms.insert(msg.data.as_data32()[1]);
                    legacy_read(&server, &msg)
                } else {
                    msg.data.as_data8()[..bytes.len()].to_vec()
                };
                assert_eq!(
                    &actual, bytes,
                    "queued XIM packet changed, lost or arrived out of order"
                );
                expected.next();
            }
            assert!(client.transport.queue.is_empty());

            // Disconnect, then reconnect in the next round.
            let mut reply = [0; 20];
            xim_parser::write(Request::DisconnectReply {}, &mut reply);
            server
                .send_event(
                    false,
                    client.client_window,
                    EventMask::NO_EVENT,
                    ClientMessageEvent::new(8, client.client_window, atoms.XIM_PROTOCOL, reply),
                )
                .unwrap();
            server.flush().unwrap();
            pump(&mut client);
            assert_eq!(client.transport.target, x11rb::NONE);
        }
        assert_eq!(all_atoms.len(), PROPERTY_ATOM_COUNT);
    }

    #[test]
    fn stalled_receiver_gets_bounded_queue() {
        let (_display, address) = private_display();
        let (receiver, screen) = x11rb::connect(Some(&address)).unwrap();
        let target = window(&receiver, screen);
        let atoms = Atoms::new::<ReplyError, _>(|name| {
            Ok(receiver.intern_atom(false, name.as_bytes())?.reply()?.atom)
        })
        .unwrap();
        let mut transport = PropertyTransport::new();
        transport.connect(&receiver, target).unwrap();
        let long = packet(0, 1);
        for _ in 0..PROPERTY_ATOM_COUNT + MAX_QUEUED_PACKETS {
            transport.send(&receiver, &atoms, 20, long.clone()).unwrap();
        }
        assert_eq!(transport.queue.len(), MAX_QUEUED_PACKETS);
        assert!(matches!(
            transport.send(&receiver, &atoms, 20, long.clone()),
            Err(ClientError::TransportBusy)
        ));
        // A disconnect drops packets addressed to the old server window.
        transport.disconnect();
        assert!(transport.queue.is_empty());
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
