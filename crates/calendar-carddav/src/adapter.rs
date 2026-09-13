//! PostgreSQL-backed guarded filesystem adapter for dav-server-rs's CardDAV
//! support, mirroring calendar-caldav::adapter::PgDavFs.
//!
//! URL layout:
//!   /contacts                                — root, one dirent per
//!                                              accessible address book named
//!                                              "{username}/{slug}"
//!   /contacts/{user}/{slug}                  — address book collection
//!                                              (personal, or the virtual
//!                                              "directory" book)
//!   /contacts/{user}/{slug}/{uid}.vcf        — one VCARD resource
//!
//! The directory book is never stored: it is projected live from
//! tenant_members+users and rejects writes.

use calendar_db::{self as db};
use chrono::{DateTime, Utc};
use dav_server::davpath::DavPath;
use dav_server::fs::{
    DavDirEntry, DavFile, DavMetaData, FsError, FsFuture, FsResult, FsStream, GuardedFileSystem,
    OpenOptions, ReadDirMeta,
};
use futures_util::stream;
use sqlx::PgPool;
use std::io::SeekFrom;
use std::sync::Arc;
use std::time::SystemTime;

/// Request credentials (mirrors calendar_caldav::adapter::DavAuth — kept
/// separate rather than shared to avoid coupling the two DAV crates).
#[derive(Debug, Clone)]
pub struct DavAuth {
    pub user: db::UserRow,
}

#[derive(Clone)]
pub struct PgAddressBookFs {
    pub pool: PgPool,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Location {
    Root,
    User,
    AddressBook(String),
    Contact(String, String), // slug, uid
}

pub(crate) fn parse_location(path: &DavPath) -> Option<Location> {
    let url = path.as_url_string();
    let segments: Vec<&str> = url
        .trim_start_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    match segments.as_slice() {
        ["contacts"] => Some(Location::Root),
        ["contacts", _user] => Some(Location::User),
        ["contacts", _user, slug] => Some(Location::AddressBook(slug.to_string())),
        ["contacts", _user, slug, file] => Some(Location::Contact(
            slug.to_string(),
            file.trim_end_matches(".vcf").to_string(),
        )),
        _ => None,
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Meta {
    pub len: u64,
    pub modified: SystemTime,
    pub created: SystemTime,
    pub etag: String,
    pub dir: bool,
    pub addressbook: bool,
}

impl Meta {
    fn dir(now: DateTime<Utc>) -> Self {
        Self {
            len: 0,
            modified: now.into(),
            created: now.into(),
            etag: String::new(),
            dir: true,
            addressbook: false,
        }
    }
}

impl DavMetaData for Meta {
    fn len(&self) -> u64 {
        self.len
    }
    fn modified(&self) -> FsResult<SystemTime> {
        Ok(self.modified)
    }
    fn is_dir(&self) -> bool {
        self.dir
    }
    fn is_calendar(&self, _path: &DavPath) -> bool {
        false
    }
    fn is_addressbook(&self, _path: &DavPath) -> bool {
        self.addressbook
    }
    fn etag(&self) -> Option<String> {
        (!self.etag.is_empty()).then(|| self.etag.clone())
    }
    fn created(&self) -> FsResult<SystemTime> {
        Ok(self.created)
    }
}

struct Entry {
    name: Vec<u8>,
    meta: Meta,
}

impl DavDirEntry for Entry {
    fn name(&self) -> Vec<u8> {
        self.name.clone()
    }
    fn metadata(&'_ self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        Box::pin(std::future::ready(Ok(Box::new(Meta {
            len: self.meta.len,
            modified: self.meta.modified,
            created: self.meta.created,
            etag: self.meta.etag.clone(),
            dir: self.meta.dir,
            addressbook: self.meta.addressbook,
        }) as Box<dyn DavMetaData>)))
    }
}

fn fs_err(e: db::DbError) -> FsError {
    match e {
        db::DbError::NotFound => FsError::NotFound,
        db::DbError::Conflict(_) => FsError::Exists,
        _ => FsError::GeneralFailure,
    }
}

/// A resolved address book: either a real personal book, or the virtual,
/// read-only tenant directory.
pub(crate) enum Book {
    Personal(db::contacts::AddressBookRow),
    Directory { tenant_id: uuid::Uuid },
}

