//! PostgreSQL-backed guarded filesystem adapter for dav-server-rs
//! (docs/ARCHITECTURE.md): DAV operations translate into domain/repository
//! operations, never a filesystem-shaped database abstraction.
//!
//! URL layout (dav-server sees full paths, no prefix stripping):
//!   /calendars                          — root collection, one dirent per
//!                                         accessible calendar named
//!                                         "{username}/{slug}" so the
//!                                         calendar-home-set depth-1 PROPFIND
//!                                         surfaces calendar collections
//!   /calendars/{user}/{slug}            — calendar collection (MKCALENDAR)
//!   /calendars/{user}/{slug}/{uuid}.ics — one VEVENT resource (master or
//!                                         RECURRENCE-ID exception)

use crate::{ExportRow, events_to_ics};
use calendar_core::CalendarCapability;
use calendar_db::{self as db, CalendarRow, EventRow};
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

/// Request credentials: the authenticated user (app password / API token /
/// session) — authorization is enforced here at the resource boundary.
#[derive(Debug, Clone)]
pub struct DavAuth {
    pub user: db::UserRow,
}

#[derive(Clone)]
pub struct PgDavFs {
    pub pool: PgPool,
}

/// Parsed URL location under /calendars.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Location {
    Root,
    User,
    Calendar(String),       // slug
    Object(String, String), // slug, resource filename (decoded, as the client chose it)
}

/// `/calendars/{user}/{slug}` → Location. The user segment is always the
/// authenticated user's own namespace.
pub(crate) fn parse_location(path: &DavPath) -> Option<Location> {
    let url = path.as_url_string();
    let segments: Vec<&str> = url
        .trim_start_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    match segments.as_slice() {
        ["calendars"] => Some(Location::Root),
        ["calendars", _user] => Some(Location::User),
        ["calendars", _user, slug] => Some(Location::Calendar(slug.to_string())),
        ["calendars", _user, slug, _file] => path
            .file_name()
            .map(|name| Location::Object(slug.to_string(), name.to_string())),
        _ => None,
    }
}

/// Read-only DAV metadata snapshot.
#[derive(Debug, Clone)]
pub(crate) struct Meta {
    pub len: u64,
    pub modified: SystemTime,
    pub created: SystemTime,
    pub etag: String,
    pub dir: bool,
    pub calendar: bool,
}

impl Meta {
    #[allow(dead_code)]
    fn dir(now: DateTime<Utc>) -> Self {
        Self {
            len: 0,
            modified: now.into(),
            created: now.into(),
            etag: String::new(),
            dir: true,
            calendar: false,
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
        self.calendar
    }
    fn is_addressbook(&self, _path: &DavPath) -> bool {
        false // this mount only ever serves calendar collections
    }
    fn etag(&self) -> Option<String> {
        (!self.etag.is_empty()).then(|| self.etag.clone())
    }
    fn created(&self) -> FsResult<SystemTime> {
        Ok(self.created)
    }
}

/// Directory entry with its metadata already resolved (no N+1 re-query).
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
            calendar: self.meta.calendar,
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

fn capability_guard(cap: CalendarCapability, required: CalendarCapability) -> FsResult<()> {
    if cap.satisfies(required) {
        Ok(())
    } else {
        Err(FsError::Forbidden)
    }
}

impl PgDavFs {
    /// (calendar, capability) for a slug in the caller's own namespace.
    async fn calendar_by_slug(
        &self,
        creds: &DavAuth,
        slug: &str,
    ) -> FsResult<(CalendarRow, CalendarCapability)> {
        db::list_calendars_for_user(&self.pool, creds.user.id)
            .await
            .map_err(fs_err)?
            .into_iter()
            .find(|(cal, _)| cal.slug == slug)
            .ok_or(FsError::NotFound)
    }

