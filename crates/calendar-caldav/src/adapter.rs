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
//!   /calendars/{user}/{slug}/{name}.ics — one resource: a VEVENT series, a
//!                                         VTODO series or a VJOURNAL,
//!                                         addressed by its stored href
//!                                         (ADR-015 D4)

use crate::{
    ExportRow, ParsedResource, TaskExportRow, events_to_ics, journal_to_ics, parse_resource,
    todos_to_ics,
};
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
use uuid::Uuid;

/// Request credentials: the authenticated user (app password / API token /
/// session) — authorization is enforced here at the resource boundary.
/// A share principal (`share_calendar_id`) is a public-share token: read-only
/// DAV on exactly that calendar, with PRIVATE/CONFIDENTIAL events hidden.
#[derive(Debug, Clone)]
pub struct DavAuth {
    pub user: db::UserRow,
    pub share_calendar_id: Option<Uuid>,
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

/// Stored component kinds (ADR-015 D5): one resource per kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ObjectKind {
    Event,
    Todo,
    Journal,
}

fn kind_of(kind: &str) -> ObjectKind {
    match kind {
        "VTODO" => ObjectKind::Todo,
        "VJOURNAL" => ObjectKind::Journal,
        _ => ObjectKind::Event,
    }
}

/// calendar_objects row as read from SQL.
#[derive(Debug, sqlx::FromRow)]
struct ObjectRow {
    kind: String,
    id: Uuid,
    etag: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    deleted_at: Option<DateTime<Utc>>,
    class: Option<String>,
}

/// One row of the calendar_objects view.
#[derive(Debug, Clone)]
pub(crate) struct ObjectRef {
    pub kind: ObjectKind,
    pub id: Uuid,
    pub etag: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
    pub class: Option<String>,
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
    /// A share principal resolves only its shared calendar, read-only.
    async fn calendar_by_slug(
        &self,
        creds: &DavAuth,
        slug: &str,
    ) -> FsResult<(CalendarRow, CalendarCapability)> {
        if let Some(calendar_id) = creds.share_calendar_id {
            let cal = db::get_calendar(&self.pool, calendar_id)
                .await
                .map_err(fs_err)?;
            if cal.slug != slug {
                return Err(FsError::NotFound);
            }
            return Ok((cal, CalendarCapability::ReadOnly));
        }
        db::list_calendars_for_user(&self.pool, creds.user.id)
            .await
            .map_err(fs_err)?
            .into_iter()
            .find(|(cal, _)| cal.slug == slug)
            .ok_or(FsError::NotFound)
    }