impl Book {
    fn writable(&self) -> bool {
        matches!(self, Book::Personal(_))
    }
}

impl PgAddressBookFs {
    async fn address_book_by_slug(&self, creds: &DavAuth, slug: &str) -> FsResult<Book> {
        if slug == db::contacts::DIRECTORY_SLUG {
            let tenant_id = db::find_personal_tenant(&self.pool, creds.user.id)
                .await
                .map_err(fs_err)?;
            return Ok(Book::Directory { tenant_id });
        }
        match db::contacts::get_address_book_by_slug(&self.pool, creds.user.id, slug).await {
            Ok(row) => Ok(Book::Personal(row)),
            Err(db::DbError::NotFound) if slug == "contacts" => {
                let tenant_id = db::find_personal_tenant(&self.pool, creds.user.id)
                    .await
                    .map_err(fs_err)?;
                let row =
                    db::contacts::ensure_default_address_book(&self.pool, tenant_id, creds.user.id)
                        .await
                        .map_err(fs_err)?;
                Ok(Book::Personal(row))
            }
            Err(e) => Err(fs_err(e)),
        }
    }

    async fn book_ctag(&self, book: &Book) -> String {
        match book {
            Book::Personal(row) => format!("ctag-{}", row.ctag),
            Book::Directory { tenant_id } => db::contacts::directory_ctag(&self.pool, *tenant_id)
                .await
                .map(|c| format!("ctag-{c}"))
                .unwrap_or_default(),
        }
    }

    /// Raw vCard text + etag for one contact resource, whichever kind of book
    /// it lives in.
    async fn contact_body(
        &self,
        book: &Book,
        uid: &str,
    ) -> FsResult<(String, String, DateTime<Utc>)> {
        match book {
            Book::Personal(row) => {
                let contact = db::contacts::get_contact_by_uid(&self.pool, row.id, uid)
                    .await
                    .map_err(fs_err)?;
                Ok((
                    contact.raw_vcard,
                    contact.etag.trim_matches('"').to_string(),
                    contact.updated_at,
                ))
            }
            Book::Directory { tenant_id } => {
                let entries = db::contacts::directory_entries(&self.pool, *tenant_id)
                    .await
                    .map_err(fs_err)?;
                let entry = entries
                    .into_iter()
                    .find(|e| e.user_id.to_string() == uid)
                    .ok_or(FsError::NotFound)?;
                let vcard = crate::vcard::write_vcard_3_0(
                    &entry.user_id.to_string(),
                    "individual",
                    entry.display_name.as_deref().unwrap_or(&entry.username),
                    None,
                    None,
                    None,
                    None,
                    &[db::contacts::NewEmail {
                        email: entry.email.clone(),
                        kind: Some("work".into()),
                        is_primary: true,
                    }],
                    &[],
                    &[],
                );
                let etag = format!("dir-{}", entry.updated_at.timestamp());
                Ok((vcard, etag, entry.updated_at))
            }
        }
    }

    async fn resolve(&self, creds: &DavAuth, path: &DavPath) -> FsResult<(Location, Meta)> {
        let now = Utc::now();
        let location = parse_location(path).ok_or(FsError::NotFound)?;
        match &location {
            Location::Root | Location::User => Ok((location, Meta::dir(now))),
            Location::AddressBook(slug) => {
                let book = self.address_book_by_slug(creds, slug).await?;
                let etag = self.book_ctag(&book).await;
                Ok((
                    location,
                    Meta {
                        len: 0,
                        modified: now.into(),
                        created: now.into(),
                        etag,
                        dir: true,
                        addressbook: true,
                    },
                ))
            }
            Location::Contact(slug, uid) => {
                let book = self.address_book_by_slug(creds, slug).await?;
                let (vcard, etag, updated_at) = self.contact_body(&book, uid).await?;
                Ok((
                    location,
                    Meta {
                        len: vcard.len() as u64,
                        modified: updated_at.into(),
                        created: updated_at.into(),
                        etag,
                        dir: false,
                        addressbook: false,
                    },
                ))
            }
        }
    }
}