    /// The series master served under `name`.
    async fn master_at(&self, calendar: &CalendarRow, name: &str) -> FsResult<EventRow> {
        let (event, _) = db::get_event_by_href(&self.pool, calendar.id, name)
            .await
            .map_err(fs_err)?;
        Ok(event)
    }

    /// The whole series as one VCALENDAR: the master, then its overrides.
    async fn series_ics(&self, master: &EventRow) -> FsResult<String> {
        let mut events = vec![master.clone()];
        events.extend(
            db::list_exceptions(&self.pool, &[master.id])
                .await
                .map_err(fs_err)?,
        );
        let mut rows = Vec::with_capacity(events.len());
        for event in events {
            rows.push(ExportRow {
                attendees: db::list_attendees(&self.pool, event.id)
                    .await
                    .map_err(fs_err)?,
                alarms: db::alarms::list_alarms(&self.pool, event.id)
                    .await
                    .unwrap_or_default(),
                location: db::location_for_event(&self.pool, &event).await,
                event,
            });
        }
        Ok(events_to_ics(&rows))
    }

    async fn resolve(&self, creds: &DavAuth, path: &DavPath) -> FsResult<(Location, Meta)> {
        let now = Utc::now();
        let location = parse_location(path).ok_or(FsError::NotFound)?;
        match &location {
            Location::Root => Ok((location, Meta::dir(now))),
            Location::User => Ok((location, Meta::dir(now))),
            Location::Calendar(slug) => {
                let (cal, _cap) = self.calendar_by_slug(creds, slug).await?;
                Ok((
                    location,
                    Meta {
                        len: 0,
                        modified: cal.updated_at.into(),
                        created: cal.created_at.into(),
                        etag: format!("ctag-{}", cal.ctag),
                        dir: true,
                        calendar: true,
                    },
                ))
            }
            Location::Object(slug, name) => {
                let (cal, cap) = self.calendar_by_slug(creds, slug).await?;
                capability_guard(cap, CalendarCapability::ReadOnly)?;
                let event = self.master_at(&cal, name).await?;
                let ics = self.series_ics(&event).await?;
                Ok((
                    location,
                    Meta {
                        len: ics.len() as u64,
                        modified: event.updated_at.into(),
                        created: event.created_at.into(),
                        etag: event.etag.trim_matches('"').to_string(),
                        dir: false,
                        calendar: false,
                    },
                ))
            }
        }
    }
}

impl GuardedFileSystem<DavAuth> for PgDavFs {
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
                Location::Object(slug, name) => {
                    let (cal, cap) = self.calendar_by_slug(creds, &slug).await?;
                    let reading = options.read && !options.write;
                    if reading {
                        capability_guard(cap, CalendarCapability::ReadOnly)?;
                        let event = self.master_at(&cal, &name).await?;
                        let ics = self.series_ics(&event).await?;
                        let modified: SystemTime = event.updated_at.into();
                        let meta = Meta {
                            len: ics.len() as u64,
                            modified,
                            created: event.created_at.into(),
                            etag: event.etag.trim_matches('"').to_string(),
                            dir: false,
                            calendar: false,
                        };
                        return Ok(Box::new(ReadFile {
                            content: ics.into_bytes().into(),
                            pos: 0,
                            meta: Arc::new(meta),
                        }) as Box<dyn DavFile>);
                    }
                    if options.write {
                        capability_guard(cap, CalendarCapability::ReadWrite)?;
                        let existing = db::get_event_by_href(&self.pool, cal.id, &name).await;
                        let exists = match &existing {
                            Ok(_) => true,
                            Err(db::DbError::NotFound) => false,
                            Err(_) => return Err(FsError::GeneralFailure),
                        };
                        if exists && options.create_new {
                            return Err(FsError::Exists); // If-None-Match: *
                        }
                        if !exists && !options.create {
                            return Err(FsError::NotFound); // PUT update of a gone resource
                        }
                        // Capture the etag here — the same metadata dav-server's
                        // If-Match check was evaluated against at request start —
                        // so flush() re-verifies it inside the write transaction
                        // (lost-update race). dav-server does not forward the
                        // If-Match header, so a bare PUT is indistinguishable
                        // from a specific-etag If-Match and gets the same
                        // within-request consistency check.
                        let precondition = if options.create_new {
                            db::ics_upsert::PutPrecondition::NotExists
                        } else {
                            match existing {
                                Ok((event, _)) => db::ics_upsert::PutPrecondition::MatchEtag(
                                    event.etag.trim_matches('"').to_string(),
                                ),
                                Err(db::DbError::NotFound) => db::ics_upsert::PutPrecondition::None,
                                Err(_) => return Err(FsError::GeneralFailure),
                            }
                        };
                        return Ok(Box::new(WriteFile {
                            pool: self.pool.clone(),
                            calendar: cal,
                            user: creds.user.clone(),
                            name,
                            buffer: Vec::new(),
                            new_meta: None,
                            precondition,
                        }) as Box<dyn DavFile>);
                    }
                    Err(FsError::NotImplemented)
                }
                Location::Calendar(_) | Location::User => Err(FsError::Forbidden),
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
                    // One dirent per accessible calendar, named "{user}/{slug}"
                    // so the home-set listing shows calendar collections.
                    db::list_calendars_for_user(&self.pool, creds.user.id)
                        .await
                        .map_err(fs_err)?
                        .into_iter()
                        .map(|(cal, _cap)| Entry {
                            name: format!(
                                "{}/{}",
                                if matches!(location, Location::Root) {
                                    creds.user.username.as_str()
                                } else {
                                    ""
                                },
                                cal.slug
                            )
                            .into_bytes(),
                            meta: Meta {
                                len: 0,
                                modified: cal.updated_at.into(),
                                created: cal.created_at.into(),
                                etag: format!("ctag-{}", cal.ctag),
                                dir: true,
                                calendar: true,
                            },
                        })
                        .collect()
                }
                Location::Calendar(slug) => {
                    let (cal, cap) = self.calendar_by_slug(creds, slug).await?;
                    capability_guard(cap, CalendarCapability::ReadOnly)?;
                    let rows = db::list_events_in_range(
                        &self.pool,
                        cal.id,
                        Utc::now() - chrono::Duration::days(366 * 2),
                        Utc::now() + chrono::Duration::days(366 * 5),
                    )
                    .await
                    .map_err(fs_err)?;
                    // ponytail: full-window scan per list; a calendar-query SQL
                    // push-down arrives when a profiled calendar needs it.
                    let mut entries = Vec::new();
                    for event in rows {
                        // Overrides are part of their master's resource.
                        if event.deleted_at.is_some() || event.master_event_id.is_some() {
                            continue;
                        }
                        let ics = self.series_ics(&event).await?;
                        entries.push(Entry {
                            name: event.resource_name().into_bytes(),
                            meta: Meta {
                                len: ics.len() as u64,
                                modified: event.updated_at.into(),
                                created: event.created_at.into(),
                                etag: event.etag.trim_matches('"').to_string(),
                                dir: false,
                                calendar: false,
                            },
                        });
                    }
                    entries
                }
                Location::Object(_, _) => return Err(FsError::NotFound),
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
                Location::Calendar(slug) => slug,
                Location::User => return Err(FsError::Exists),
                _ => return Err(FsError::Forbidden),
            };
            calendar_core::validate_slug(&slug).map_err(|_| FsError::Forbidden)?;
            let tenant_id = db::find_personal_tenant(&self.pool, creds.user.id)
                .await
                .map_err(fs_err)?;
            db::create_calendar(
                &self.pool,
                tenant_id,
                &db::NewCalendar {
                    slug: slug.clone(),
                    name: slug.clone(),
                    ..Default::default()
                },
                creds.user.id,
                &[(creds.user.id, CalendarCapability::Owner, true)],
            )
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
            match parse_location(path).ok_or(FsError::NotFound)? {
                Location::Calendar(slug) => {
                    let (cal, cap) = self.calendar_by_slug(creds, &slug).await?;
                    capability_guard(cap, CalendarCapability::Owner)?;
                    db::soft_delete_calendar(&self.pool, cal.id)
                        .await
                        .map_err(fs_err)?;
                    Ok(())
                }
                _ => Err(FsError::Forbidden),
            }
        })
    }

    fn remove_file<'a>(&'a self, path: &'a DavPath, creds: &'a DavAuth) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let (location, _) = self.resolve(creds, path).await?;
            match location {
                Location::Object(slug, name) => {
                    let (cal, cap) = self.calendar_by_slug(creds, &slug).await?;
                    capability_guard(cap, CalendarCapability::ReadWrite)?;
                    let (event, _) = db::get_event_by_href(&self.pool, cal.id, &name)
                        .await
                        .map_err(fs_err)?;
                    db::delete_event(&self.pool, event.id, None)
                        .await
                        .map_err(fs_err)?;
                    Ok(())
                }
                _ => Err(FsError::Forbidden),
            }
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
                Location::Calendar(slug) => slug,
                _ => return Err(FsError::Forbidden),
            };
            let (cal, cap) = self.calendar_by_slug(creds, &slug).await?;
            capability_guard(cap, CalendarCapability::ReadWrite)?;
            let mut changes = db::CalendarUpdate::default();
            for (set, prop) in &patch {
                if !*set {
                    continue;
                }
                let value = prop.xml.as_deref().and_then(xml_text);
                match prop.name.as_str() {
                    "displayname" => {
                        changes.name = Some(value.unwrap_or_default().to_string());
                    }
                    "calendar-description" => {
                        changes.description = value.map(|s| s.to_string());
                    }
                    _ => continue,
                }
            }
            db::update_calendar(&self.pool, cal.id, &changes)
                .await
                .map_err(fs_err)?;
            Ok(patch
                .into_iter()
                .filter(|(set, prop)| {
                    *set && matches!(prop.name.as_str(), "displayname" | "calendar-description")
                })
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
            let (location, _) = self.resolve(creds, path).await?;
            let Location::Calendar(slug) = location else {
                return Ok(vec![]);
            };
            let (cal, _) = self.calendar_by_slug(creds, &slug).await?;
            Ok(vec![
                dav_server::fs::DavProp::new(
                    "displayname".into(),
                    "D".into(),
                    "DAV:".into(),
                    xml_escape(&cal.name),
                ),
                dav_server::fs::DavProp::new(
                    "calendar-description".into(),
                    "C".into(),
                    "urn:ietf:params:xml:ns:caldav".into(),
                    xml_escape(cal.description.as_deref().unwrap_or("")),
                ),
            ])
        })
    }

    fn get_quota<'a>(&'a self, _creds: &'a DavAuth) -> FsFuture<'a, (u64, Option<u64>)> {
        Box::pin(std::future::ready(Ok((0, Some(u64::MAX / 2)))))
    }
}

