//! CardDAV adapter: vCard parse/serialize, the dav-server-rs guarded
//! filesystem adapter, and the sync-collection REPORT helper dav-server does
//! not implement (mirrors calendar-caldav).

pub mod adapter;
pub mod vcard;

pub use adapter::{DavAuth, PgAddressBookFs};