impl GuardedFileSystem<DavAuth> for PgAddressBookFs {
    fn metadata<'a>(
        &'a self,
        path: &'a DavPath,
        creds: &'a DavAuth,
    ) -> FsFuture<'a, Box<dyn DavMetaData>> {
        Box::pin(async move {
            let (_, meta) = self.resolve(creds, path).await?;
            Ok(Box::new(meta) as Box<dyn DavMetaData>)
        })
    }

    fn open<'a>(
        &'a self,
        path: &'a DavPath,
        options: OpenOptions,
        creds: &'a DavAuth,
    ) -> FsFuture<'a, Box<dyn DavFile>> {
        Box::pin(async move {
            let location = parse_location(path).ok_or(FsError::NotFound)?;
            match location {
                Location::Contact(slug, uid) => {
                    let book = self.address_book_by_slug(creds, &slug).await?;
                    let reading = options.read && !options.write;
                    if reading {
                        let (vcard, etag, updated_at) = self.contact_body(&book, &uid).await?;
                        let meta = Meta {
                            len: vcard.len() as u64,
                            modified: updated_at.into(),
                            created: updated_at.into(),
                            etag,
                            dir: false,
                            addressbook: false,
                        };
                        return Ok(Box::new(ReadFile {
                            content: vcard.into_bytes().into(),
                            pos: 0,
                            meta: Arc::new(meta),
                        }) as Box<dyn DavFile>);
                    }
                    if options.write {
                        if !book.writable() {
                            return Err(FsError::Forbidden);
                        }
                        let Book::Personal(address_book) = book else {
                            return Err(FsError::Forbidden);
                        };
                        let existing =
                            db::contacts::get_contact_by_uid(&self.pool, address_book.id, &uid)
                                .await
                                .is_ok();
                        if existing && options.create_new {
                            return Err(FsError::Exists);
                        }
                        if !existing && !options.create {
                            return Err(FsError::NotFound);
                        }
                        return Ok(Box::new(WriteFile {
                            pool: self.pool.clone(),
                            address_book_id: address_book.id,
                            uid,
                            buffer: Vec::new(),
                            new_meta: None,
                        }) as Box<dyn DavFile>);
                    }
                    Err(FsError::NotImplemented)
                }
                Location::AddressBook(_) | Location::User => Err(FsError::Forbidden),
                Location::Root => Err(FsError::NotFound),
            }
        })
    }

    fn read_dir<'a>(
        &'a self,
        path: &'a DavPath,
        _meta: ReadDirMeta,
        creds: &'a DavAuth,
    ) -> FsFuture<'a, FsStream<Box<dyn DavDirEntry>>> {
        Box::pin(async move {
            let location = parse_location(path).ok_or(FsError::NotFound)?;
            let entries: Vec<Entry> = match &location {
                Location::Root | Location::User => {
                    let tenant_id = db::find_personal_tenant(&self.pool, creds.user.id)
                        .await
                        .map_err(fs_err)?;
                    // Always at least the lazily-provisioned default personal
                    // book, so discovery never comes back empty.
                    let mut books =
                        db::contacts::list_address_books_for_user(&self.pool, creds.user.id)
                            .await
                            .map_err(fs_err)?;
                    if books.is_empty() {
                        books.push(
                            db::contacts::ensure_default_address_book(
                                &self.pool,
                                tenant_id,
                                creds.user.id,
                            )
                            .await
                            .map_err(fs_err)?,
                        );
                    }
                    let prefix = if matches!(location, Location::Root) {
                        creds.user.username.as_str()
                    } else {
                        ""
                    };
                    let mut entries: Vec<Entry> = books
                        .into_iter()
                        .map(|book| Entry {
                            name: format!("{prefix}/{}", book.slug).into_bytes(),
                            meta: Meta {
                                len: 0,
                                modified: book.updated_at.into(),
                                created: book.created_at.into(),
                                etag: format!("ctag-{}", book.ctag),
                                dir: true,
                                addressbook: true,
                            },
                        })
                        .collect();
                    let dir_ctag = db::contacts::directory_ctag(&self.pool, tenant_id)
                        .await
                        .unwrap_or_default();
                    entries.push(Entry {
                        name: format!("{prefix}/{}", db::contacts::DIRECTORY_SLUG).into_bytes(),
                        meta: Meta {
                            len: 0,
                            modified: Utc::now().into(),
                            created: Utc::now().into(),
                            etag: format!("ctag-{dir_ctag}"),
                            dir: true,
                            addressbook: true,
                        },
                    });
                    entries
                }
                Location::AddressBook(slug) => {
                    let book = self.address_book_by_slug(creds, slug).await?;
                    match &book {
                        Book::Personal(row) => {
                            let contacts = db::contacts::list_contacts(&self.pool, row.id)
                                .await
                                .map_err(fs_err)?;
                            contacts
                                .into_iter()
                                .map(|c| Entry {
                                    name: format!("{}.vcf", c.uid).into_bytes(),
                                    meta: Meta {
                                        len: c.raw_vcard.len() as u64,
                                        modified: c.updated_at.into(),
                                        created: c.created_at.into(),
                                        etag: c.etag.trim_matches('"').to_string(),
                                        dir: false,
                                        addressbook: false,
                                    },
                                })
                                .collect()
                        }
                        Book::Directory { tenant_id } => {
                            let people = db::contacts::directory_entries(&self.pool, *tenant_id)
                                .await
                                .map_err(fs_err)?;
                            people
                                .into_iter()
                                .map(|p| Entry {
                                    name: format!("{}.vcf", p.user_id).into_bytes(),
                                    meta: Meta {
                                        len: 0,
                                        modified: p.updated_at.into(),
                                        created: p.updated_at.into(),
                                        etag: format!("dir-{}", p.updated_at.timestamp()),
                                        dir: false,
                                        addressbook: false,
                                    },
                                })
                                .collect()
                        }
                    }
                }
                Location::Contact(_, _) => return Err(FsError::NotFound),
            };
            let boxed: Vec<std::result::Result<Box<dyn DavDirEntry>, FsError>> = entries
                .into_iter()
                .map(|e| Ok(Box::new(e) as Box<dyn DavDirEntry>))
                .collect();
            Ok(Box::pin(stream::iter(boxed)) as FsStream<Box<dyn DavDirEntry>>)
        })
    }

    fn create_dir<'a>(&'a self, path: &'a DavPath, creds: &'a DavAuth) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let slug = match parse_location(path).ok_or(FsError::NotFound)? {
                Location::AddressBook(slug) => slug,
                Location::User => return Err(FsError::Exists),
                _ => return Err(FsError::Forbidden),
            };
            if slug == db::contacts::DIRECTORY_SLUG {
                return Err(FsError::Forbidden);
            }
            calendar_core_validate_slug(&slug)?;
            let tenant_id = db::find_personal_tenant(&self.pool, creds.user.id)
                .await
                .map_err(fs_err)?;
            db::contacts::create_address_book(&self.pool, tenant_id, creds.user.id, &slug, &slug)
                .await
                .map_err(|e| match e {
                    db::DbError::Conflict(_) => FsError::Exists,
                    other => fs_err(other),
                })?;
            Ok(())
        })
    }

    fn remove_dir<'a>(&'a self, path: &'a DavPath, creds: &'a DavAuth) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let Location::AddressBook(slug) = parse_location(path).ok_or(FsError::NotFound)? else {
                return Err(FsError::Forbidden);
            };
            match self.address_book_by_slug(creds, &slug).await? {
                Book::Personal(row) => db::contacts::soft_delete_address_book(&self.pool, row.id)
                    .await
                    .map_err(fs_err),
                Book::Directory { .. } => Err(FsError::Forbidden),
            }
        })
    }

    fn remove_file<'a>(&'a self, path: &'a DavPath, creds: &'a DavAuth) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let Location::Contact(slug, uid) = parse_location(path).ok_or(FsError::NotFound)?
            else {
                return Err(FsError::Forbidden);
            };
            let book = self.address_book_by_slug(creds, &slug).await?;
            let Book::Personal(row) = book else {
                return Err(FsError::Forbidden);
            };
            let contact = db::contacts::get_contact_by_uid(&self.pool, row.id, &uid)
                .await
                .map_err(fs_err)?;
            db::contacts::soft_delete_contact(&self.pool, contact.id)
                .await
                .map_err(fs_err)
        })
    }

    fn rename<'a>(
        &'a self,
        _from: &'a DavPath,
        _to: &'a DavPath,
        _creds: &'a DavAuth,
    ) -> FsFuture<'a, ()> {
        Box::pin(std::future::ready(Err(FsError::NotImplemented)))
    }

    fn copy<'a>(
        &'a self,
        _from: &'a DavPath,
        _to: &'a DavPath,
        _creds: &'a DavAuth,
    ) -> FsFuture<'a, ()> {
        Box::pin(std::future::ready(Err(FsError::NotImplemented)))
    }

    fn set_accessed<'a>(
        &'a self,
        _path: &'a DavPath,
        _tm: SystemTime,
        _creds: &DavAuth,
    ) -> FsFuture<'a, ()> {
        Box::pin(std::future::ready(Ok(())))
    }

    fn set_modified<'a>(
        &'a self,
        _path: &'a DavPath,
        _tm: SystemTime,
        _creds: &DavAuth,
    ) -> FsFuture<'a, ()> {
        Box::pin(std::future::ready(Ok(())))
    }

    fn have_props<'a>(
        &'a self,
        _path: &'a DavPath,
        _creds: &'a DavAuth,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        Box::pin(std::future::ready(true))
    }

    fn patch_props<'a>(
        &'a self,
        path: &'a DavPath,
        patch: Vec<(bool, dav_server::fs::DavProp)>,
        creds: &'a DavAuth,
    ) -> FsFuture<'a, Vec<(http::StatusCode, dav_server::fs::DavProp)>> {
        Box::pin(async move {
            let slug = match parse_location(path).ok_or(FsError::NotFound)? {
                Location::AddressBook(slug) => slug,
                _ => return Err(FsError::Forbidden),
            };
            let Book::Personal(row) = self.address_book_by_slug(creds, &slug).await? else {
                return Err(FsError::Forbidden);
            };
            for (set, prop) in &patch {
                if *set
                    && prop.name == "displayname"
                    && let Some(value) = prop.xml.as_deref().and_then(xml_text)
                {
                    db::contacts::rename_address_book(&self.pool, row.id, &value)
                        .await
                        .map_err(fs_err)?;
                }
            }
            Ok(patch
                .into_iter()
                .filter(|(set, prop)| *set && prop.name == "displayname")
                .map(|(_, prop)| (http::StatusCode::OK, prop))
                .collect())
        })
    }

    fn get_props<'a>(
        &'a self,
        path: &'a DavPath,
        _do_content: bool,
        creds: &'a DavAuth,
    ) -> FsFuture<'a, Vec<dav_server::fs::DavProp>> {
        Box::pin(async move {
            let Location::AddressBook(slug) = parse_location(path).ok_or(FsError::NotFound)? else {
                return Ok(vec![]);
            };
            let name = match self.address_book_by_slug(creds, &slug).await? {
                Book::Personal(row) => row.name,
                Book::Directory { .. } => "Directory".to_string(),
            };
            Ok(vec![dav_server::fs::DavProp::new(
                "displayname".into(),
                "D".into(),
                "DAV:".into(),
                xml_escape(&name),
            )])
        })
    }

    fn get_quota<'a>(&'a self, _creds: &'a DavAuth) -> FsFuture<'a, (u64, Option<u64>)> {
        Box::pin(std::future::ready(Ok((0, Some(u64::MAX / 2)))))
    }
}