/// File for GET/HEAD/REPORT reads: content is the serialized VCALENDAR.
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

/// File for PUT: buffers the body, commits on flush (one resource = one
/// VEVENT; parse errors fail the PUT).
#[derive(Debug)]
struct WriteFile {
    pool: PgPool,
    calendar: CalendarRow,
    user: db::UserRow,
    /// Filename the client is PUTting to.
    name: String,
    buffer: Vec<u8>,
    new_meta: Option<Meta>,
    /// Captured at open(): the etag the resource carried when dav-server's
    /// If-Match check passed, or create-only for If-None-Match: *.
    precondition: db::ics_upsert::PutPrecondition,
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
                    tracing::warn!("CalDAV PUT body is not UTF-8");
                    return Err(FsError::Forbidden);
                }
            };
            let events = match crate::parse_ics(text) {
                Ok(events) => events,
                Err(e) => {
                    tracing::warn!(error = %e, "CalDAV PUT body is not valid iCalendar");
                    return Err(FsError::Forbidden);
                }
            };
            // One resource = one UID: a master VEVENT plus its own overrides.
            // (An override without its master, e.g. an invitation to a single
            // occurrence, is not supported.)
            let (masters, overrides): (Vec<_>, Vec<_>) = events
                .iter()
                .partition(|e| e.recurrence_id.is_none() && e.recurrence_id_date.is_none());
            if masters.len() != 1 || events.iter().any(|e| e.uid != masters[0].uid) {
                tracing::warn!(
                    "CalDAV PUT resource must hold one master VEVENT and only its own overrides"
                );
                return Err(FsError::Forbidden);
            }
            // The ReadWrite capability was already enforced at open() time.
            let master = upsert_for(&self.user, masters[0]);
            let overrides: Vec<_> = overrides
                .iter()
                .map(|e| upsert_for(&self.user, e))
                .collect();
            let result = db::ics_upsert::put_series(
                &self.pool,
                self.calendar.id,
                self.user.id,
                &self.name,
                &master,
                &overrides,
                &self.precondition,
            )
            .await;
            let (event, _) = result.map_err(|e| {
                tracing::warn!(error = %e, "CalDAV PUT failed to store event");
                match e {
                    db::DbError::NotFound => FsError::NotFound,
                    // A failed etag/create-only precondition aborts the write
                    // transaction. FsError has no 412-mapping variant (dav-server's
                    // FsError -> status table lacks PreconditionFailed), so 403
                    // Forbidden is the closest existing error: the client sees the
                    // write refused and must refetch and retry.
                    db::DbError::Conflict(_) => FsError::Forbidden,
                    _ => FsError::GeneralFailure,
                }
            })?;
            // New scheduled events fan out invitations.
            db::scheduling::schedule_requests(&self.pool, event.id).await;
            self.new_meta = Some(Meta {
                len: 0,
                modified: event.updated_at.into(),
                created: event.created_at.into(),
                etag: event.etag.trim_matches('"').to_string(),
                dir: false,
                calendar: false,
            });
            Ok(())
        })
    }
}