    /// The object (any kind) served under `name`, via the calendar_objects
    /// view (ADR-015 D5). `class` drives share-principal filtering for all
    /// three kinds alike.
    async fn object_at(&self, calendar_id: Uuid, name: &str) -> FsResult<Option<ObjectRef>> {
        let row: Option<ObjectRow> = sqlx::query_as::<_, ObjectRow>(
            "SELECT o.kind, o.id, o.etag, o.created_at, o.updated_at, o.deleted_at,
                    COALESCE(e.class, t.class, j.class) AS class
             FROM calendar_objects o
             LEFT JOIN events e ON e.id = o.id
             LEFT JOIN tasks t ON t.id = o.id
             LEFT JOIN journals j ON j.id = o.id
             WHERE o.calendar_id = $1 AND o.href = $2",
        )
        .bind(calendar_id)
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| FsError::GeneralFailure)?;
        Ok(row.map(|row| ObjectRef {
            kind: kind_of(&row.kind),
            id: row.id,
            etag: row.etag,
            created_at: row.created_at,
            updated_at: row.updated_at,
            deleted_at: row.deleted_at,
            class: row.class,
        }))
    }

    /// The whole series or single component as one VCALENDAR, with the
    /// calendar's stored VTIMEZONE definitions (ADR-012) where the kind uses
    /// them.
    async fn render_object(&self, object: &ObjectRef) -> FsResult<String> {
        match object.kind {
            ObjectKind::Event => {
                let (event, _) = db::get_event(&self.pool, object.id).await.map_err(fs_err)?;
                self.series_ics(&event).await
            }
            ObjectKind::Todo => {
                let (task, _) = db::tasks::get_task(&self.pool, object.id)
                    .await
                    .map_err(fs_err)?;
                self.task_ics(&task).await
            }
            ObjectKind::Journal => {
                let (journal, _) = db::journals::get_journal(&self.pool, object.id)
                    .await
                    .map_err(fs_err)?;
                Ok(journal_to_ics(&journal))
            }
        }
    }

    /// The whole task series as one VCALENDAR: the master, then its
    /// RECURRENCE-ID overrides, with attendees and alarms.
    async fn task_ics(&self, master: &db::tasks::TaskRow) -> FsResult<String> {
        let mut tasks = vec![master.clone()];
        tasks.extend(
            db::tasks::list_overrides(&self.pool, master.id)
                .await
                .map_err(fs_err)?,
        );
        let zones = db::timezones::list_for_calendar(&self.pool, master.calendar_id)
            .await
            .unwrap_or_default();
        let mut rows = Vec::with_capacity(tasks.len());
        for task in tasks {
            rows.push(TaskExportRow {
                attendees: db::tasks::list_task_attendees(&self.pool, task.id)
                    .await
                    .unwrap_or_default(),
                alarms: db::tasks::list_task_alarms(&self.pool, task.id)
                    .await
                    .unwrap_or_default(),
                vtimezones: zones.clone(),
                task,
            });
        }
        Ok(todos_to_ics(&rows))
    }

    /// The whole event series as one VCALENDAR: the master, then its overrides,
    /// plus the calendar's stored VTIMEZONE definitions (ADR-012).
    async fn series_ics(&self, master: &EventRow) -> FsResult<String> {
        let mut events = vec![master.clone()];
        events.extend(
            db::list_exceptions(&self.pool, &[master.id])
                .await
                .map_err(fs_err)?,
        );
        let zones = db::timezones::list_for_calendar(&self.pool, master.calendar_id)
            .await
            .unwrap_or_default();
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
                vtimezones: zones.clone(),
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
                let Some(object) = self.object_at(cal.id, name).await? else {
                    return Err(FsError::NotFound);
                };
                if creds.share_calendar_id.is_some() && object.class.as_deref() != Some("PUBLIC") {
                    return Err(FsError::NotFound);
                }
                let ics = self.render_object(&object).await?;
                Ok((
                    location,
                    Meta {
                        len: ics.len() as u64,
                        modified: object.updated_at.into(),
                        created: object.created_at.into(),
                        etag: object.etag.trim_matches('"').to_string(),
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
                        let Some(object) = self.object_at(cal.id, &name).await? else {
                            return Err(FsError::NotFound);
                        };
                        if creds.share_calendar_id.is_some()
                            && object.class.as_deref() != Some("PUBLIC")
                        {
                            return Err(FsError::NotFound);
                        }
                        let ics = self.render_object(&object).await?;
                        let modified: SystemTime = object.updated_at.into();
                        let meta = Meta {
                            len: ics.len() as u64,
                            modified,
                            created: object.created_at.into(),
                            etag: object.etag.trim_matches('"').to_string(),
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
                        let existing = self.object_at(cal.id, &name).await;
                        let exists = match &existing {
                            Ok(Some(object)) if object.deleted_at.is_none() => true,
                            Ok(_) => false,
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
                                Ok(Some(object)) => db::ics_upsert::PutPrecondition::MatchEtag(
                                    object.etag.trim_matches('"').to_string(),
                                ),
                                Ok(None) => db::ics_upsert::PutPrecondition::None,
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
                    // so the home-set listing shows calendar collections. A
                    // share principal sees exactly its shared calendar.
                    let calendars: Vec<(CalendarRow, CalendarCapability)> =
                        if let Some(calendar_id) = creds.share_calendar_id {
                            db::get_calendar(&self.pool, calendar_id)
                                .await
                                .map(|cal| vec![(cal, CalendarCapability::ReadOnly)])
                                .map_err(fs_err)?
                        } else {
                            db::list_calendars_for_user(&self.pool, creds.user.id)
                                .await
                                .map_err(fs_err)?
                        };
                    calendars
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
                        // A share principal sees PUBLIC events only.
                        if creds.share_calendar_id.is_some()
                            && event.class.as_deref() != Some("PUBLIC")
                        {
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
                    // Tasks and journals are listed in full (no window):
                    // undated items have no time to window on (ADR-015 D5).
                    let share = creds.share_calendar_id.is_some();
                    for task in
                        db::tasks::list_tasks(&self.pool, cal.id, &db::tasks::TaskFilter::default())
                            .await
                            .map_err(fs_err)?
                    {
                        if share && task.class.as_deref() != Some("PUBLIC") {
                            continue;
                        }
                        let ics = self.task_ics(&task).await?;
                        entries.push(Entry {
                            name: task.resource_name().into_bytes(),
                            meta: Meta {
                                len: ics.len() as u64,
                                modified: task.updated_at.into(),
                                created: task.created_at.into(),
                                etag: task.etag.trim_matches('"').to_string(),
                                dir: false,
                                calendar: false,
                            },
                        });
                    }
                    for journal in db::journals::list_journals(
                        &self.pool,
                        cal.id,
                        &db::journals::JournalFilter::default(),
                    )
                    .await
                    .map_err(fs_err)?
                    {
                        if share && journal.class.as_deref() != Some("PUBLIC") {
                            continue;
                        }
                        let ics = journal_to_ics(&journal);
                        entries.push(Entry {
                            name: journal.resource_name().into_bytes(),
                            meta: Meta {
                                len: ics.len() as u64,
                                modified: journal.updated_at.into(),
                                created: journal.created_at.into(),
                                etag: journal.etag.trim_matches('"').to_string(),
                                dir: false,
                                calendar: false,
                            },
                        });
                    }
                    // ponytail: full-window scan per list; a calendar-query SQL
                    // push-down arrives when a profiled calendar needs it.
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
            // A share principal cannot create calendars in the owner's tenant.
            if creds.share_calendar_id.is_some() {
                return Err(FsError::Forbidden);
            }
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
            if creds.share_calendar_id.is_some() {
                return Err(FsError::Forbidden);
            }
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
                    let Some(object) = self.object_at(cal.id, &name).await? else {
                        return Err(FsError::NotFound);
                    };
                    match object.kind {
                        ObjectKind::Event => {
                            // Deleting a delivered copy is a decline; deleting
                            // the organizer's row cancels for everyone.
                            if db::scheduling::origin_of(
                                &self.pool,
                                db::scheduling::SubjectKind::Event,
                                object.id,
                            )
                            .await
                            .unwrap_or(None)
                            .is_some()
                            {
                                db::scheduling::decline_copy(
                                    &self.pool,
                                    db::scheduling::SubjectKind::Event,
                                    object.id,
                                    creds.user.id,
                                )
                                .await
                                .map_err(fs_err)?;
                            } else {
                                db::delete_event(&self.pool, object.id, None)
                                    .await
                                    .map_err(fs_err)?;
                                db::scheduling::dispatch_cancel(
                                    &self.pool,
                                    db::scheduling::SubjectKind::Event,
                                    object.id,
                                    creds.user.id,
                                )
                                .await
                                .ok();
                            }
                            // Webhook trigger, same as the JSON API path.
                            if let Ok(webhooks) = db::webhooks::matching_webhooks(
                                &self.pool,
                                cal.tenant_id,
                                "event_deleted",
                            )
                            .await
                            {
                                for webhook in webhooks {
                                    db::webhooks::enqueue_delivery(
                                        &self.pool,
                                        webhook.id,
                                        object.id,
                                        "event_deleted",
                                    )
                                    .await
                                    .ok();
                                }
                            }
                        }
                        // Task deletes cascade to overrides and the subtask
                        // tree (one change_log row each); journal deletes are
                        // flat. A delivered copy delete is a decline; the
                        // organizer's delete cancels for everyone.
                        ObjectKind::Todo => {
                            if db::scheduling::origin_of(
                                &self.pool,
                                db::scheduling::SubjectKind::Task,
                                object.id,
                            )
                            .await
                            .unwrap_or(None)
                            .is_some()
                            {
                                db::scheduling::decline_copy(
                                    &self.pool,
                                    db::scheduling::SubjectKind::Task,
                                    object.id,
                                    creds.user.id,
                                )
                                .await
                                .map_err(fs_err)?;
                            } else {
                                db::tasks::delete_task(&self.pool, object.id, None)
                                    .await
                                    .map_err(fs_err)?;
                                db::scheduling::dispatch_cancel(
                                    &self.pool,
                                    db::scheduling::SubjectKind::Task,
                                    object.id,
                                    creds.user.id,
                                )
                                .await
                                .ok();
                            }
                        }
                        ObjectKind::Journal => {
                            db::journals::delete_journal(&self.pool, object.id, None)
                                .await
                                .map_err(fs_err)?;
                        }
                    }
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
            // Per-prop statuses: a set on an unsupported property is a 409
            // (RFC 4918 §9.9.1), never a silent drop.
            Ok(patch
                .into_iter()
                .map(|(set, prop)| {
                    let handled =
                        set && matches!(prop.name.as_str(), "displayname" | "calendar-description");
                    let status = if handled {
                        http::StatusCode::OK
                    } else {
                        http::StatusCode::CONFLICT
                    };
                    (status, prop)
                })
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
            let parsed = match parse_resource(text) {
                Ok(parsed) => parsed,
                Err(e) => {
                    tracing::warn!(error = %e, "CalDAV PUT body is not valid iCalendar");
                    return Err(FsError::Forbidden);
                }
            };
            match parsed {
                ParsedResource::Events(events) => self.flush_events(events).await,
                ParsedResource::Todos(series) => self.flush_todo(*series).await,
                ParsedResource::Journal(journal) => self.flush_journal(*journal).await,
            }
        })
    }
}

impl WriteFile {
    /// VEVENT PUT: one master plus its own overrides (parse_resource already
    /// enforced one UID; an override-only resource was refused at parse).
    async fn flush_events(
        &mut self,
        parsed: crate::ParsedCalendar,
    ) -> std::result::Result<(), FsError> {
        let events = parsed.events;
        // Client-supplied VTIMEZONEs ride along to the store (ADR-012); a
        // non-compilable one already failed parse_calendar above.
        let zones: Vec<db::timezones::NewTimezone> = parsed
            .timezones
            .iter()
            .map(|tz| db::timezones::NewTimezone {
                tzid: tz.tzid.clone(),
                definition: tz.definition.clone(),
                rules: tz.rules.clone(),
            })
            .collect();
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
        // A PUT to a delivered copy applies only the attendee's own PARTSTAT;
        // everything else is discarded (the organizer's next dispatch
        // rebuilds the copy).
        if let Some((existing, _)) = db::get_event_by_href(&self.pool, self.calendar.id, &self.name)
            .await
            .ok()
            && let Some(origin_id) = db::scheduling::origin_of(
                &self.pool,
                db::scheduling::SubjectKind::Event,
                existing.id,
            )
            .await
            .unwrap_or(None)
        {
            if let Some(email) = db::scheduling::attendee_email_for_user(
                &self.pool,
                db::scheduling::SubjectKind::Event,
                existing.id,
                self.user.id,
            )
            .await
            .unwrap_or(None)
                && let Some(partstat) = masters[0]
                    .attendees
                    .iter()
                    .find(|a| {
                        a.email
                            .as_deref()
                            .is_some_and(|e| e.eq_ignore_ascii_case(&self.user.email))
                    })
                    .and_then(|a| a.partstat.clone())
            {
                db::scheduling::reply(
                    &self.pool,
                    db::scheduling::SubjectKind::Event,
                    origin_id,
                    &email,
                    &partstat,
                )
                .await
                .map_err(fs_err)?;
            }
            // The new etag makes the client refetch; the write is done.
            let (row, _) = db::get_event(&self.pool, existing.id)
                .await
                .map_err(fs_err)?;
            self.new_meta = Some(Meta {
                len: 0,
                modified: row.updated_at.into(),
                created: row.created_at.into(),
                etag: row.etag.trim_matches('"').to_string(),
                dir: false,
                calendar: false,
            });
            return Ok(());
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
            &zones,
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
        // Scheduling dispatch (stage 8a): copies for internal attendees,
        // REQUEST intents for external ones.
        db::scheduling::dispatch(
            &self.pool,
            db::scheduling::SubjectKind::Event,
            event.id,
            self.user.id,
        )
        .await
        .ok();
        // Webhook triggers, same as the JSON API path.
        let trigger = if matches!(
            self.precondition,
            db::ics_upsert::PutPrecondition::MatchEtag(_)
        ) {
            "event_updated"
        } else {
            "event_created"
        };
        if let Ok(webhooks) =
            db::webhooks::matching_webhooks(&self.pool, self.calendar.tenant_id, trigger).await
        {
            for webhook in webhooks {
                db::webhooks::enqueue_delivery(&self.pool, webhook.id, event.id, trigger)
                    .await
                    .ok();
            }
        }
        self.new_meta = Some(Meta {
            len: 0,
            modified: event.updated_at.into(),
            created: event.created_at.into(),
            etag: event.etag.trim_matches('"').to_string(),
            dir: false,
            calendar: false,
        });
        Ok(())
    }

    /// VTODO PUT (ADR-015 D3/D8): the master is created or fully replaced with
    /// the same If-Match precondition re-verification pattern the event
    /// put_series uses; overrides that already exist are updated, completed
    /// ones are written through the completion path (D8), and overrides
    /// absent from the PUT are removed.
    async fn flush_todo(
        &mut self,
        series: crate::ParsedTodoSeries,
    ) -> std::result::Result<(), FsError> {
        // A PUT to a delivered copy applies only the assignee's own PARTSTAT;
        // everything else is discarded (the organizer's next dispatch
        // rebuilds the copy).
        if let Some((existing, _)) =
            db::tasks::get_task_by_href(&self.pool, self.calendar.id, &self.name)
                .await
                .ok()
            && let Some(origin_id) = db::scheduling::origin_of(
                &self.pool,
                db::scheduling::SubjectKind::Task,
                existing.id,
            )
            .await
            .unwrap_or(None)
        {
            if let Some(email) = db::scheduling::attendee_email_for_user(
                &self.pool,
                db::scheduling::SubjectKind::Task,
                existing.id,
                self.user.id,
            )
            .await
            .unwrap_or(None)
                && let Some(partstat) = series
                    .master
                    .attendees
                    .iter()
                    .find(|a| {
                        a.email
                            .as_deref()
                            .is_some_and(|e| e.eq_ignore_ascii_case(&self.user.email))
                    })
                    .and_then(|a| a.partstat.clone())
            {
                db::scheduling::reply(
                    &self.pool,
                    db::scheduling::SubjectKind::Task,
                    origin_id,
                    &email,
                    &partstat,
                )
                .await
                .map_err(fs_err)?;
            }
            // The new etag makes the client refetch; the write is done.
            let (row, _) = db::tasks::get_task(&self.pool, existing.id)
                .await
                .map_err(fs_err)?;
            self.new_meta = Some(Meta {
                len: 0,
                modified: row.updated_at.into(),
                created: row.created_at.into(),
                etag: row.etag.trim_matches('"').to_string(),
                dir: false,
                calendar: false,
            });
            return Ok(());
        }
        // Client-supplied VTIMEZONEs ride along to the calendar's zone table
        // (ADR-012) so custom-zone tasks expand like events do.
        if !series.timezones.is_empty() {
            let zones: Vec<db::timezones::NewTimezone> = series
                .timezones
                .iter()
                .map(|tz| db::timezones::NewTimezone {
                    tzid: tz.tzid.clone(),
                    definition: tz.definition.clone(),
                    rules: tz.rules.clone(),
                })
                .collect();
            let mut tx = self
                .pool
                .begin()
                .await
                .map_err(|_| FsError::GeneralFailure)?;
            db::timezones::upsert_for_calendar(&mut tx, self.calendar.id, &zones)
                .await
                .map_err(|_| FsError::GeneralFailure)?;
            tx.commit().await.map_err(|_| FsError::GeneralFailure)?;
        }
        // ADR-012 reject-at-write for the task's own tzid: it must be a tzdb
        // zone or resolvable from this PUT's zones or the calendar's stored
        // ones — no silent UTC fallback.
        let stored_zones: Vec<(String,)> =
            sqlx::query_as("SELECT tzid FROM timezones WHERE calendar_id = $1")
                .bind(self.calendar.id)
                .fetch_all(&self.pool)
                .await
                .map_err(|_| FsError::GeneralFailure)?;
        let known: std::collections::HashSet<String> = series
            .timezones
            .iter()
            .map(|z| z.tzid.clone())
            .chain(stored_zones.into_iter().map(|(s,)| s))
            .collect();
        for todo in std::iter::once(&series.master).chain(&series.overrides) {
            if let Some(tzid) = &todo.tzid
                && !calendar_core::recurrence::is_tzdb_tzid(tzid)
                && !known.contains(tzid)
            {
                tracing::warn!(tzid = %tzid, "CalDAV PUT task references an unknown timezone");
                return Err(FsError::Forbidden);
            }
        }
        let mut data = crate::todo::new_task_data(&series.master).map_err(|e| {
            tracing::warn!(error = %e, "CalDAV PUT task rejected");
            FsError::Forbidden
        })?;
        // The resource is addressed by the client-chosen filename (D4).
        data.href = Some(self.name.clone());
        let patch = crate::todo::task_patch(&series.master).map_err(|e| {
            tracing::warn!(error = %e, "CalDAV PUT task rejected");
            FsError::Forbidden
        })?;
        let attendees: Vec<db::NewAttendee> = series
            .master
            .attendees
            .iter()
            .map(crate::todo::attendee)
            .collect();
        let alarms: Vec<db::alarms::NewAlarm> = series
            .master
            .alarms
            .iter()
            .map(crate::todo::alarm_data)
            .collect();
        // Existing/new is decided by href lookup, not by URL uuid; the etag
        // captured at open() is re-verified inside the write transaction.
        let existing = db::tasks::get_task_by_href(&self.pool, self.calendar.id, &self.name)
            .await
            .ok();
        let master_id = match (existing, &self.precondition) {
            (Some((task, etag)), precondition) => {
                if matches!(precondition, db::ics_upsert::PutPrecondition::NotExists)
                    || !precondition_matches(precondition, &etag)
                {
                    // A failed etag/create-only precondition aborts the write;
                    // 403 is the closest FsError (see the event path note).
                    return Err(FsError::Forbidden);
                }
                // A PUT replaces the resource. ponytail: TaskPatch cannot
                // carry rrule/rdate/exdate/extra_props/COMPLETED, so those
                // keep their stored values on update (and optional properties
                // absent from the PUT are not cleared); a db-level
                // put_task_series replaces this call when the data layer
                // grows one.
                db::tasks::update_task(&self.pool, task.id, Some(etag.trim_matches('"')), &patch)
                    .await
                    .map_err(put_err)?;
                task.id
            }
            (None, _) => {
                let (task, _) = db::tasks::create_task(
                    &self.pool,
                    self.calendar.id,
                    self.user.id,
                    &attendees,
                    &alarms,
                    &data,
                )
                .await
                .map_err(put_err)?;
                task.id
            }
        };
        self.apply_task_overrides(master_id, &series.overrides)
            .await?;
        // Override writes bump the master's etag; the resource validator is
        // the master's final one.
        let (task, _) = db::tasks::get_task(&self.pool, master_id)
            .await
            .map_err(fs_err)?;
        // Scheduling dispatch (stage 8c): copies for internal assignees,
        // REQUEST intents for external ones.
        db::scheduling::dispatch(
            &self.pool,
            db::scheduling::SubjectKind::Task,
            master_id,
            self.user.id,
        )
        .await
        .ok();
        self.new_meta = Some(Meta {
            len: 0,
            modified: task.updated_at.into(),
            created: task.created_at.into(),
            etag: task.etag.trim_matches('"').to_string(),
            dir: false,
            calendar: false,
        });
        Ok(())
    }

    /// Syncs the PUT's override set (D3: the resource is the series).
    async fn apply_task_overrides(
        &self,
        master_id: Uuid,
        overrides: &[crate::ParsedTodo],
    ) -> std::result::Result<(), FsError> {
        let existing = db::tasks::list_overrides(&self.pool, master_id)
            .await
            .map_err(fs_err)?;
        for parsed in overrides {
            let patch = crate::todo::task_patch(parsed).map_err(|_| FsError::Forbidden)?;
            match existing.iter().find(|o| {
                o.recurrence_id == parsed.recurrence_id
                    && o.recurrence_id_date == parsed.recurrence_id_date
            }) {
                Some(row) => {
                    db::tasks::update_task(&self.pool, row.id, None, &patch)
                        .await
                        .map_err(put_err)?;
                }
                None if parsed.status.as_deref() == Some("COMPLETED") => {
                    // D8 recurring completion on the wire: write through the
                    // completion path, which creates the override.
                    let occurrence = match (parsed.recurrence_id, parsed.recurrence_id_date) {
                        (Some(at), _) => db::tasks::Occurrence::Timed(at),
                        (None, Some(date)) => db::tasks::Occurrence::AllDay(date),
                        (None, None) => continue,
                    };
                    db::tasks::complete_task(&self.pool, master_id, Some(occurrence), self.user.id)
                        .await
                        .map_err(put_err)?;
                }
                None => {
                    // ponytail: NewTaskData has no master_task_id, so the db
                    // layer cannot insert arbitrary overrides yet; non-completed
                    // ones are dropped until it can.
                    tracing::warn!(
                        "CalDAV PUT task override could not be stored (no existing override row)"
                    );
                }
            }
        }
        // Overrides removed from the PUT disappear (resource-is-a-series).
        for row in existing.iter().filter(|o| {
            !overrides.iter().any(|p| {
                p.recurrence_id == o.recurrence_id && p.recurrence_id_date == o.recurrence_id_date
            })
        }) {
            db::tasks::delete_task(&self.pool, row.id, None)
                .await
                .map_err(put_err)?;
        }
        Ok(())
    }

    /// VJOURNAL PUT (D9): one component per resource, no overrides.
    async fn flush_journal(
        &mut self,
        parsed: crate::ParsedJournal,
    ) -> std::result::Result<(), FsError> {
        let mut data = crate::journal::new_journal_data(&parsed).map_err(|e| {
            tracing::warn!(error = %e, "CalDAV PUT journal rejected");
            FsError::Forbidden
        })?;
        let patch = crate::journal::journal_patch(&parsed);
        // The resource is addressed by the client-chosen filename (D4).
        data.href = Some(self.name.clone());
        let (journal, _) =
            match db::journals::get_journal_by_href(&self.pool, self.calendar.id, &self.name).await
            {
                Ok((journal, etag)) => {
                    if !precondition_matches(&self.precondition, &etag) {
                        return Err(FsError::Forbidden);
                    }
                    db::journals::update_journal(
                        &self.pool,
                        journal.id,
                        Some(etag.trim_matches('"')),
                        &patch,
                    )
                    .await
                    .map_err(put_err)?
                }
                Err(db::DbError::NotFound) => {
                    db::journals::create_journal(&self.pool, self.calendar.id, self.user.id, &data)
                        .await
                        .map_err(put_err)?
                }
                Err(_) => return Err(FsError::GeneralFailure),
            };
        self.new_meta = Some(Meta {
            len: 0,
            modified: journal.updated_at.into(),
            created: journal.created_at.into(),
            etag: journal.etag.trim_matches('"').to_string(),
            dir: false,
            calendar: false,
        });
        Ok(())
    }
}

/// True when the captured PUT precondition still holds for `etag` (None and
/// MatchEtag both pass; NotExists was rejected before this check).
fn precondition_matches(precondition: &db::ics_upsert::PutPrecondition, etag: &str) -> bool {
    match precondition {
        db::ics_upsert::PutPrecondition::MatchEtag(expected) => {
            constant_time_eq(expected.trim_matches('"'), etag.trim_matches('"'))
        }
        _ => true,
    }
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

fn put_err(e: db::DbError) -> FsError {
    tracing::warn!(error = %e, "CalDAV PUT failed to store object");
    match e {
        db::DbError::NotFound => FsError::NotFound,
        db::DbError::Conflict(_) => FsError::Forbidden,
        _ => FsError::GeneralFailure,
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