fn calendar_core_validate_slug(slug: &str) -> FsResult<()> {
    // Same shape as calendar slugs (a-z0-9-); avoids a calendar-core dep for
    // one regex-equivalent check.
    let ok = !slug.is_empty()
        && slug.len() <= 63
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && slug.chars().next().is_some_and(|c| c != '-');
    if ok { Ok(()) } else { Err(FsError::Forbidden) }
}

/// File for GET/HEAD reads: content is the verbatim (or projected) vCard text.
#[derive(Debug)]
struct ReadFile {
    content: bytes::Bytes,
    pos: u64,
    meta: Arc<Meta>,
}

impl DavFile for ReadFile {
    fn metadata(&'_ mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        let meta = self.meta.clone();
        Box::pin(async move { Ok(Box::new(Meta { ..(*meta).clone() }) as Box<dyn DavMetaData>) })
    }
    fn write_buf(&'_ mut self, _buf: Box<dyn bytes::Buf + Send>) -> FsFuture<'_, ()> {
        Box::pin(std::future::ready(Err(FsError::Forbidden)))
    }
    fn write_bytes(&'_ mut self, _bytes: bytes::Bytes) -> FsFuture<'_, ()> {
        Box::pin(std::future::ready(Err(FsError::Forbidden)))
    }
    fn read_bytes(&'_ mut self, count: usize) -> FsFuture<'_, bytes::Bytes> {
        let content = self.content.clone();
        Box::pin(async move {
            let start = self.pos.min(content.len() as u64) as usize;
            let end = (start + count).min(content.len());
            self.pos = end as u64;
            Ok(content.slice(start..end))
        })
    }
    fn seek(&'_ mut self, pos: SeekFrom) -> FsFuture<'_, u64> {
        let len = self.content.len() as u64;
        Box::pin(async move {
            let target: Option<u64> = match pos {
                SeekFrom::Start(n) => Some(n),
                SeekFrom::Current(n) => {
                    let signed = self.pos as i64;
                    signed.checked_add(n).filter(|v| *v >= 0).map(|v| v as u64)
                }
                SeekFrom::End(n) => (len as i64)
                    .checked_add(n)
                    .filter(|v| *v >= 0)
                    .map(|v| v as u64),
            };
            let target = target.ok_or(FsError::GeneralFailure)?;
            self.pos = target.min(len);
            Ok(self.pos)
        })
    }
    fn flush(&'_ mut self) -> FsFuture<'_, ()> {
        Box::pin(std::future::ready(Ok(())))
    }
}

/// File for PUT: buffers the body, parses+stores one VCARD on flush.
#[derive(Debug)]
struct WriteFile {
    pool: PgPool,
    address_book_id: uuid::Uuid,
    uid: String,
    buffer: Vec<u8>,
    new_meta: Option<Meta>,
}

impl DavFile for WriteFile {
    fn metadata(&'_ mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        let meta = self.new_meta.clone();
        Box::pin(async move {
            meta.map(|m| Box::new(m) as Box<dyn DavMetaData>)
                .ok_or(FsError::NotFound)
        })
    }
    fn write_buf(&'_ mut self, mut buf: Box<dyn bytes::Buf + Send>) -> FsFuture<'_, ()> {
        Box::pin(async move {
            self.buffer
                .extend_from_slice(buf.copy_to_bytes(buf.remaining()).as_ref());
            Ok(())
        })
    }
    fn write_bytes(&'_ mut self, bytes: bytes::Bytes) -> FsFuture<'_, ()> {
        Box::pin(async move {
            self.buffer.extend_from_slice(&bytes);
            Ok(())
        })
    }
    fn read_bytes(&'_ mut self, _count: usize) -> FsFuture<'_, bytes::Bytes> {
        Box::pin(std::future::ready(Err(FsError::Forbidden)))
    }
    fn seek(&'_ mut self, _pos: SeekFrom) -> FsFuture<'_, u64> {
        Box::pin(std::future::ready(Ok(0)))
    }
    fn flush(&'_ mut self) -> FsFuture<'_, ()> {
        Box::pin(async move {
            let text = match std::str::from_utf8(&self.buffer) {
                Ok(text) => text,
                Err(_) => {
                    tracing::warn!("CardDAV PUT body is not UTF-8");
                    return Err(FsError::Forbidden);
                }
            };
            let cards = match crate::vcard::parse_vcard(text) {
                Ok(cards) if !cards.is_empty() => cards,
                _ => {
                    tracing::warn!("CardDAV PUT body is not a valid vCard");
                    return Err(FsError::Forbidden);
                }
            };
            let mut card = cards.into_iter().next().unwrap();
            card.uid = self.uid.clone(); // the URL is the identity, not the card's own UID line
            let data = card
                .into_new_contact(text.to_string())
                .map_err(|_| FsError::Forbidden)?;
            let contact = db::contacts::upsert_contact(&self.pool, self.address_book_id, &data)
                .await
                .map_err(|e| {
                    tracing::warn!(error = %e, "CardDAV PUT failed to store contact");
                    fs_err(e)
                })?;
            self.new_meta = Some(Meta {
                len: contact.raw_vcard.len() as u64,
                modified: contact.updated_at.into(),
                created: contact.created_at.into(),
                etag: contact.etag.trim_matches('"').to_string(),
                dir: false,
                addressbook: false,
            });
            Ok(())
        })
    }
}

fn xml_text(value: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(value).ok()?;
    Some(text.trim().to_string())
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