/// Storage record for one parsed VEVENT written by `user`. organizer_email is
/// NOT NULL: an organizer-less VEVENT PUTs as owned by the writer.
fn upsert_for(user: &db::UserRow, parsed: &crate::ParsedEvent) -> db::ics_upsert::IcsEventUpsert {
    let mut data = crate::upsert_data(parsed);
    if data.organizer_email.is_empty() {
        data.organizer_email = user.email.clone();
    }
    data.organizer_user_id = data
        .organizer_email
        .eq_ignore_ascii_case(&user.email)
        .then_some(user.id);
    data
}

/// Text content of a PROPPATCH property element:
/// `<D:displayname xmlns:D="DAV:">Work &amp; Co</D:displayname>` -> `Work & Co`.
fn xml_text(value: &[u8]) -> Option<String> {
    let mut text = std::str::from_utf8(value).ok()?.trim();
    if text.starts_with("<?") {
        text = text[text.find("?>")? + 2..].trim(); // dav-server prefixes an XML declaration
    }
    let inner = if text.starts_with('<') {
        text[text.find('>')? + 1..text.rfind("</")?].trim()
    } else {
        text
    };
    Some(
        inner
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&"),
    )
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn property_text_is_unwrapped_and_unescaped() {
        let el = br#"<D:displayname xmlns:D="DAV:">Work &amp; Co</D:displayname>"#;
        assert_eq!(xml_text(el).as_deref(), Some("Work & Co"));
        let declared = br#"<?xml version="1.0" encoding="utf-8"?><D:displayname xmlns:D="DAV:">Work</D:displayname>"#;
        assert_eq!(xml_text(declared).as_deref(), Some("Work"));
        assert_eq!(xml_text(b"plain").as_deref(), Some("plain"));
        assert_eq!(xml_text(b"<D:displayname/>"), None);
    }
}
