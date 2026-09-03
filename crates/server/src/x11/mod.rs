//! X11 integration: connection, capture, input, cursor, resize, clipboard.

pub mod capture;
pub mod clipboard;
pub mod cursor;
pub mod input;
pub mod resize;

use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xproto::{self, ConnectionExt as _, ImageOrder};
use x11rb::protocol::{damage, randr, shm, xfixes, xtest};
use x11rb::rust_connection::RustConnection;

/// A connection to the session's X server plus the facts we need about it.
pub struct XDisplay {
    conn: RustConnection,
    screen_num: usize,
    root: xproto::Window,
    depth: u8,
    msb_first: bool,
    /// Current root size, updated by [`XDisplay::refresh_size`].
    size: Mutex<(u32, u32)>,
    /// Extension availability.
    pub ext: Extensions,
}

/// Which optional extensions the server provides.
#[derive(Clone, Copy, Debug, Default)]
pub struct Extensions {
    /// MIT-SHM (fast capture).
    pub shm: bool,
    /// DAMAGE (change tracking).
    pub damage: bool,
    /// XFIXES (regions, cursor images, selection events).
    pub xfixes: bool,
    /// XTEST (input injection).
    pub xtest: bool,
    /// RANDR (resizing).
    pub randr: bool,
}

impl XDisplay {
    /// Connect to `display` (e.g. `":7"`), retrying for up to `timeout`
    /// while the X server is still starting.
    pub fn connect(display: &str, timeout: Duration) -> Result<Self> {
        let start = Instant::now();
        loop {
            match RustConnection::connect(Some(display)) {
                Ok((conn, screen_num)) => return Self::from_connection(conn, screen_num),
                Err(e) => {
                    if start.elapsed() > timeout {
                        return Err(anyhow!("cannot connect to X display {display}: {e}"));
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }

    fn from_connection(conn: RustConnection, screen_num: usize) -> Result<Self> {
        let setup = conn.setup();
        let screen = setup
            .roots
            .get(screen_num)
            .ok_or_else(|| anyhow!("no such screen"))?;
        let root = screen.root;
        let depth = screen.root_depth;
        if depth != 24 && depth != 32 {
            bail!("unsupported root depth {depth}; LynxRDP needs a 24-bit TrueColor screen");
        }
        let msb_first = setup.image_byte_order == ImageOrder::MSB_FIRST;
        let size = (
            u32::from(screen.width_in_pixels),
            u32::from(screen.height_in_pixels),
        );

        let has = |name: &'static str| -> Result<bool> {
            Ok(conn
                .extension_information(name)
                .context("querying extension")?
                .is_some())
        };
        let mut ext = Extensions {
            shm: has(shm::X11_EXTENSION_NAME)?,
            damage: has(damage::X11_EXTENSION_NAME)?,
            xfixes: has(xfixes::X11_EXTENSION_NAME)?,
            xtest: has(xtest::X11_EXTENSION_NAME)?,
            randr: has(randr::X11_EXTENSION_NAME)?,
        };
        // Version negotiation: XFIXES and DAMAGE require it before use.
        if ext.xfixes {
            let v = xfixes::query_version(&conn, 5, 0)?
                .reply()
                .context("xfixes version")?;
            if v.major_version < 2 {
                log::warn!(
                    "XFIXES {}.{} too old; cursor/clipboard tracking disabled",
                    v.major_version,
                    v.minor_version
                );
                ext.xfixes = false;
            }
        }
        if ext.damage {
            damage::query_version(&conn, 1, 1)?
                .reply()
                .context("damage version")?;
        }
        if ext.randr {
            let v = randr::query_version(&conn, 1, 4)?
                .reply()
                .context("randr version")?;
            if (v.major_version, v.minor_version) < (1, 2) {
                log::warn!(
                    "RANDR {}.{} too old; resizing disabled",
                    v.major_version,
                    v.minor_version
                );
                ext.randr = false;
            }
        }
        if ext.xtest {
            xtest::get_version(&conn, 2, 2)?
                .reply()
                .context("xtest version")?;
        }
        if ext.shm {
            match shm::query_version(&conn)?.reply() {
                Ok(v) if v.shared_pixmaps || v.major_version >= 1 => {}
                Ok(_) => ext.shm = false,
                Err(e) => {
                    log::warn!("MIT-SHM unusable: {e}");
                    ext.shm = false;
                }
            }
        }
        log::debug!(
            "X display: root {root} {}x{} depth {depth} ext {:?}",
            size.0,
            size.1,
            ext
        );
        Ok(Self {
            conn,
            screen_num,
            root,
            depth,
            msb_first,
            size: Mutex::new(size),
            ext,
        })
    }

    /// The underlying connection.
    pub fn conn(&self) -> &RustConnection {
        &self.conn
    }

    /// Root window.
    pub fn root(&self) -> xproto::Window {
        self.root
    }

    /// Screen index.
    pub fn screen_num(&self) -> usize {
        self.screen_num
    }

    /// Root depth.
    pub fn depth(&self) -> u8 {
        self.depth
    }

    /// Whether the server sends images most significant byte first.
    pub fn msb_first(&self) -> bool {
        self.msb_first
    }

    /// Last known root size.
    pub fn size(&self) -> (u32, u32) {
        *self.size.lock().unwrap()
    }

    /// Ask the server for the current root size and remember it.
    pub fn refresh_size(&self) -> Result<(u32, u32)> {
        let geo = self
            .conn
            .get_geometry(self.root)?
            .reply()
            .context("root geometry")?;
        let size = (u32::from(geo.width), u32::from(geo.height));
        *self.size.lock().unwrap() = size;
        Ok(size)
    }

    /// Allocate a resource id.
    pub fn generate_id(&self) -> Result<u32> {
        Ok(self.conn.generate_id()?)
    }

    /// Flush the output buffer.
    pub fn flush(&self) -> Result<()> {
        Ok(self.conn.flush()?)
    }

    /// Round trip to the server.
    pub fn sync(&self) -> Result<()> {
        self.conn.get_input_focus()?.reply()?;
        Ok(())
    }

    /// Current pointer position on the root window.
    pub fn pointer_position(&self) -> Result<(i16, i16)> {
        let r = self.conn.query_pointer(self.root)?.reply()?;
        Ok((r.root_x, r.root_y))
    }

    /// Intern an atom.
    pub fn atom(&self, name: &str) -> Result<xproto::Atom> {
        Ok(self.conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
    }
}
