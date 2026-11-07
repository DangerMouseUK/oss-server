//! Built-in modules and the default module set.

use crate::module::ModuleCatalog;
use crate::net::Transport;
use crate::zone::Zone;

/// Modules loaded when the zone has no `conf/modules.conf` and the builder
/// was given no list.
pub const DEFAULT_MODULES: &[&str] = &[];

/// Every module shipped with this crate, under its own name and its ASSS name.
pub fn builtin_catalog() -> ModuleCatalog {
    ModuleCatalog::default()
}

/// The transport used when the builder isn't given one.
pub fn default_transport(_zone: &mut Zone) -> Box<dyn Transport> {
    Box::new(NullTransport)
}

/// Accepts no connections (placeholder until the `oss-net` transport lands).
struct NullTransport;

impl Transport for NullTransport {
    fn on_datagram(
        &mut self,
        _: &mut Zone,
        _: &dyn crate::net::DatagramIo,
        from: std::net::SocketAddr,
        data: &[u8],
    ) {
        tracing::debug!(target: "net", %from, len = data.len(), "no transport; dropping datagram");
    }

    fn flush(&mut self, zone: &mut Zone, _: &dyn crate::net::DatagramIo) {
        let out = zone.take_outbox();
        zone.return_outbox(out);
    }

    fn tick(&mut self, _: &mut Zone, _: &dyn crate::net::DatagramIo) {}
}
